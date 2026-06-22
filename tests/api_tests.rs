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
        cors_allowed_origins: vec![],
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
async fn test_list_webhooks_not_found() {
    let res = test_server().await.get("/payments/nonexistent/webhooks").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(res.json::<Value>()["error"], "payment not found");
}

#[tokio::test]
async fn test_list_webhooks_empty() {
    let server = test_server().await;
    let id = server.post("/payments")
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();

    let res = server.get(&format!("/payments/{id}/webhooks")).await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["payment_id"], id);
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_redeliver_webhook_not_found() {
    let res = test_server().await.get("/payments/nonexistent/webhooks/xyz/redeliver").await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_redeliver_delivery_not_found() {
    let server = test_server().await;
    let id = server.post("/payments")
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();

    let res = server.post(&format!("/payments/{id}/webhooks/nonexistent/redeliver")).await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_webhook_delivery_isolation() {
    let server = test_server().await;
    
    // Create two payments
    let id1 = server.post("/payments")
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();
    
    let id2 = server.post("/payments")
        .json(&json!({ "amount": "10", "asset": "USDC" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();
    
    // Manually insert a delivery for payment 1
    let pool = server.state::<Arc<AppState>>().pool.clone();
    stellargate::db::save_webhook_delivery(
        &pool,
        "delivery-1",
        &id1,
        "https://example.com/webhook",
        r#"{"event":"payment.completed"}"#,
    )
    .await
    .unwrap();
    
    // List webhooks for payment 1 should find it
    let res1 = server.get(&format!("/payments/{id1}/webhooks")).await;
    res1.assert_status_ok();
    assert_eq!(res1.json::<Value>()["deliveries"].as_array().unwrap().len(), 1);
    
    // List webhooks for payment 2 should be empty
    let res2 = server.get(&format!("/payments/{id2}/webhooks")).await;
    res2.assert_status_ok();
    assert_eq!(res2.json::<Value>()["deliveries"].as_array().unwrap().len(), 0);
    
    // Try to redeliver delivery from payment 1 on payment 2 (should fail)
    let res_cross = server.post(&format!("/payments/{id2}/webhooks/delivery-1/redeliver")).await;
    res_cross.assert_status(StatusCode::NOT_FOUND);
}
