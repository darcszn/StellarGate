use axum::http::StatusCode;
use axum::body::Bytes;
use axum_test::TestServer;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{api, config::Config, db, AppState};

async fn test_server() -> TestServer {
    let cfg = Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        gateway_secret: String::new(),
        usdc_issuer: String::new(),
        webhook_secret: String::new(),
        webhook_retry_attempts: 1,
        webhook_retry_delay_ms: 0,
        poll_interval_secs: 10,
    };
    let pool = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str(&cfg.database_url).unwrap().create_if_missing(true))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let http = reqwest::Client::new();
    TestServer::new(api::router(Arc::new(AppState { pool, config: cfg, http }))).unwrap()
}

#[tokio::test]
async fn test_health() {
    let res = test_server().await.get("/health").await;
    res.assert_status_ok();
}

#[tokio::test]
async fn test_create_payment() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["status"], "pending");
    assert_eq!(body["asset"], "XLM");
    assert_eq!(body["memo"].as_str().unwrap().len(), 8);
}

#[tokio::test]
async fn test_create_invalid_asset() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "10", "asset": "BTC" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_invalid_amount() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "-1", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_by_id() {
    let server = test_server().await;
    let id = server.post("/payments")
        .json(&json!({ "amount": "5", "asset": "USDC" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();

    let res = server.get(&format!("/payments/{id}")).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["id"], id);
}

#[tokio::test]
async fn test_get_not_found() {
    let res = test_server().await.get("/payments/does-not-exist").await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_reject_too_many_decimals() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "1.00000001", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_asset_is_case_insensitive() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "1", "asset": "usdc" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>()["asset"], "USDC");
}

#[tokio::test]
async fn test_reject_bad_webhook_url() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "1", "asset": "XLM", "webhook_url": "ftp://x" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_payments() {
    let server = test_server().await;
    for amt in ["1", "2", "3"] {
        server.post("/payments").json(&json!({ "amount": amt, "asset": "XLM" })).await;
    }

    let res = server.get("/payments").await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["total"], 3);
    assert_eq!(body["payments"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_list_filter_by_status() {
    let server = test_server().await;
    server.post("/payments").json(&json!({ "amount": "1", "asset": "XLM" })).await;

    // All created payments start pending, so completed should be empty.
    let res = server.get("/payments?status=completed").await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["total"], 0);

    let res = server.get("/payments?status=pending").await;
    assert_eq!(res.json::<Value>()["total"], 1);
}

#[tokio::test]
async fn test_list_invalid_status() {
    let res = test_server().await.get("/payments?status=bogus").await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_unknown_route_returns_json_404() {
    let res = test_server().await.get("/nope").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(res.json::<Value>()["error"], "not found");
}

#[tokio::test]
async fn test_malformed_json_body_returns_400_json_error() {
    let server = test_server().await;
    let res = server
        .post("/payments")
        .content_type("application/json")
        .bytes(Bytes::from_static(b"{not valid json}"))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let body: Value = res.json();
    assert!(body["error"].as_str().is_some(), "expected an error field");
}

#[tokio::test]
async fn test_missing_amount_returns_400_json_error() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let body: Value = res.json();
    assert!(body["error"].as_str().is_some(), "expected an error field");
}

#[tokio::test]
async fn test_wrong_content_type_returns_400_json_error() {
    let server = test_server().await;
    let res = server
        .post("/payments")
        .content_type("text/plain")
        .bytes(Bytes::from_static(b"amount=10"))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let body: Value = res.json();
    assert!(body["error"].as_str().is_some(), "expected an error field");
}
