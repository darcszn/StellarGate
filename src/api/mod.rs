use crate::{db, AppState};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Extension, Json,
};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
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

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = build_cors(&state.config);
    let rate_limit_rps = state.config.rate_limit_requests_per_sec;

    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/ready", get(ready))
        // Merchant provisioning — returns a one-time plaintext API key.
        .route("/merchants", post(provision_merchant))
        .nest("/payments", {
            // Auth middleware only on the write + list routes; the per-payment
            // status and webhook endpoints stay public (anyone with the id can
            // poll or inspect).
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
                .layer(middleware::from_fn(
                    move |ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next| {
                        rate_limit_middleware(addr, rate_limit_rps, req, next)
                    },
                ))
        })
        .fallback(not_found)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
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

/// `POST /merchants` — provision a new merchant and return its API key once.
/// In production this endpoint should be protected (e.g., by an admin secret
/// or removed entirely in favour of an offline provisioning script).
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
    addr: SocketAddr,
    rate_limit_rps: u32,
    req: Request,
    next: Next,
) -> axum::response::Response {
    static LIMITERS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, governor::DefaultDirectRateLimiter>>,
    > = std::sync::OnceLock::new();

    let limiters = LIMITERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let ip = addr.ip().to_string();

    // Scoped so the `MutexGuard` is dropped before the `next.run().await`
    // below, keeping the returned future `Send`.
    let allowed = {
        let mut map = limiters.lock().unwrap();
        let limiter = map.entry(ip).or_insert_with(|| {
            governor::RateLimiter::direct(governor::Quota::per_second(
                std::num::NonZeroU32::new(rate_limit_rps).unwrap(),
            ))
        });
        limiter.check().is_ok()
    };

    if !allowed {
        let retry_after = (1000 / rate_limit_rps).max(1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_str(&retry_after.to_string()).unwrap(),
            )],
            axum::Json(json!({
                "error": "rate limit exceeded",
                "code": "rate_limit_exceeded"
            })),
        )
            .into_response();
    }

    next.run(req).await
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
