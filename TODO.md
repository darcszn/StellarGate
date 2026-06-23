# TODO - Webhook decoupling + redrive worker

## Step 1: Inspect + implement DB support for redrive
- [ ] Add `db` queries to list/claim webhook_deliveries rows with `status in (pending, failed)` and `attempts < max`
- [ ] Ensure claiming prevents duplicate delivery attempts if the app is restarted while a redrive tick runs


## Step 2: Refactor webhook dispatch to be record-only
- [ ] Modify `src/webhook.rs::dispatch` to only:
  - build payload
  - insert a `webhook_deliveries` row with `status='pending'`, `attempts=0`
  - NOT perform HTTP

## Step 3: Add webhook redrive worker
- [ ] Implement worker loop in `src/webhook.rs` (or new module) that:
  - periodically selects pending/failed deliveries eligible by backoff
  - uses a `Semaphore` to cap concurrent HTTP attempts
  - updates row status to `delivered` or `failed` (terminal when attempts == max)

## Step 4: Wire worker startup
- [ ] Start the redrive worker from `src/main.rs`

## Step 5: Decouple reconciliation from delivery
- [x] Update `src/horizon.rs::settle` so it never awaits HTTP/webhook work (it records webhook delivery without waiting for HTTP)


## Step 6: Config additions
- [ ] Add environment/config knobs for:
  - redrive interval
  - concurrency
  - max attempts
  - backoff initial/max

## Step 7: Tests
- [ ] Add test for “settle/poller not blocked by slow webhook”
- [ ] Add test for “failed/pending delivery is retried after restart”

## Step 8: Run test suite
- [ ] `cargo test`

