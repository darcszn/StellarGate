//! Stellar Horizon integration: detecting and verifying on-chain payments.
//!
//! A background poller periodically asks Horizon for the most recent payments
//! into the gateway account, matches them against pending payment intents by
//! transaction memo, verifies the asset and amount, and transitions the intent
//! to a terminal or watchable state, firing a webhook in each case.
//!
//! ## Payment resolution policy
//!
//! | Scenario | DB status | Webhook event | Notes |
//! |---|---|---|---|
//! | Paid exactly the requested amount | `completed` | `payment.completed` | — |
//! | Paid **more** than requested | `completed` | `payment.overpaid` | `delta` = excess; merchant should refund |
//! | Paid **less** than requested | `underpaid` | `payment.underpaid` | `delta` = shortfall; intent stays watchable |
//! | Top-up brings total to exactly expected | `completed` | `payment.completed` | — |
//! | Top-up brings total above expected | `completed` | `payment.overpaid` | `delta` = cumulative excess |
//!
//! Once an intent reaches `completed`, it is removed from the watchlist.
//! Any subsequent on-chain payment to the same address and memo is silently
//! ignored — it will not trigger an additional webhook.
//!
//! Only a single follow-up (top-up) payment is supported per underpaid intent:
//! `tx_hash` records the most recent processed transaction. If more than one
//! partial payment is needed, the user should send the full remaining balance
//! (shown in the `delta` field of the `payment.underpaid` event) in one
//! transaction.
//!
//! The matching logic in [`verify`] is pure and unit-tested; the networked
//! [`fetch_recent_payments`] and [`run_poller`] wrap it with I/O.

use crate::{db, money, webhook, AppState};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// A single payment operation as returned by Horizon, with the embedded
/// transaction (requested via `join=transactions`) so we can read its memo.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonPayment {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub amount: Option<String>,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub asset_code: Option<String>,
    #[serde(default)]
    pub asset_issuer: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    #[serde(default)]
    pub transaction: Option<TransactionRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransactionRef {
    #[serde(default)]
    pub memo: Option<String>,
    #[serde(default)]
    pub memo_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PaymentsPage {
    #[serde(rename = "_embedded")]
    embedded: Embedded,
}

#[derive(Debug, Deserialize)]
struct Embedded {
    records: Vec<HorizonPayment>,
}

/// The outcome of matching a Horizon payment against a pending intent.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Cumulative paid amount equals the requested amount exactly.
    Completed { tx_hash: String, paid_amount: String },
    /// Cumulative paid amount exceeds the requested amount.
    /// The intent is fulfilled; `delta` is the excess the merchant should refund.
    Overpaid { tx_hash: String, paid_amount: String },
    /// Cumulative paid amount is still below the requested amount.
    /// The intent remains open; `delta` is the shortfall still owed.
    Underpaid { tx_hash: String, paid_amount: String },
}

impl HorizonPayment {
    fn memo(&self) -> Option<&str> {
        self.transaction.as_ref().and_then(|t| t.memo.as_deref())
    }
}

/// Decide whether a Horizon payment satisfies a pending intent.
///
/// `already_paid_stroops` is the cumulative amount already received for this
/// intent (0 for a fresh `pending` payment, non-zero for an `underpaid` one).
///
/// Returns `None` when the payment is unrelated (wrong type, destination, memo,
/// or asset). When it matches, returns the verdict for the cumulative total.
pub fn verify(
    payment: &db::Payment,
    hp: &HorizonPayment,
    usdc_issuer: &str,
    already_paid_stroops: i64,
) -> Option<Verdict> {
    if hp.kind != "payment" {
        return None;
    }
    if hp.to.as_deref() != Some(payment.destination_address.as_str()) {
        return None;
    }
    if hp.memo() != Some(payment.memo.as_str()) {
        return None;
    }

    let asset_matches = match payment.asset.as_str() {
        "XLM" => hp.asset_type.as_deref() == Some("native"),
        "USDC" => {
            hp.asset_code.as_deref() == Some("USDC")
                && hp.asset_issuer.as_deref() == Some(usdc_issuer)
        }
        _ => false,
    };
    if !asset_matches {
        return None;
    }

    let raw_amount = hp.amount.as_deref()?;
    let new_paid = money::parse_stroops(raw_amount)?;
    let expected = money::parse_stroops(&payment.amount)?;
    let total_paid = already_paid_stroops + new_paid;
    let tx_hash = hp.transaction_hash.clone().unwrap_or_default();
    let paid_amount = money::stroops_to_string(total_paid);

    use std::cmp::Ordering;
    match total_paid.cmp(&expected) {
        Ordering::Equal => Some(Verdict::Completed { tx_hash, paid_amount }),
        Ordering::Greater => Some(Verdict::Overpaid { tx_hash, paid_amount }),
        Ordering::Less => Some(Verdict::Underpaid { tx_hash, paid_amount }),
    }
}

/// Compute the absolute difference between two amount strings as a display
/// string. Returns `None` if either value fails to parse (should never happen
/// for amounts we wrote ourselves).
fn delta_str(a: &str, b: &str) -> Option<String> {
    let va = money::parse_stroops(a)?;
    let vb = money::parse_stroops(b)?;
    Some(money::stroops_to_string((va - vb).abs()))
}

/// Fetch the most recent payments into `account` from Horizon, newest first,
/// with their transactions joined so memos are available.
pub async fn fetch_recent_payments(
    client: &reqwest::Client,
    horizon_url: &str,
    account: &str,
    limit: u32,
) -> anyhow::Result<Vec<HorizonPayment>> {
    let url = format!(
        "{}/accounts/{}/payments?order=desc&limit={}&join=transactions",
        horizon_url.trim_end_matches('/'),
        account,
        limit
    );
    let page: PaymentsPage = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(page.embedded.records)
}

/// Run one poll cycle: reconcile pending intents against recent on-chain
/// payments. Safe to call repeatedly; already-settled intents are ignored.
pub async fn poll_once(state: &Arc<AppState>) -> anyhow::Result<usize> {
    let pending = db::list_pending(&state.pool).await?;
    if pending.is_empty() {
        return Ok(0);
    }

    let payments = fetch_recent_payments(
        &state.http,
        &state.config.horizon_url,
        &state.config.gateway_public,
        200,
    )
    .await?;

    // Index pending intents by memo for O(1) lookup against on-chain records.
    let by_memo: HashMap<&str, &db::Payment> =
        pending.iter().map(|p| (p.memo.as_str(), p)).collect();

    let mut settled = 0;
    for hp in &payments {
        let Some(memo) = hp.memo() else { continue };
        let Some(payment) = by_memo.get(memo) else {
            continue;
        };

        // Skip transactions already recorded for this intent. This prevents
        // double-counting the original underpayment tx on subsequent poll cycles.
        let hp_hash = hp.transaction_hash.as_deref().unwrap_or("");
        if payment.tx_hash.as_deref() == Some(hp_hash) {
            continue;
        }

        // For underpaid intents, carry forward what has already been received.
        let already_paid_stroops = payment
            .paid_amount
            .as_deref()
            .and_then(money::parse_stroops)
            .unwrap_or(0);

        match verify(payment, hp, &state.config.usdc_issuer, already_paid_stroops) {
            Some(Verdict::Completed { tx_hash, paid_amount }) => {
                settle(state, payment, "completed", &tx_hash, &paid_amount, "payment.completed", None).await;
                settled += 1;
            }
            Some(Verdict::Overpaid { tx_hash, paid_amount }) => {
                let delta = delta_str(&paid_amount, &payment.amount);
                info!(
                    payment_id = %payment.id,
                    excess = %delta.as_deref().unwrap_or("?"),
                    "overpayment — intent completed, excess should be refunded"
                );
                settle(state, payment, "completed", &tx_hash, &paid_amount, "payment.overpaid", delta.as_deref()).await;
                settled += 1;
            }
            Some(Verdict::Underpaid { tx_hash, paid_amount }) => {
                let delta = delta_str(&payment.amount, &paid_amount);
                warn!(
                    payment_id = %payment.id,
                    expected = %payment.amount,
                    paid = %paid_amount,
                    remaining = %delta.as_deref().unwrap_or("?"),
                    "underpayment — intent remains open for a top-up"
                );
                settle(state, payment, "underpaid", &tx_hash, &paid_amount, "payment.underpaid", delta.as_deref()).await;
                settled += 1;
            }
            None => {}
        }
    }

    Ok(settled)
}

/// Persist a terminal or intermediate status for `payment` and fire its webhook.
async fn settle(
    state: &Arc<AppState>,
    payment: &db::Payment,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
    event: &str,
    delta: Option<&str>,
) {
    if let Err(e) =
        db::update_payment_status(&state.pool, &payment.id, status, tx_hash, paid_amount).await
    {
        warn!(payment_id = %payment.id, error = %e, "failed to update payment status");
        return;
    }
    info!(payment_id = %payment.id, status, %tx_hash, "payment settled");

    // Reflect the new state in the copy we hand to the webhook.
    let mut settled = payment.clone();
    settled.status = status.to_string();
    settled.tx_hash = Some(tx_hash.to_string());
    settled.paid_amount = Some(paid_amount.to_string());
    webhook::dispatch(state, &settled, event, delta).await;
}

/// Background loop that polls Horizon on the configured interval until the
/// process shuts down. Idles (without polling) while no gateway is configured.
pub async fn run_poller(state: Arc<AppState>) {
    if !state.config.gateway_configured() {
        warn!("STELLAR_GATEWAY_PUBLIC is unconfigured; Horizon poller disabled");
        return;
    }

    let interval = Duration::from_secs(state.config.poll_interval_secs.max(1));
    info!(
        account = %state.config.gateway_public,
        interval_secs = state.config.poll_interval_secs,
        "Horizon poller started"
    );

    loop {
        tokio::time::sleep(interval).await;
        match poll_once(&state).await {
            Ok(0) => debug!("poll: nothing to settle"),
            Ok(n) => info!(settled = n, "poll cycle settled payments"),
            Err(e) => warn!(error = %e, "poll cycle failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(asset: &str, amount: &str) -> db::Payment {
        db::Payment {
            id: "id-1".into(),
            merchant_id: "m".into(),
            destination_address: "GGATEWAY".into(),
            memo: "MEMO1234".into(),
            amount: amount.into(),
            asset: asset.into(),
            status: "pending".into(),
            webhook_url: None,
            tx_hash: None,
            paid_amount: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    fn native_payment(amount: &str, memo: &str, to: &str) -> HorizonPayment {
        HorizonPayment {
            kind: "payment".into(),
            amount: Some(amount.into()),
            asset_type: Some("native".into()),
            asset_code: None,
            asset_issuer: None,
            to: Some(to.into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some(memo.into()),
                memo_type: Some("text".into()),
            }),
        }
    }

    const USDC_ISSUER: &str = "GUSDC";

    #[test]
    fn exact_xlm_payment_completes() {
        let p = pending("XLM", "10.00");
        let hp = native_payment("10.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, USDC_ISSUER, 0),
            Some(Verdict::Completed {
                tx_hash: "TXHASH".into(),
                paid_amount: "10".into(),
            })
        );
    }

    #[test]
    fn overpayment_yields_overpaid_verdict() {
        let p = pending("XLM", "10");
        let hp = native_payment("12.5", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, USDC_ISSUER, 0),
            Some(Verdict::Overpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "12.5".into(),
            })
        );
    }

    #[test]
    fn underpayment_yields_underpaid_verdict() {
        let p = pending("XLM", "10");
        let hp = native_payment("9.9999999", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, USDC_ISSUER, 0),
            Some(Verdict::Underpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "9.9999999".into(),
            })
        );
    }

    #[test]
    fn topup_completing_underpaid_intent() {
        // First payment: 3 of 5 XLM — underpaid.
        let p = pending("XLM", "5");
        let hp1 = native_payment("3.0000000", "MEMO1234", "GGATEWAY");
        assert!(matches!(
            verify(&p, &hp1, USDC_ISSUER, 0),
            Some(Verdict::Underpaid { .. })
        ));

        // Top-up: 2 XLM arrives; cumulative = 5 = expected — completes exactly.
        let hp2 = native_payment("2.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp2, USDC_ISSUER, 30_000_000),
            Some(Verdict::Completed {
                tx_hash: "TXHASH".into(),
                paid_amount: "5".into(),
            })
        );
    }

    #[test]
    fn topup_overpaying_underpaid_intent() {
        // First payment: 3 of 5 XLM — underpaid.
        let p = pending("XLM", "5");
        // Top-up of 3 XLM; cumulative = 6 > 5 — overpaid.
        let hp = native_payment("3.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, USDC_ISSUER, 30_000_000),
            Some(Verdict::Overpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "6".into(),
            })
        );
    }

    #[test]
    fn wrong_memo_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "OTHER", "GGATEWAY");
        assert_eq!(verify(&p, &hp, USDC_ISSUER, 0), None);
    }

    #[test]
    fn wrong_destination_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "MEMO1234", "GSOMEONEELSE");
        assert_eq!(verify(&p, &hp, USDC_ISSUER, 0), None);
    }

    #[test]
    fn xlm_intent_rejects_usdc_payment() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.asset_type = Some("credit_alphanum4".into());
        hp.asset_code = Some("USDC".into());
        hp.asset_issuer = Some(USDC_ISSUER.into());
        assert_eq!(verify(&p, &hp, USDC_ISSUER, 0), None);
    }

    #[test]
    fn usdc_payment_with_correct_issuer_completes() {
        let p = pending("USDC", "5");
        let hp = HorizonPayment {
            kind: "payment".into(),
            amount: Some("5.0".into()),
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some(USDC_ISSUER.into()),
            to: Some("GGATEWAY".into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some("MEMO1234".into()),
                memo_type: Some("text".into()),
            }),
        };
        assert!(matches!(
            verify(&p, &hp, USDC_ISSUER, 0),
            Some(Verdict::Completed { .. })
        ));
    }

    #[test]
    fn usdc_payment_with_wrong_issuer_is_ignored() {
        let p = pending("USDC", "5");
        let mut hp = HorizonPayment {
            kind: "payment".into(),
            amount: Some("5.0".into()),
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some("GFAKEISSUER".into()),
            to: Some("GGATEWAY".into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some("MEMO1234".into()),
                memo_type: Some("text".into()),
            }),
        };
        assert_eq!(verify(&p, &hp, USDC_ISSUER, 0), None);
        // Sanity: with the right issuer it would have matched.
        hp.asset_issuer = Some(USDC_ISSUER.into());
        assert!(verify(&p, &hp, USDC_ISSUER, 0).is_some());
    }

    #[test]
    fn non_payment_operation_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.kind = "create_account".into();
        assert_eq!(verify(&p, &hp, USDC_ISSUER, 0), None);
    }

    #[test]
    fn deserializes_horizon_payments_page() {
        let body = r#"{
            "_embedded": { "records": [
                {
                    "type": "payment",
                    "amount": "10.0000000",
                    "asset_type": "native",
                    "to": "GGATEWAY",
                    "transaction_hash": "abc",
                    "transaction": { "memo": "MEMO1234", "memo_type": "text" }
                }
            ]}
        }"#;
        let page: PaymentsPage = serde_json::from_str(body).unwrap();
        assert_eq!(page.embedded.records.len(), 1);
        assert_eq!(page.embedded.records[0].memo(), Some("MEMO1234"));
    }
}
