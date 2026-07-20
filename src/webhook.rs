//! Outbound webhook delivery.
//!
//! When a payment reaches a terminal state we POST a signed JSON event to the
//! merchant's `webhook_url`. To let receivers prove both authenticity and
//! freshness, every request carries two headers:
//!
//! - `X-StellarGate-Timestamp`: the Unix time (seconds) the event was signed.
//! - `X-StellarGate-Signature`: the hex HMAC-SHA256 of `"{timestamp}.{body}"`
//!   (Stripe-style), keyed with the shared `WEBHOOK_SECRET`.
//!
//! Binding the signature to the timestamp stops a captured request from being
//! replayed indefinitely: a receiver recomputes the signature over the same
//! `"{timestamp}.{body}"` string and rejects timestamps that fall outside a
//! small tolerance window. See the README "Verifying webhooks" section for the
//! verification recipe and recommended window.

use crate::{db, AppState};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Compute the hex-encoded HMAC-SHA256 signature for a webhook, binding it to
/// `timestamp` by signing the Stripe-style payload `"{timestamp}.{body}"`.
///
/// Receivers must recompute the signature over the same `"{timestamp}.{body}"`
/// string and reject the request if `timestamp` is too far from their own clock
/// (see the README), which is what prevents replay of an old, valid signature.
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Current Unix time in seconds, used as the webhook signing timestamp.
pub(crate) fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the JSON event payload for a payment in a terminal state.
///
/// `delta` carries the absolute difference between the requested and received
/// amounts, and is included in the payload for `payment.overpaid` (excess to
/// refund) and `payment.underpaid` (shortfall still owed) events.
pub fn build_payload(payment: &db::Payment, event: &str, delta: Option<&str>) -> serde_json::Value {
    let mut payload = json!({
        "event": event,
        "payment_id": payment.id,
        "merchant_id": payment.merchant_id,
        "tx_hash": payment.tx_hash,
        "amount": payment.amount,
        "paid_amount": payment.paid_amount,
        "asset": payment.asset,
        "status": payment.status,
    });
    if let Some(d) = delta {
        payload["delta"] = json!(d);
    }
    payload
}

/// Dispatch a webhook for `payment` if it has a `webhook_url`, retrying on
/// failure per the configured policy. Each attempt and the final outcome are
/// recorded in the `webhook_deliveries` table. Errors are logged, never
/// propagated — a failed webhook must not roll back a confirmed payment.
///
/// `delta` is the absolute amount difference included in overpaid/underpaid
/// events; pass `None` for exact-payment events.
pub async fn dispatch(state: &AppState, payment: &db::Payment, event: &str, delta: Option<&str>) {
    let Some(url) = payment.webhook_url.clone() else {
        return;
    };

    let payload = build_payload(payment, event, delta);
    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!(payment_id = %payment.id, error = %e, "failed to serialize webhook payload");
            return;
        }
    };
    let timestamp = current_timestamp();
    let signature = sign(&state.config.webhook_secret, timestamp, &body);

    let delivery_id = Uuid::new_v4().to_string();
    if let Err(e) = db::save_webhook_delivery(
        &state.pool,
        &delivery_id,
        &payment.id,
        &url,
        &String::from_utf8_lossy(&body),
    )
    .await
    {
        warn!(error = %e, "failed to record webhook delivery");
    }

    let client = match safe_client(state, &url).await {
        Ok(c) => c,
        Err(e) => {
            warn!(payment_id = %payment.id, %url, error = %e, "webhook blocked by SSRF guard");
            let _ = db::update_webhook_delivery(&state.pool, &delivery_id, "failed", 0).await;
            return;
        }
    };

    let attempts = state.config.webhook_retry_attempts.max(1);
    let delay = Duration::from_millis(state.config.webhook_retry_delay_ms);

    for attempt in 1..=attempts {
        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-StellarGate-Signature", &signature)
            .header("X-StellarGate-Timestamp", timestamp.to_string())
            .header("X-StellarGate-Event", event)
            .body(body.clone())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                info!(payment_id = %payment.id, %url, attempt, "webhook delivered");
                let _ = db::update_webhook_delivery(
                    &state.pool,
                    &delivery_id,
                    "delivered",
                    attempt as i64,
                )
                .await;
                return;
            }
            Ok(resp) => {
                warn!(payment_id = %payment.id, status = %resp.status(), attempt, "webhook rejected");
            }
            Err(e) => {
                warn!(payment_id = %payment.id, error = %e, attempt, "webhook request failed");
            }
        }

        if attempt < attempts {
            tokio::time::sleep(delay).await;
        }
    }

    warn!(payment_id = %payment.id, %url, "webhook delivery exhausted all retries");
    let _ = db::update_webhook_delivery(&state.pool, &delivery_id, "failed", attempts as i64).await;
}

/// Resolve and SSRF-check `url`, returning a client pinned to the validated
/// address. Honors `webhook_allow_private_targets` for local dev/tests that
/// intentionally target a loopback mock server.
pub(crate) async fn safe_client(state: &AppState, url: &str) -> anyhow::Result<reqwest::Client> {
    let target = crate::ssrf::validate(url, state.config.webhook_allow_private_targets).await?;
    Ok(crate::ssrf::pinned_client(&target)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_deterministic() {
        // Identical inputs always produce the same digest.
        let body = b"{\"event\":\"payment.success\"}";
        assert_eq!(
            sign("key", 1_700_000_000, body),
            sign("key", 1_700_000_000, body)
        );
    }

    #[test]
    fn signature_matches_known_vector() {
        /* Locks the Stripe-style signed payload format "{timestamp}.{body}".
        Independently reproducible:
          printf '1700000000.{"id":"evt_1"}' | openssl dgst -sha256 -hmac whsec_test */
        let sig = sign("whsec_test", 1_700_000_000, br#"{"id":"evt_1"}"#);
        assert_eq!(
            sig,
            "c89214b5b5da833daed6f0b8c5bb6bd58cea9022bd80ccc78230f3942d632925"
        );
    }

    #[test]
    fn signature_covers_timestamp() {
        /* The whole point of the timestamp: changing only it changes the
        signature, so an old signature cannot be replayed with a fresh time. */
        let body = b"{\"event\":\"payment.completed\"}";
        assert_ne!(
            sign("secret", 1_700_000_000, body),
            sign("secret", 1_700_000_001, body),
            "changing only the timestamp must change the signature"
        );
    }

    #[test]
    fn signature_changes_with_secret() {
        let body = b"{\"event\":\"payment.success\"}";
        assert_ne!(sign("secret-a", 1, body), sign("secret-b", 1, body));
    }
}
