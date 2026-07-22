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
use std::time::Duration;
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
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
    let request_timeout = Duration::from_secs(state.config.request_timeout_secs);

    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_handler))
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
            /* Auth middleware on the write + list routes, the webhook listing,
            and redelivery (it triggers a merchant-scoped outbound request);
            only the per-payment status endpoint stays public (anyone with the
            id can poll it). */
            let authed = axum::Router::new()
                .route("/", post(payments::create).get(payments::list))
                .route("/:id/webhooks", get(payments::list_webhooks))
                .route(
                    "/:id/webhooks/:delivery_id/redeliver",
                    post(payments::redeliver_webhook),
                )
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ));

            axum::Router::new()
                .merge(authed)
                .route("/:id", get(payments::get_by_id))
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
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
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
    if let Some(bucket) = rate_limited_bucket(&req) {
        let key = rate_limit_key(bucket, &req);
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

/// Identifies which rate-limit bucket (if any) a request falls into. Returns
/// `None` for everything else, so unrelated routes aren't limited at all.
///
/// Redelivery is bucketed by shape rather than by path: the URL carries a
/// payment and delivery id, and keying on those would let every id mint its
/// own limiter entry — both an unbounded map and a trivially bypassed limit.
fn rate_limited_bucket(req: &Request) -> Option<&'static str> {
    if req.method() != axum::http::Method::POST {
        return None;
    }
    let path = req.uri().path();
    match path {
        "/payments" => Some("payments"),
        "/merchants" => Some("merchants"),
        _ if path.starts_with("/payments/") && path.ends_with("/redeliver") => Some("redeliver"),
        _ => None,
    }
}

/// Keyed by bucket + client so each bucket is rate-limited independently —
/// provisioning a merchant should never eat into a client's payment quota (or
/// vice versa).
fn rate_limit_key(bucket: &str, req: &Request) -> String {
    format!("{bucket}:{}", client_ip_key(req))
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

    let allow_origins: Vec<axum::http::HeaderValue> = origins
        .iter()
        .map(|o| {
            o.parse().unwrap_or_else(|e| {
                // Origins are validated in Config::from_env, so this branch is
                // unreachable in production. Treat it as a programming error.
                panic!("BUG: unparseable CORS origin {o:?} reached build_cors: {e}")
            })
        })
        .collect();

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

/// Readiness probe — returns 200 only when both the database AND Horizon are
/// reachable. A pod that cannot reach Horizon cannot detect on-chain payments;
/// routing traffic to it is worse than routing it elsewhere (issue #172).
///
/// Uses a 3-second timeout on the Horizon check so a slow node never hangs
/// the probe. The check is skipped when no gateway is configured
/// (STELLAR_GATEWAY_PUBLIC=UNCONFIGURED) since without a gateway there is no
/// on-chain work to do.
async fn ready(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 1. Database must respond.
    if db::ping(&state.pool).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unavailable", "reason": "database unreachable" })),
        )
            .into_response();
    }

    // 2. Horizon must respond (only when a gateway wallet is configured).
    if state.config.gateway_configured() {
        if let Err(reason) = check_horizon_ready(&state).await {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "reason": reason })),
            )
                .into_response();
        }
    }

    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

/// Probe Horizon with a hard 3-second timeout.
/// Returns Ok(()) when reachable (any non-5xx response), or an error string.
async fn check_horizon_ready(state: &Arc<AppState>) -> Result<(), String> {
    let url = state.config.horizon_url.trim_end_matches('/').to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(3_000),
        state.http.get(&url).header("Accept", "application/json").send(),
    )
    .await;
    match result {
        Ok(Ok(resp)) if resp.status().as_u16() < 500 => Ok(()),
        Ok(Ok(resp)) => Err(format!("Horizon returned {}", resp.status())),
        Ok(Err(e))   => Err(format!("Horizon unreachable: {e}")),
        Err(_)       => Err("Horizon health check timed out".to_string()),
    }
}

/// `GET /metrics` — Prometheus-compatible plain-text metrics snapshot.
async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let body = crate::metrics::render(&state.webhook_metrics);
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "code": "not_found" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use tower_http::timeout::TimeoutLayer;

    /// Exercises the exact `TimeoutLayer` construction used in `router()`,
    /// against a router small enough to run with millisecond durations —
    /// `request_timeout_secs` itself is whole seconds, too coarse for a fast test.
    fn timeout_test_router(timeout: Duration) -> axum::Router {
        axum::Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }),
            )
            .route("/fast", get(|| async { "ok" }))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                timeout,
            ))
    }

    #[tokio::test]
    async fn slow_handler_is_aborted_with_408() {
        let server = TestServer::new(timeout_test_router(Duration::from_millis(20))).unwrap();
        let response = server.get("/slow").await;
        response.assert_status(StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn fast_handler_is_unaffected() {
        let server = TestServer::new(timeout_test_router(Duration::from_millis(200))).unwrap();
        let response = server.get("/fast").await;
        response.assert_status_ok();
    }
}
