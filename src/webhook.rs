//! Outbound webhook delivery.
//!
//! When a payment reaches a terminal state we POST a signed JSON event to the
//! merchant's `webhook_url`. The body is signed with HMAC-SHA256 over the exact
//! bytes we send, and the hex digest is placed in `X-StellarGate-Signature` so
//! the receiver can verify authenticity.

use crate::{db, AppState};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Compute the hex-encoded HMAC-SHA256 signature for a webhook body.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
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
    let signature = sign(&state.config.webhook_secret, &body);

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

    let attempts = state.config.webhook_retry_attempts.max(1);
    let delay = Duration::from_millis(state.config.webhook_retry_delay_ms);

    for attempt in 1..=attempts {
        let result = state
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-StellarGate-Signature", &signature)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_and_keyed() {
        // Known HMAC-SHA256 vector: key "key", message "The quick brown fox jumps over the lazy dog".
        let sig = sign("key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            sig,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn signature_changes_with_secret() {
        let body = b"{\"event\":\"payment.success\"}";
        assert_ne!(sign("secret-a", body), sign("secret-b", body));
    }
}
