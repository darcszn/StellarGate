# StellarGate

[![CI](https://github.com/StellarGateLabs/StellarGate/actions/workflows/ci.yml/badge.svg)](https://github.com/StellarGateLabs/StellarGate/actions/workflows/ci.yml)

A developer-friendly payment gateway API built on [Stellar](https://stellar.org) for accepting, verifying, and managing payments in XLM and USDC.

> Think Stripe — but powered by the Stellar blockchain instead of banks.

## Overview

StellarGate abstracts Stellar payments into a simple REST API. Developers can create payment intents, receive a destination address and memo, and get notified when payment is confirmed on-chain.

```
Client App → POST /payments → get address + memo
User pays via Stellar wallet (e.g. Lobstr)
StellarGate detects transaction on Horizon
Payment marked complete → webhook fired to your app
```

## Current Status

This project is under active development. The following is implemented:

- [x] `POST /payments` — create a payment intent
- [x] `GET /payments/:id` — query payment status
- [x] `GET /payments` — list & filter payments (pagination)
- [x] `GET /health` — health check
- [x] SQLite persistence
- [x] Input validation (asset, amount as exact stroops, webhook URL)
- [x] Transaction listener (Horizon SSE streaming + interval polling)
- [x] Payment verification (memo + asset + amount)
- [x] Webhook dispatch (timestamped HMAC-SHA256 signature, replay-resistant, with retries)
- [x] Multi-merchant support (`merchant_id` per payment)
- [x] Pending-intent expiry (configurable TTL + `payment.expired` webhook)
- [ ] Horizon streaming (currently polled on an interval)
- [ ] Dashboard UI

## Tech Stack

- **Language:** Rust
- **HTTP Framework:** [axum](https://github.com/tokio-rs/axum)
- **Database:** SQLite via [sqlx](https://github.com/launchbadge/sqlx)
- **Async Runtime:** [tokio](https://tokio.rs)
- **Blockchain:** [Stellar Horizon API](https://developers.stellar.org/api)

## Getting Started

### Prerequisites

- Rust 1.75+ — [install via rustup](https://rustup.rs)

### Setup

```bash
git clone https://github.com/StellarGateLabs/StellarGate.git
cd StellarGate

cp .env.example .env
# Edit .env with your Stellar keys
```

### Environment Variables

| Variable | Description | Default |
|---|---|---|
| `PORT` | HTTP port | `3000` |
| `DATABASE_URL` | sqlx connection string | `sqlite:stellargate.db` |
| `STELLAR_NETWORK` | `testnet` or `public` | `testnet` |
| `STELLAR_HORIZON_URL` | Horizon endpoint | testnet |
| `STELLAR_GATEWAY_PUBLIC` | Your gateway wallet public key (`G...`). Validated as a Stellar strkey at startup; an invalid value aborts boot. | — |
| `STELLAR_GATEWAY_SECRET` | Your gateway wallet secret key | — |
| `ACCEPTED_ASSETS` | Comma-separated assets to accept. Format: `CODE` for native (e.g. `XLM`) or `CODE:ISSUER` for non-native (e.g. `USDC:GISSUER`). Adding an asset is config-only — no code changes needed. Each `ISSUER` is validated as a Stellar strkey at startup. | `XLM,USDC:<testnet-issuer>` |
| `STELLAR_LISTENER_MODE` | `stream` (SSE + poller reconciler) or `poll` (interval only) | `stream` |
| `POLL_INTERVAL_SECS` | How often the Horizon poller reconciles | `10` |
| `PAYMENT_TTL_SECS` | How long a payment intent stays `pending` before it is expired (from `created_at`) | `3600` |
| `WEBHOOK_SECRET` | HMAC signing secret for webhooks | — |
| `WEBHOOK_RETRY_ATTEMPTS` | Webhook delivery attempts | `3` |
| `WEBHOOK_RETRY_DELAY_MS` | Delay between webhook retries | `5000` |
| `WEBHOOK_ALLOW_PRIVATE_TARGETS` | Bypasses the SSRF guard's loopback/link-local/private/reserved IP check on `webhook_url` (still requires http(s) and a resolvable host). For local development and tests only — never enable in production. | `false` |
| `CORS_ALLOWED_ORIGINS` | Comma-separated allowed CORS origins (e.g. `https://app.example.com`). Required on `public` network; omitting on testnet falls back to permissive with a warning. | _(unset — permissive on testnet)_ |
| `RATE_LIMIT_REQUESTS_PER_SEC` | Rate limit for `POST /payments` and `POST /merchants` (requests per second per IP, tracked independently per route) | `10` |
| `DB_POOL_MAX_CONNECTIONS` | SQLite connection pool size. WAL mode allows one writer + many concurrent readers. | `10` |
| `DB_BUSY_TIMEOUT_MS` | How long (ms) SQLite waits to acquire a write lock before returning an error. Must be `> 0` under concurrent load. | `5000` |
| `ADMIN_PROVISIONING_SECRET` | Shared secret required via the `X-Admin-Secret` header to call `POST /merchants`. Unset disables provisioning entirely (every request gets `401`). | _(unset — provisioning disabled)_ |

> `DATABASE_URL` is a sqlx connection string (`sqlite:stellargate.db`), not a
> file path. The Horizon poller stays idle until `STELLAR_GATEWAY_PUBLIC` is set.
> The poller pages forward through payments from a cursor persisted in the
> database, so it never misses an intent regardless of on-chain volume and
> resumes from where it left off after a restart.

### Run

```bash
cargo run
```

### Docker Compose

The quickest way to run StellarGate without installing Rust:

```bash
cp .env.example .env
# Edit .env with your Stellar keys, then:
docker compose up --build
```

The API will be available at `http://localhost:3000`. The SQLite database is
stored in a named Docker volume (`stellargate_data`) so it persists across
container restarts. Verify the service is healthy:

```bash
curl http://localhost:3000/health
# {"status":"ok"}
```

To stop and remove containers while keeping the database volume:

```bash
docker compose down
```

### Test

```bash
cargo test
```

Tests cover amount/stroops handling, Horizon payment verification, webhook
signing, and the HTTP API (create, fetch, list/filter, validation).

## API Reference

### `POST /merchants`

Provision a new merchant and return its API key. This is an **admin-only**
route: set `ADMIN_PROVISIONING_SECRET` and send it via the `X-Admin-Secret`
header, or the endpoint always returns `401`. There is no self-service
sign-up — provisioning is intended to be run by whoever operates the gateway
(e.g. an internal admin tool or a one-off `curl` from a trusted machine), not
exposed to end users.

**Request**

```bash
curl -X POST http://localhost:3000/merchants \
  -H "X-Admin-Secret: $ADMIN_PROVISIONING_SECRET"
```

**Response** `201 Created`
```json
{
  "merchant_id": "a1b2c3d4-...",
  "api_key": "e5f6...-...-..."
}
```

> `api_key` is returned once, in plaintext, and never shown again — store it
> securely. Use it as the `Authorization: Bearer <api_key>` header on
> `POST /payments` and `GET /payments`.

---

### `POST /payments`

Create a new payment intent.

**Request**
```json
{
  "amount": "10.00",
  "asset": "XLM",
  "merchant_id": "your-merchant-id",
  "webhook_url": "https://yourapp.com/webhooks/stellar"
}
```

| Field | Type | Required | Values |
|---|---|---|---|
| `amount` | string | ✅ | Any positive number |
| `asset` | string | ✅ | `XLM` or `USDC` |
| `merchant_id` | string | ❌ | Any string |
| `webhook_url` | string | ❌ | Valid HTTPS URL (HTTP permitted only in testnet/development) |

> `webhook_url` is checked against an SSRF guard: its host is resolved and
> rejected if it's loopback, link-local (including the cloud metadata address
> `169.254.169.254`), private, or otherwise reserved. The same check runs
> again on every redelivery (`POST /payments/:id/webhooks/:delivery_id/redeliver`)
> against the exact address resolved, not a second DNS lookup, so a
> DNS-rebinding attempt after the initial check can't reach an internal host.

**Headers**

| Header | Required | Description |
|---|---|---|
| `Idempotency-Key` | ❌ | Opaque client-chosen key for safe retries. Reusing a key (scoped per `merchant_id`) returns the original payment with `200 OK` instead of minting a duplicate intent. |

**Response** `201 Created` (or `200 OK` when an `Idempotency-Key` matches a prior request)
```json
{
  "id": "a1b2c3d4-...",
  "destination_address": "GBBD47IF6LWK7P7...",
  "memo": "A1B2C3D4",
  "amount": "10.00",
  "asset": "XLM",
  "status": "pending",
  "created_at": "2026-04-29T15:00:00",
  "expires_at": "2026-04-29T16:00:00"
}
```

> The user must send exactly `amount` of `asset` to `destination_address` with `memo` set as the transaction memo. The intent expires at `expires_at` (default one hour after creation) if unpaid.

---

### `GET /payments/:id`

Fetch the current status of a payment.

**Response** `200 OK`
```json
{
  "id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "destination_address": "GBBD47IF6LWK7P7...",
  "memo": "A1B2C3D4",
  "amount": "10.00",
  "asset": "XLM",
  "status": "pending",
  "tx_hash": null,
  "paid_amount": null,
  "created_at": "2026-04-29T15:00:00",
  "updated_at": "2026-04-29T15:00:00",
  "expires_at": "2026-04-29T16:00:00"
}
```

**Status values**

| Status | Meaning |
|---|---|
| `pending` | Awaiting payment |
| `completed` | Payment confirmed on-chain |
| `failed` | Partial payment or verification failed |
| `expired` | TTL elapsed before payment arrived; no longer watched |

---

### `GET /payments`

List payments, newest first.

**Query parameters**

| Param | Description | Default |
|---|---|---|
| `status` | Filter by `pending`, `completed`, `failed`, or `expired` | all |
| `limit` | Page size (1–100) | `20` |
| `offset` | Rows to skip | `0` |

**Response** `200 OK`
```json
{
  "total": 42,
  "limit": 20,
  "offset": 0,
  "payments": [ { "id": "...", "status": "pending", "...": "..." } ]
}
```

---

### `GET /health`

Cheap liveness probe. Always returns `200 OK` as long as the process is running.

```json
200 OK — { "status": "ok" }
```

### `GET /ready`

Readiness probe. Runs `SELECT 1` against the database; returns `503` when unreachable.

```json
200 OK          — { "status": "ok" }
503 Unavailable — { "status": "unavailable" }
```

## Payment Flow

```
1. Developer calls POST /payments
2. StellarGate returns { destination_address, memo, amount }
3. End user sends payment via any Stellar wallet
4. StellarGate listener detects the transaction on Horizon (SSE stream, ~1s; poller as fallback)
5. Verifies: correct memo + amount + asset
6. Updates payment status and fires a webhook event
```

## Payment Resolution Policy

Every on-chain payment matched by memo, destination, and asset is resolved as follows:

| Scenario | `status` | Webhook event | `delta` field |
|---|---|---|---|
| Paid exactly the requested amount | `completed` | `payment.completed` | not present |
| Paid **more** than requested | `completed` | `payment.overpaid` | excess amount (should be refunded) |
| Paid **less** than requested | `underpaid` | `payment.underpaid` | shortfall still owed |
| Top-up brings cumulative total to exactly expected | `completed` | `payment.completed` | not present |
| Top-up brings cumulative total above expected | `completed` | `payment.overpaid` | cumulative excess |

**Overpayment:** The intent is fulfilled and moves to `completed`. The `payment.overpaid` event includes a `delta` field showing the excess amount the merchant should consider refunding to the sender.

**Underpayment:** The intent moves to `underpaid` and remains watchable. StellarGate continues polling for a follow-up payment to the same memo. When the cumulative total meets or exceeds the requested amount, the intent completes normally.

**Top-up limitation:** Only a single follow-up payment is tracked per underpaid intent. If multiple partial payments are needed, the sender should consolidate them — send the full remaining shortfall (shown in `delta`) in one transaction.

**Post-completion payments:** Once an intent reaches `completed`, any further on-chain payments to the same address and memo are not tracked and will not trigger additional webhooks.

## Webhook Events

### `payment.completed`

Fired when the cumulative received amount equals the requested amount exactly.

```json
{
  "event": "payment.completed",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123...",
  "amount": "10.00",
  "paid_amount": "10",
  "asset": "XLM",
  "status": "completed"
}
```

### `payment.overpaid`

Fired when the cumulative received amount exceeds the requested amount. `delta` is the excess the merchant should refund.

```json
{
  "event": "payment.overpaid",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123...",
  "amount": "10.00",
  "paid_amount": "12.5",
  "asset": "XLM",
  "status": "completed",
  "delta": "2.5"
}
```

### `payment.underpaid`

Fired when a payment is received but falls short of the requested amount. `delta` is the remaining shortfall. The intent stays open for a top-up.

```json
{
  "event": "payment.underpaid",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123...",
  "amount": "10.00",
  "paid_amount": "7",
  "asset": "XLM",
  "status": "underpaid",
  "delta": "3"
}
```

Event types: `payment.success` (paid in full), `payment.failed` (underpaid or
verification failed), and `payment.expired` (the intent's TTL elapsed before
payment arrived). The `event` field carries the type; `status` carries the
matching payment status.

### Verifying webhooks

Every webhook request carries two headers:

| Header | Value |
|---|---|
| `X-StellarGate-Timestamp` | Unix time (seconds) at which the event was signed |
| `X-StellarGate-Signature` | Hex HMAC-SHA256 of `"{timestamp}.{raw_body}"`, keyed with your `WEBHOOK_SECRET` |

The signature covers the timestamp as well as the body (Stripe-style), so a
captured request cannot be replayed indefinitely. To verify:

1. Read `X-StellarGate-Timestamp` (`t`) and `X-StellarGate-Signature` (`sig`).
2. Reject the request if `t` is too old: `abs(now - t) > tolerance`. A
   **5-minute** tolerance is recommended — large enough for clock skew and
   network delay, small enough to bound the replay window.
3. Concatenate `"{t}.{raw_body}"` using the **exact bytes** received (verify
   before any JSON re-encoding, which would change the bytes).
4. Compute `HMAC_SHA256(WEBHOOK_SECRET, "{t}.{raw_body}")` and hex-encode it.
5. Compare it to `sig` with a **constant-time** equality check. Reject on
   mismatch.

Example (Node.js):

```js
const crypto = require("crypto");

function verify(rawBody, headers, secret, toleranceSec = 300) {
  const t = Number(headers["x-stellargate-timestamp"]);
  const sig = headers["x-stellargate-signature"];
  if (!Number.isFinite(t) || Math.abs(Date.now() / 1000 - t) > toleranceSec) {
    return false; // stale or missing timestamp — reject
  }
  const expected = crypto
    .createHmac("sha256", secret)
    .update(`${t}.${rawBody}`)
    .digest("hex");
  return crypto.timingSafeEqual(Buffer.from(sig), Buffer.from(expected));
}
```

## Project Structure

```
migrations/
└── 0001_initial_schema.sql   # Versioned schema applied automatically on startup

src/
├── main.rs          # Entry point, server startup, listener/poller spawn, graceful shutdown
├── lib.rs           # Shared state and module exports
├── config.rs        # Environment configuration
├── db.rs            # Database queries (SQLite)
├── money.rs         # Stroops-based amount parsing/validation
├── strkey.rs        # Stellar address (strkey) validation
├── horizon.rs       # Horizon polling listener + payment verification
├── expiry.rs        # Background sweeper that expires overdue pending intents
├── webhook.rs       # HMAC-SHA256 signed webhook dispatch
└── api/
    ├── mod.rs       # Axum router, layers (CORS/trace/body-limit), 404 fallback
    └── payments.rs  # Payment handlers (create, get, list)

tests/
└── api_tests.rs     # Integration tests
```

## Database Migrations

Schema is managed with [`sqlx::migrate!`](https://docs.rs/sqlx/latest/sqlx/macro.migrate.html). Migrations live in `migrations/` as numbered SQL files and are applied automatically on startup — both a fresh database and an existing one converge to the same schema.

**Adding a migration:**

1. Create `migrations/<next_number>_<short_description>.sql` (e.g. `0002_add_refunds_table.sql`).
2. Write your `ALTER TABLE` / `CREATE TABLE` SQL in the file.
3. Run `cargo test` — the test suite boots against an in-memory database and will apply all migrations, catching syntax errors early.

sqlx records applied migrations in a `_sqlx_migrations` table so each file is run exactly once.

## Contributing

This project is open to contributors. See the [Wave Program](https://github.com/StellarGateLabs/StellarGate/issues) for scoped issues you can pick up.

**To contribute:**

1. Fork the repo
2. Create a branch: `git checkout -b feat/your-feature`
3. Make your changes and add tests
4. Run `cargo test` — all tests must pass
5. Open a pull request

## License

MIT
