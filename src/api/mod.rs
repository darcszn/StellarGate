use crate::{db, AppState};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json,
};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

mod payments;

/// Reject request bodies larger than this (256 KiB) before they hit a handler.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The authenticated merchant ID injected by the auth middleware.
#[derive(Clone)]
pub struct AuthenticatedMerchant(pub String);

#[derive(Clone)]
struct RateLimitState {
    requests_per_sec: u32,
    limiters: Arc<Mutex<HashMap<String, governor::DefaultDirectRateLimiter>>>,
}

impl RateLimitState {
    fn new(requests_per_sec: u32) -> Self {
        Self {
            requests_per_sec: requests_per_sec.max(1),
            limiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = build_cors(&state.config);
    let rate_limit = RateLimitState::new(state.config.rate_limit_requests_per_sec);

    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/ready", get(ready))
        /* Merchant provisioning — returns a one-time plaintext API key. Gated
        behind ADMIN_PROVISIONING_SECRET so it can't be used to mint
        unlimited credentials anonymously. */
        .route(
            "/merchants",
            post(provision_merchant).route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_secret,
            )),
        )
        .nest("/payments", {
            /* Auth middleware only on the write + list routes; the per-payment
            status and webhook endpoints stay public (anyone with the id can
            poll or inspect). */
            let authed = axum::Router::new()
                .route("/", post(payments::create).get(payments::list))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ));

            axum::Router::new()
                .merge(authed)
                .route("/:id", get(payments::get_by_id))
                .route("/:id/webhooks", get(payments::list_webhooks))
                .route(
                    "/:id/webhooks/:delivery_id/redeliver",
                    post(payments::redeliver_webhook),
                )
        })
        .fallback(not_found)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            rate_limit,
            rate_limit_middleware,
        ))
        .layer(cors)
        .with_state(state)
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> axum::response::Response {
    let raw_key = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);

    let Some(key) = raw_key else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid Authorization header", "code": "unauthorized" })),
        )
            .into_response();
    };

    match db::find_merchant_by_key(&state.pool, &key).await {
        Ok(Some(merchant_id)) => {
            req.extensions_mut()
                .insert(AuthenticatedMerchant(merchant_id));
            next.run(req).await
        }
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid API key", "code": "unauthorized" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal server error", "code": "internal_error" })),
        )
            .into_response(),
    }
}

/// Guards `POST /merchants` with a shared admin secret sent via the
/// `X-Admin-Secret` header. An unset `ADMIN_PROVISIONING_SECRET` disables
/// provisioning entirely rather than leaving the endpoint open.
async fn require_admin_secret(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let configured = &state.config.admin_provisioning_secret;
    let provided = req
        .headers()
        .get("x-admin-secret")
        .and_then(|v| v.to_str().ok());

    if configured.is_empty() || provided != Some(configured.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid admin secret", "code": "unauthorized" })),
        )
            .into_response();
    }

    next.run(req).await
}

/// `POST /merchants` — provision a new merchant and return its API key once.
/// Requires the `X-Admin-Secret` header (see `require_admin_secret`).
async fn provision_merchant(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let merchant_id = uuid::Uuid::new_v4().to_string();
    let raw_key = uuid::Uuid::new_v4().to_string();

    db::create_merchant(&state.pool, &merchant_id, &raw_key)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error", "code": "internal_error" })),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "merchant_id": merchant_id,
            "api_key": raw_key,
        })),
    ))
}

async fn rate_limit_middleware(
    State(rate_limit): State<RateLimitState>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    if req.method() == axum::http::Method::POST
        && matches!(req.uri().path(), "/payments" | "/merchants")
    {
        let key = rate_limit_key(&req);
        let limited = {
            let mut map = rate_limit.limiters.lock().unwrap();
            let limiter = map.entry(key).or_insert_with(|| {
                governor::RateLimiter::direct(governor::Quota::per_second(
                    NonZeroU32::new(rate_limit.requests_per_sec).unwrap(),
                ))
            });
            limiter.check().is_err()
        };

        if limited {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
                Json(json!({
                    "error": "rate limit exceeded",
                    "code": "rate_limit_exceeded"
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

/// Keyed by path + client so `/merchants` and `/payments` are rate-limited
/// independently — provisioning a merchant should never eat into a client's
/// payment quota (or vice versa).
fn rate_limit_key(req: &Request) -> String {
    format!("{}:{}", req.uri().path(), client_ip_key(req))
}

fn client_ip_key(req: &Request) -> String {
    if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.ip().to_string();
    }

    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
            if let Some(first) = value.split(',').map(str::trim).find(|s| !s.is_empty()) {
                return first.to_string();
            }
        }
    }

    "local".to_string()
}

fn build_cors(cfg: &crate::config::Config) -> CorsLayer {
    use axum::http::HeaderName;
    use tower_http::cors::AllowOrigin;

    let origins = &cfg.cors_allowed_origins;

    if origins.is_empty() {
        if cfg.network == "public" {
            tracing::warn!(
                "CORS_ALLOWED_ORIGINS is not set on a public-network deployment. \
                 All origins are allowed — set CORS_ALLOWED_ORIGINS in production."
            );
        }
        return CorsLayer::permissive();
    }

    let allow_origins: Vec<axum::http::HeaderValue> =
        origins.iter().filter_map(|o| o.parse().ok()).collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allow_origins))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("authorization"),
        ])
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn ready(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match db::ping(&state.pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unavailable" })),
        )
            .into_response(),
    }
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "code": "not_found" })),
    )
}
