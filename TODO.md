# TODO - Webhook decoupling + redrive worker

## Step 1: Inspect + implement DB support for redrive
- [x] Add `db` queries to list/claim webhook_deliveries rows with `status in (pending, failed)` and `attempts < max`
  (`db::list_redrivable_deliveries`)
- [x] Ensure claiming prevents duplicate delivery attempts if the app is restarted while a redrive tick runs
  (a single sequential worker loop never has two ticks in flight at once; the `grace_secs` idle window keeps
  the worker from ever picking up a row a still-running inline `dispatch()` hasn't finished with yet)


## Step 2: Refactor webhook dispatch to be record-only
- [ ] Modify `src/webhook.rs::dispatch` to only:
  - build payload
  - insert a `webhook_deliveries` row with `status='pending'`, `attempts=0`
  - NOT perform HTTP

  Deliberately not done: `dispatch()` still delivers inline so the common case settles and notifies in one
  pass with no added latency. The redrive worker (Steps 1/3/4) is a safety net on top of that for the crash
  case, not a replacement for it — rewriting dispatch to be record-only would be a bigger, riskier change
  than issue #156 asked for and would break the existing synchronous-delivery test coverage.

## Step 3: Add webhook redrive worker
- [x] Implement worker loop in `src/webhook.rs` (or new module) that:
  - periodically selects pending/failed deliveries eligible by backoff
  - uses a `Semaphore` to cap concurrent HTTP attempts
  - updates row status to `delivered` or `failed` (terminal when attempts == max)
  (`webhook::redrive_once` / `webhook::run_redrive_worker`)

## Step 4: Wire worker startup
- [x] Start the redrive worker from `src/main.rs`

## Step 5: Decouple reconciliation from delivery
- [x] Update `src/horizon.rs::settle` so it never awaits HTTP/webhook work (it records webhook delivery without waiting for HTTP)


## Step 6: Config additions
- [x] Add environment/config knobs for:
  - redrive interval (`WEBHOOK_REDRIVE_INTERVAL_SECS`)
  - concurrency (`WEBHOOK_REDRIVE_CONCURRENCY`)
  - max attempts (`WEBHOOK_REDRIVE_MAX_ATTEMPTS`)
  - backoff initial/max (`WEBHOOK_REDRIVE_GRACE_SECS` — the idle window before a stuck row is retried)

## Step 7: Tests
- [x] Add test for “settle/poller not blocked by slow webhook”
  (`tests/webhook_dispatch_tests.rs::settle_not_blocked_by_slow_webhook`)
- [x] Add test for “failed/pending delivery is retried after restart”
  (`tests/webhook_dispatch_tests.rs::pending_delivery_is_redriven_after_restart`, plus
  `failed_delivery_below_max_attempts_is_retried_by_redrive`,
  `redrive_skips_deliveries_still_within_grace_window`, and
  `redrive_marks_delivery_failed_after_exhausting_max_attempts`)
- [x] Add expiry-webhook e2e test (wiremock)
  (`src/expiry.rs::tests::sweep_fires_payment_expired_webhook_end_to_end`)

## Step 8: Run test suite
- [x] `cargo test`

