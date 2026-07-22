use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use stellargate::{
    api,
    config::{Config, ListenerMode},
    db, expiry, horizon,
    metrics::WebhookMetrics,
    webhook, AppState,
};
use tokio::sync::watch;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    dotenvy::dotenv().ok();

    let cfg = Config::from_env()?;

    let pool = SqlitePoolOptions::new()
        .max_connections(cfg.db_pool_max_connections)
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_millis(cfg.db_busy_timeout_ms)),
        )
        .await?;
    db::migrate(&pool).await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("StellarGate/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Dedicated client for outbound webhook POSTs. A shorter, independent
    // timeout prevents a slow receiver from blocking the reconciler for the
    // full 30 s shared-client window (which — with retries — becomes
    // attempts × 30 s of settlement-blocking latency).
    let webhook_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.webhook_timeout_secs))
        .user_agent(concat!("StellarGate/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let state = Arc::new(AppState {
        pool,
        config: cfg.clone(),
        http,
        webhook_http,
        webhook_metrics: WebhookMetrics::new(),
    });

    /* Verify the gateway account can actually receive every accepted asset.
    A missing trustline mints unpayable intents, so surface it loudly at boot
    rather than letting payments silently bounce on-chain. Best-effort: a
    Horizon hiccup (or a not-yet-funded account) must not block startup. */
    if cfg.gateway_configured() {
        match horizon::check_trustlines(&state).await {
            Ok(missing) if missing.is_empty() => {
                info!("gateway trustlines verified for all accepted assets");
            }
            Ok(missing) => {
                info!(
                    missing = ?missing,
                    "startup trustline check found accepted assets with no trustline"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not verify gateway trustlines at startup");
            }
        }
    }

    // Broadcast shutdown to all background tasks.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    /* Detect on-chain payments. In stream mode the SSE listener settles intents
    in near real time while the poller runs alongside as a reconciler; in
    poll mode only the interval poller runs. */
    let stream_handle = if cfg.listener_mode == ListenerMode::Stream {
        Some(tokio::spawn(horizon::run_stream_listener(
            state.clone(),
            shutdown_rx.clone(),
        )))
    } else {
        None
    };
    let poller_handle = tokio::spawn(horizon::run_poller(state.clone(), shutdown_rx.clone()));
    let sweeper_handle = tokio::spawn(expiry::run_sweeper(state.clone(), shutdown_rx.clone()));
    let redrive_handle = tokio::spawn(webhook::run_redrive_worker(state.clone(), shutdown_rx));

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("StellarGate API listening on {addr}");

    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Signal background tasks and wait (bounded) for them to finish.
    let _ = shutdown_tx.send(true);
    let timeout = Duration::from_secs(30);
    let bg = async {
        let _ = poller_handle.await;
        let _ = sweeper_handle.await;
        let _ = redrive_handle.await;
        if let Some(h) = stream_handle {
            let _ = h.await;
        }
    };
    if tokio::time::timeout(timeout, bg).await.is_err() {
        info!("background tasks did not finish within 30s; forcing exit");
    }

    info!("shutdown complete");
    Ok(())
}

/// Resolves when the process receives Ctrl-C (or SIGTERM on Unix), letting axum
/// drain in-flight requests before exiting.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
