//! In-process metrics: atomic counters and a latency histogram for webhook
//! delivery outcomes.
//!
//! All types are cheaply clonable (backed by `Arc`-wrapped atomics) so they
//! can be stored on `AppState` and shared across handlers and background tasks
//! without additional synchronisation.
//!
//! ## Exposition
//! `GET /metrics` returns a plain-text Prometheus-compatible snapshot so any
//! standard scraper can ingest the data with zero configuration.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Histogram buckets for webhook delivery latency (milliseconds).
/// Covers the range from sub-10 ms fast paths up to the 10 s default timeout.
const LATENCY_BUCKETS_MS: &[u64] = &[10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

#[derive(Clone)]
pub struct WebhookMetrics {
    inner: Arc<WebhookMetricsInner>,
}

struct WebhookMetricsInner {
    /// Deliveries that reached the endpoint and received a 2xx response.
    delivered: AtomicU64,
    /// Deliveries that exhausted all retry attempts without a success.
    failed: AtomicU64,
    /// Individual retry attempts (i.e. attempts after the first try).
    retried: AtomicU64,
    /// Sum of all delivery latencies in milliseconds (for computing mean).
    latency_sum_ms: AtomicU64,
    /// Total completed delivery attempts (for mean denominator).
    latency_count: AtomicU64,
    /// Per-bucket counts. Index `i` corresponds to `LATENCY_BUCKETS_MS[i]`;
    /// the last slot is the `+Inf` bucket.
    latency_buckets: [AtomicU64; 10],
}

impl Default for WebhookMetricsInner {
    fn default() -> Self {
        Self {
            delivered: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retried: AtomicU64::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            // 9 explicit buckets + 1 +Inf = 10 slots
            latency_buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl WebhookMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WebhookMetricsInner::default()),
        }
    }

    /// Record a successful delivery (2xx response received).
    pub fn record_delivered(&self) {
        self.inner.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a final delivery failure (all retries exhausted without success).
    pub fn record_failed(&self) {
        self.inner.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one retry attempt (every attempt after the first try).
    pub fn record_retry(&self) {
        self.inner.retried.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the end-to-end latency for one delivery, in milliseconds.
    ///
    /// Histogram buckets are cumulative: a 75 ms observation increments every
    /// bucket whose `le` bound is ≥ 75 (i.e. `le="100"`, `le="250"`, …
    /// `le="+Inf"`), matching the Prometheus exposition format.
    pub fn record_latency_ms(&self, ms: u64) {
        self.inner.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.inner.latency_count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= bound {
                self.inner.latency_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        // +Inf bucket is always incremented.
        self.inner.latency_buckets[LATENCY_BUCKETS_MS.len()].fetch_add(1, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn delivered(&self) -> u64 {
        self.inner.delivered.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> u64 {
        self.inner.failed.load(Ordering::Relaxed)
    }
    pub fn retried(&self) -> u64 {
        self.inner.retried.load(Ordering::Relaxed)
    }
    pub fn latency_sum_ms(&self) -> u64 {
        self.inner.latency_sum_ms.load(Ordering::Relaxed)
    }
    pub fn latency_count(&self) -> u64 {
        self.inner.latency_count.load(Ordering::Relaxed)
    }
    pub fn latency_bucket(&self, i: usize) -> u64 {
        self.inner.latency_buckets[i].load(Ordering::Relaxed)
    }
}

impl Default for WebhookMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ── Prometheus text exposition ────────────────────────────────────────────────

/// Render webhook delivery metrics as a Prometheus-compatible plain-text snapshot.
/// Called by `GET /metrics`.
pub fn render(webhook: &WebhookMetrics) -> String {
    let mut out = String::with_capacity(1024);

    // stellargate_webhook_deliveries_total — counter vec by outcome
    out.push_str("# HELP stellargate_webhook_deliveries_total Total webhook delivery attempts by outcome.\n");
    out.push_str("# TYPE stellargate_webhook_deliveries_total counter\n");
    out.push_str(&format!(
        "stellargate_webhook_deliveries_total{{outcome=\"delivered\"}} {}\n",
        webhook.delivered()
    ));
    out.push_str(&format!(
        "stellargate_webhook_deliveries_total{{outcome=\"failed\"}} {}\n",
        webhook.failed()
    ));

    // stellargate_webhook_retries_total — counter
    out.push_str("# HELP stellargate_webhook_retries_total Total webhook retry attempts (excludes the first try).\n");
    out.push_str("# TYPE stellargate_webhook_retries_total counter\n");
    out.push_str(&format!(
        "stellargate_webhook_retries_total {}\n",
        webhook.retried()
    ));

    // stellargate_webhook_delivery_latency_ms — histogram
    out.push_str("# HELP stellargate_webhook_delivery_latency_ms End-to-end webhook delivery latency in milliseconds.\n");
    out.push_str("# TYPE stellargate_webhook_delivery_latency_ms histogram\n");
    for (i, &bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
        out.push_str(&format!(
            "stellargate_webhook_delivery_latency_ms_bucket{{le=\"{}\"}} {}\n",
            bound,
            webhook.latency_bucket(i)
        ));
    }
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_bucket{{le=\"+Inf\"}} {}\n",
        webhook.latency_bucket(LATENCY_BUCKETS_MS.len())
    ));
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_sum {}\n",
        webhook.latency_sum_ms()
    ));
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_count {}\n",
        webhook.latency_count()
    ));

    out
}
