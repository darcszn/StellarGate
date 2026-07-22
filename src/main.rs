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
use tracing::{info, warn};
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

    if cfg.gateway_configured() {
        match horizon::check_trustlines(&state).await {
            Ok(missing) if missing.is_empty() => {
                info!("gateway trustlines verified for all accepted assets");
            }
            Ok(missing) => {
                info!(missing = ?missing, "startup trustline check found accepted assets with no trustline");
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not verify gateway trustlines at startup");
            }
        }
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Each background task is wrapped so task_started/task_stopped keep the
    // healthy gauge accurate. Panics are caught at join time and recorded via
    // task_failed so the failure counter (and its alert) fires.
    let stream_handle = if cfg.listener_mode == ListenerMode::Stream {
        let th = state.task_health.clone();
        th.task_started();
        let s = state.clone();
        let rx = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            horizon::run_stream_listener(s, rx).await;
            th.task_stopped();
        }))
    } else {
        None
    };

    let poller_handle = {
        let th = state.task_health.clone();
        th.task_started();
        let s = state.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            horizon::run_poller(s, rx).await;
            th.task_stopped();
        })
    };
    let sweeper_handle = {
        let th = state.task_health.clone();
        th.task_started();
        let s = state.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            expiry::run_sweeper(s, rx).await;
            th.task_stopped();
        })
    };
    let redrive_handle = {
        let th = state.task_health.clone();
        th.task_started();
        let s = state.clone();
        tokio::spawn(async move {
            webhook::run_redrive_worker(s, shutdown_rx).await;
            th.task_stopped();
        })
    };

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("StellarGate API listening on {addr}");

    // Clone before state is moved into the router.
    let task_health = state.task_health.clone();

    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    let _ = shutdown_tx.send(true);
    let timeout = Duration::from_secs(30);
    let bg = async move {
        // A JoinError means the task panicked — record it so the healthy gauge
        // and failure counter both reflect the crash and can fire an alert.
        macro_rules! join_task {
            ($handle:expr) => {
                if let Err(e) = $handle.await {
                    if e.is_panic() {
                        warn!("background task panicked");
                        task_health.task_failed();
                    }
                }
            };
        }
        join_task!(poller_handle);
        join_task!(sweeper_handle);
        join_task!(redrive_handle);
        if let Some(h) = stream_handle {
            join_task!(h);
        }
        join_task!(poller_handle);
        join_task!(sweeper_handle);
        join_task!(redrive_handle);
        if let Some(h) = stream_handle { join_task!(h); }
    };
    if tokio::time::timeout(timeout, bg).await.is_err() {
        info!("background tasks did not finish within 30s; forcing exit");
    }

    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
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
