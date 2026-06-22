use crate::AppState;
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tower_http::{
    cors::CorsLayer, limit::RequestBodyLimitLayer, trace::TraceLayer,
};

mod payments;

/// Reject request bodies larger than this (256 KiB) before they hit a handler.
const MAX_BODY_BYTES: usize = 256 * 1024;

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = build_cors(&state.config);
    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/payments", post(payments::create).get(payments::list))
        .route("/payments/:id", get(payments::get_by_id))
        .route("/payments/:id/webhooks", get(payments::list_webhooks))
        .route("/payments/:id/webhooks/:delivery_id/redeliver", post(payments::redeliver_webhook))
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(state)
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
        .filter_map(|o| o.parse().ok())
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

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found" })),
    )
}
