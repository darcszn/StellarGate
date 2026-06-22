//! Expiry sweeper: retiring pending payment intents that were never paid.
//!
//! Every intent is created with an `expires_at` timestamp (`created_at +
//! PAYMENT_TTL_SECS`). A background task periodically transitions any `pending`
//! intent past that deadline to the terminal `expired` status and fires a
//! `payment.expired` webhook, so the Horizon poller stops watching dead intents
//! and merchants get a definite signal that the intent will never settle.
//!
//! Expiry is purely time- and database-driven, so the sweeper runs even when no
//! Stellar gateway is configured (unlike the Horizon poller).

use crate::{db, webhook, AppState};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// The webhook event emitted when an intent is swept to `expired`.
const EXPIRED_EVENT: &str = "payment.expired";

/// Run one sweep: expire every overdue pending intent and fire its webhook.
/// Returns how many intents were expired. Safe to call repeatedly — an intent
/// is transitioned at most once.
pub async fn sweep_once(state: &Arc<AppState>) -> anyhow::Result<usize> {
    let expired = db::expire_overdue(&state.pool).await?;
    for payment in &expired {
        info!(payment_id = %payment.id, "payment intent expired");
        webhook::dispatch(state, payment, EXPIRED_EVENT, None).await;
    }
    Ok(expired.len())
}

/// Background loop that sweeps expired intents on the configured poll interval
/// until the process shuts down.
pub async fn run_sweeper(state: Arc<AppState>) {
    let interval = Duration::from_secs(state.config.poll_interval_secs.max(1));
    info!(
        ttl_secs = state.config.payment_ttl_secs,
        interval_secs = state.config.poll_interval_secs,
        "expiry sweeper started"
    );

    loop {
        tokio::time::sleep(interval).await;
        match sweep_once(&state).await {
            Ok(0) => debug!("sweep: nothing to expire"),
            Ok(n) => info!(expired = n, "sweep expired payments"),
            Err(e) => warn!(error = %e, "sweep cycle failed"),
        }
    }
}
