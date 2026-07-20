//! Integration tests for `webhook::dispatch` against a mock HTTP server.
//!
//! Covers request construction (signature header), retry-then-success, and
//! exhausted-retries paths, asserting both the requests the mock server
//! received and the resulting `webhook_deliveries` row.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stellargate::{
    config::{AcceptedAsset, Config, ListenerMode},
    db, webhook, AppState,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_config(webhook_secret: &str, retry_attempts: u32) -> Config {
    Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        gateway_secret: String::new(),
        accepted_assets: AcceptedAsset::default_list(),
        webhook_secret: webhook_secret.into(),
        webhook_retry_attempts: retry_attempts,
        webhook_retry_delay_ms: 0,
        poll_interval_secs: 10,
        payment_ttl_secs: 3600,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
        rate_limit_requests_per_sec: 1000,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        admin_provisioning_secret: String::new(),
    }
}

async fn setup_state(cfg: Config) -> AppState {
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    AppState {
        pool,
        config: cfg,
        http: reqwest::Client::new(),
    }
}

async fn create_test_payment(state: &AppState, webhook_url: &str) -> db::Payment {
    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_test",
            merchant_id: "merchant1",
            destination_address: "GDESTINATION",
            memo: "MEMOTEST",
            amount: "10",
            asset: "XLM",
            webhook_url: Some(webhook_url),
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn dispatch_delivers_successfully_with_valid_signature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = make_config("test-secret", 3);
    let state = setup_state(cfg).await;
    let payment = create_test_payment(&state, &format!("{}/hook", server.uri())).await;

    webhook::dispatch(&state, &payment, "payment.completed", None).await;

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let req = &received[0];
    /* The signature now covers "{timestamp}.{body}", so verify using the
    timestamp the request advertises in its header. */
    let timestamp: i64 = req
        .headers
        .get("X-StellarGate-Timestamp")
        .expect("timestamp header must be present")
        .to_str()
        .unwrap()
        .parse()
        .expect("timestamp header must be an integer");
    let expected_sig = webhook::sign(&state.config.webhook_secret, timestamp, &req.body);
    assert_eq!(
        req.headers
            .get("X-StellarGate-Signature")
            .expect("signature header must be present")
            .to_str()
            .unwrap(),
        expected_sig
    );
    assert_eq!(
        req.headers.get("X-StellarGate-Event").unwrap(),
        "payment.completed"
    );

    let deliveries = db::list_webhook_deliveries(&state.pool, &payment.id)
        .await
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].status, "delivered");
    assert_eq!(deliveries[0].attempts, 1);
}

#[tokio::test]
async fn dispatch_retries_on_5xx_then_succeeds() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(move |_req: &wiremock::Request| {
            let attempt = calls_for_responder.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let cfg = make_config("test-secret", 3);
    let state = setup_state(cfg).await;
    let payment = create_test_payment(&state, &format!("{}/hook", server.uri())).await;

    webhook::dispatch(&state, &payment, "payment.completed", None).await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let deliveries = db::list_webhook_deliveries(&state.pool, &payment.id)
        .await
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].status, "delivered");
    assert_eq!(deliveries[0].attempts, 2);
}

#[tokio::test]
async fn dispatch_marks_failed_after_exhausting_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;

    let cfg = make_config("test-secret", 3);
    let state = setup_state(cfg).await;
    let payment = create_test_payment(&state, &format!("{}/hook", server.uri())).await;

    webhook::dispatch(&state, &payment, "payment.completed", None).await;

    let deliveries = db::list_webhook_deliveries(&state.pool, &payment.id)
        .await
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].status, "failed");
    assert_eq!(deliveries[0].attempts, 3);
}
