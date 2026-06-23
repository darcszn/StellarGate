use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use stellargate::{
    api,
    config::{Config, ListenerMode},
    db, expiry, horizon, AppState,
};
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
        .connect_with(SqliteConnectOptions::from_str(&cfg.database_url)?.create_if_missing(true))
        .await?;
    db::migrate(&pool).await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("StellarGate/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let state = Arc::new(AppState {
        pool,
        config: cfg.clone(),
        http,
    });

    // Broadcast shutdown to all background tasks.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Detect on-chain payments. In stream mode the SSE listener settles intents
    // in near real time while the poller runs alongside as a reconciler; in
    // poll mode only the interval poller runs.
    let stream_handle = if cfg.listener_mode == ListenerMode::Stream {
        Some(tokio::spawn(horizon::run_stream_listener(
            state.clone(),
            shutdown_rx.clone(),
        )))
    } else {
        None
    };
    let poller_handle = tokio::spawn(horizon::run_poller(state.clone(), shutdown_rx.clone()));
    let sweeper_handle = tokio::spawn(expiry::run_sweeper(state.clone(), shutdown_rx));

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("StellarGate API listening on {addr}");

    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

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
