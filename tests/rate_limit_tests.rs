//! Rate-limit behaviour lives in its own integration binary on purpose.
//!
//! The broader API tests run at a high limit and exercise merchant auth heavily.
//! Keeping the low-quota assertion here makes the expected 429 path explicit.

use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{
    api,
    config::{Config, ListenerMode},
    db, AppState,
};

fn make_config(rate_limit_requests_per_sec: u32) -> Config {
    Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        gateway_secret: String::new(),
        accepted_assets: stellargate::config::AcceptedAsset::default_list(),
        webhook_secret: String::new(),
        webhook_retry_attempts: 1,
        webhook_retry_delay_ms: 0,
        poll_interval_secs: 10,
        payment_ttl_secs: 3600,
        rate_limit_requests_per_sec,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
        admin_provisioning_secret: TEST_ADMIN_SECRET.into(),
    }
}

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

async fn server_with_config(cfg: Config) -> TestServer {
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let http = reqwest::Client::new();
    let router = api::router(Arc::new(AppState {
        pool,
        config: cfg,
        http,
    }))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    TestServer::new(router).unwrap()
}

async fn provision_merchant(server: &TestServer) -> String {
    let res = server
        .post("/merchants")
        .add_header("X-Admin-Secret", TEST_ADMIN_SECRET)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["api_key"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_rate_limit_exceeded_returns_429() {
    let server = server_with_config(make_config(1)).await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    // The first request consumes the single per-second token.
    let first = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    first.assert_status(StatusCode::CREATED);

    // A second immediate request exceeds the quota and is rejected.
    let second = server
        .post("/payments")
        .add_header("Authorization", auth)
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    second.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.json::<Value>()["code"], "rate_limit_exceeded");
}
