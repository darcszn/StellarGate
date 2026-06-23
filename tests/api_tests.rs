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
use time::format_description::well_known::Rfc3339;

fn make_config() -> Config {
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
        // High enough that these tests never trip the limiter; dedicated
        // rate-limit coverage lives in tests/rate_limit_tests.rs.
        rate_limit_requests_per_sec: 1000,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
    }
}

async fn test_server_with_pool() -> (TestServer, db::Db) {
    server_with_config(make_config()).await
}

async fn server_with_config(cfg: Config) -> (TestServer, db::Db) {
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
        pool: pool.clone(),
        config: cfg,
        http,
    }))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = TestServer::new(router).unwrap();
    (server, pool)
}

async fn test_server() -> TestServer {
    test_server_with_pool().await.0
}

/// Provision a merchant via POST /merchants and return the API key.
async fn provision_merchant(server: &TestServer) -> String {
    let res = server.post("/merchants").await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["api_key"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn test_health() {
    let res = test_server().await.get("/health").await;
    res.assert_status_ok();
    assert_eq!(res.json::<serde_json::Value>()["status"], "ok");
}

#[tokio::test]
async fn test_ready_ok_with_live_db() {
    let res = test_server().await.get("/ready").await;
    res.assert_status_ok();
    assert_eq!(res.json::<serde_json::Value>()["status"], "ok");
}

#[tokio::test]
async fn test_unauthenticated_create_returns_401() {
    let res = test_server()
        .await
        .post("/payments")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(res.json::<Value>()["code"], "unauthorized");
}

#[tokio::test]
async fn test_unauthenticated_list_returns_401() {
    let res = test_server().await.get("/payments").await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(res.json::<Value>()["code"], "unauthorized");
}

#[tokio::test]
async fn test_invalid_api_key_returns_401() {
    let res = test_server()
        .await
        .post("/payments")
        .add_header("Authorization", "Bearer not-a-real-key")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_create_payment() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["status"], "pending");
    assert_eq!(body["asset"], "XLM");
    assert_eq!(body["memo"].as_str().unwrap().len(), 8);
}

/// Timestamps must be strict RFC 3339 UTC with an explicit Z suffix.
#[tokio::test]
async fn test_timestamps_are_rfc3339_utc() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();

    for field in ["created_at", "updated_at"] {
        let ts = body[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        time::OffsetDateTime::parse(ts, &Rfc3339)
            .unwrap_or_else(|e| panic!("{field} = {ts:?} is not valid RFC 3339: {e}"));
        assert!(
            ts.ends_with('Z'),
            "{field} = {ts:?} must have explicit Z suffix"
        );
    }
}

#[tokio::test]
async fn test_idempotency_key_returns_same_payment() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    // First request mints a new payment (201 Created).
    let res1 = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .add_header("Idempotency-Key", "retry-abc-123")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res1.assert_status(StatusCode::CREATED);
    let id1 = res1.json::<Value>()["id"].as_str().unwrap().to_string();

    // Identical retry with the same key returns the original payment (200 OK).
    let res2 = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .add_header("Idempotency-Key", "retry-abc-123")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res2.assert_status_ok();
    let id2 = res2.json::<Value>()["id"].as_str().unwrap().to_string();

    assert_eq!(id1, id2, "same idempotency key must yield the same payment");

    // Exactly one payment visible to this merchant.
    let list: Value = server
        .get("/payments")
        .add_header("Authorization", auth)
        .await
        .json();
    assert_eq!(list["total"], 1);
}

#[tokio::test]
async fn test_different_or_missing_idempotency_key_creates_new_payment() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    let id_a = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .add_header("Idempotency-Key", "key-a")
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res_b = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .add_header("Idempotency-Key", "key-b")
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    res_b.assert_status(StatusCode::CREATED);
    let id_b = res_b.json::<Value>()["id"].as_str().unwrap().to_string();

    let res_c = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    res_c.assert_status(StatusCode::CREATED);
    let id_c = res_c.json::<Value>()["id"].as_str().unwrap().to_string();

    assert_ne!(id_a, id_b);
    assert_ne!(id_a, id_c);
    assert_ne!(id_b, id_c);

    let list: Value = server
        .get("/payments")
        .add_header("Authorization", auth)
        .await
        .json();
    assert_eq!(list["total"], 3);
}

#[tokio::test]
async fn test_idempotency_key_scoped_per_merchant() {
    let server = test_server().await;
    let key1 = provision_merchant(&server).await;
    let key2 = provision_merchant(&server).await;

    // Same idempotency key, different merchants → two distinct payments.
    let id_m1 = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key1}"))
        .add_header("Idempotency-Key", "shared-key")
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let id_m2 = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key2}"))
        .add_header("Idempotency-Key", "shared-key")
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ne!(
        id_m1, id_m2,
        "same key under different merchants must not collide"
    );

    // Re-using key1's idempotency key under merchant1 returns merchant1's original payment.
    let res_retry = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key1}"))
        .add_header("Idempotency-Key", "shared-key")
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    res_retry.assert_status_ok();
    assert_eq!(res_retry.json::<Value>()["id"].as_str().unwrap(), id_m1);
}

#[tokio::test]
async fn test_merchant_list_scoped_to_own_payments() {
    let server = test_server().await;
    let key1 = provision_merchant(&server).await;
    let key2 = provision_merchant(&server).await;

    // Merchant 1 creates 2 payments.
    for _ in 0..2 {
        server
            .post("/payments")
            .add_header("Authorization", format!("Bearer {key1}"))
            .json(&json!({ "amount": "1", "asset": "XLM" }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    // Merchant 2 creates 1 payment.
    server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key2}"))
        .json(&json!({ "amount": "2", "asset": "XLM" }))
        .await
        .assert_status(StatusCode::CREATED);

    // Each merchant only sees their own payments.
    let list1: Value = server
        .get("/payments")
        .add_header("Authorization", format!("Bearer {key1}"))
        .await
        .json();
    assert_eq!(list1["total"], 2, "merchant1 should see 2 payments");

    let list2: Value = server
        .get("/payments")
        .add_header("Authorization", format!("Bearer {key2}"))
        .await
        .json();
    assert_eq!(list2["total"], 1, "merchant2 should see 1 payment");
}

#[tokio::test]
async fn test_create_invalid_asset() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "10", "asset": "BTC" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["code"], "unsupported_asset");
    res.assert_contains_header("x-request-id");
}

#[tokio::test]
async fn test_create_invalid_amount() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "-1", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_by_id() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let id = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "5", "asset": "USDC" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = server.get(&format!("/payments/{id}")).await;
    res.assert_status_ok();
    let body = res.json::<Value>();
    assert_eq!(body["id"], id);

    // Timestamps on the GET response must also be strict RFC 3339.
    for field in ["created_at", "updated_at"] {
        let ts = body[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        time::OffsetDateTime::parse(ts, &Rfc3339)
            .unwrap_or_else(|e| panic!("{field} = {ts:?} is not valid RFC 3339: {e}"));
    }
}

#[tokio::test]
async fn test_get_not_found() {
    let res = test_server().await.get("/payments/does-not-exist").await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_reject_too_many_decimals() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "1.00000001", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_asset_is_case_insensitive() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "1", "asset": "usdc" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>()["asset"], "USDC");
}

#[tokio::test]
async fn test_reject_bad_webhook_url() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "1", "asset": "XLM", "webhook_url": "ftp://x" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_payments() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");
    for amt in ["1", "2", "3"] {
        server
            .post("/payments")
            .add_header("Authorization", auth.clone())
            .json(&json!({ "amount": amt, "asset": "XLM" }))
            .await;
    }

    let res = server
        .get("/payments")
        .add_header("Authorization", auth)
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["total"], 3);
    assert_eq!(body["payments"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_list_filter_by_status() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");
    server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;

    let res = server
        .get("/payments?status=completed")
        .add_header("Authorization", auth.clone())
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["total"], 0);

    let res = server
        .get("/payments?status=pending")
        .add_header("Authorization", auth)
        .await;
    assert_eq!(res.json::<Value>()["total"], 1);
}

#[tokio::test]
async fn test_list_invalid_status() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .get("/payments?status=bogus")
        .add_header("Authorization", format!("Bearer {key}"))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_cursor_pagination() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");
    for amt in ["1", "2", "3", "4", "5"] {
        server
            .post("/payments")
            .add_header("Authorization", auth.clone())
            .json(&json!({ "amount": amt, "asset": "XLM" }))
            .await;
    }

    // Page 1 via offset path — also returns next_cursor for migration.
    let res = server
        .get("/payments?limit=2")
        .add_header("Authorization", auth.clone())
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["payments"].as_array().unwrap().len(), 2);
    let cursor = body["next_cursor"]
        .as_str()
        .expect("next_cursor must be present on a full page");

    // Page 2 via keyset cursor.
    let res2 = server
        .get(&format!("/payments?cursor={cursor}&limit=2"))
        .add_header("Authorization", auth.clone())
        .await;
    res2.assert_status_ok();
    let body2: Value = res2.json();
    assert_eq!(body2["payments"].as_array().unwrap().len(), 2);
    let cursor2 = body2["next_cursor"]
        .as_str()
        .expect("next_cursor must be present on a full page");

    // Page 3 — last page, fewer items than limit.
    let res3 = server
        .get(&format!("/payments?cursor={cursor2}&limit=2"))
        .add_header("Authorization", auth.clone())
        .await;
    res3.assert_status_ok();
    let body3: Value = res3.json();
    assert_eq!(body3["payments"].as_array().unwrap().len(), 1);
    assert!(
        body3["next_cursor"].is_null(),
        "last page must have null next_cursor"
    );

    // All 5 IDs are unique across all pages.
    let ids: Vec<String> = [&body, &body2, &body3]
        .iter()
        .flat_map(|b| b["payments"].as_array().unwrap().iter())
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 5);
}

#[tokio::test]
async fn test_list_cursor_invalid() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let res = server
        .get("/payments?cursor=notvalidhex!!")
        .add_header("Authorization", format!("Bearer {key}"))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_unknown_route_returns_json_404() {
    let res = test_server().await.get("/nope").await;
    res.assert_status(StatusCode::NOT_FOUND);
    let body: Value = res.json();
    assert_eq!(body["error"], "not found");
    assert_eq!(body["code"], "not_found");
    res.assert_contains_header("x-request-id");
}

#[tokio::test]
async fn test_list_webhooks_not_found() {
    let res = test_server()
        .await
        .get("/payments/nonexistent/webhooks")
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
    let body: Value = res.json();
    assert_eq!(body["error"], "payment not found");
    assert_eq!(body["code"], "payment_not_found");
}

#[tokio::test]
async fn test_list_webhooks_empty() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let id = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = server.get(&format!("/payments/{id}/webhooks")).await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["payment_id"], id);
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_redeliver_webhook_not_found() {
    let res = test_server()
        .await
        .post("/payments/nonexistent/webhooks/xyz/redeliver")
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_redeliver_delivery_not_found() {
    let server = test_server().await;
    let key = provision_merchant(&server).await;
    let id = server
        .post("/payments")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = server
        .post(&format!("/payments/{id}/webhooks/nonexistent/redeliver"))
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_webhook_delivery_isolation() {
    let (server, pool) = test_server_with_pool().await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    // Create two payments
    let id1 = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "5", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let id2 = server
        .post("/payments")
        .add_header("Authorization", auth)
        .json(&json!({ "amount": "10", "asset": "USDC" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Manually insert a delivery for payment 1
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
    assert_eq!(
        res1.json::<Value>()["deliveries"].as_array().unwrap().len(),
        1
    );

    // List webhooks for payment 2 should be empty
    let res2 = server.get(&format!("/payments/{id2}/webhooks")).await;
    res2.assert_status_ok();
    assert_eq!(
        res2.json::<Value>()["deliveries"].as_array().unwrap().len(),
        0
    );

    // Try to redeliver delivery from payment 1 on payment 2 (should fail)
    let res_cross = server
        .post(&format!("/payments/{id2}/webhooks/delivery-1/redeliver"))
        .await;
    res_cross.assert_status(StatusCode::NOT_FOUND);
}
