# StellarGate

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
- [x] Transaction listener (Horizon polling)
- [x] Payment verification (memo + asset + amount)
- [x] Webhook dispatch (HMAC-SHA256 signed, with retries)
- [x] Multi-merchant support (`merchant_id` per payment)
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
| `STELLAR_GATEWAY_PUBLIC` | Your gateway wallet public key | — |
| `STELLAR_GATEWAY_SECRET` | Your gateway wallet secret key | — |
| `USDC_ISSUER` | USDC issuer address | testnet issuer |
| `POLL_INTERVAL_SECS` | How often the Horizon poller reconciles | `10` |
| `WEBHOOK_SECRET` | HMAC signing secret for webhooks | — |
| `WEBHOOK_RETRY_ATTEMPTS` | Webhook delivery attempts | `3` |
| `WEBHOOK_RETRY_DELAY_MS` | Delay between webhook retries | `5000` |
| `CORS_ALLOWED_ORIGINS` | Comma-separated allowed CORS origins (e.g. `https://app.example.com`). Required on `public` network; omitting on testnet falls back to permissive with a warning. | _(unset — permissive on testnet)_ |

> `DATABASE_URL` is a sqlx connection string (`sqlite:stellargate.db`), not a
> file path. The Horizon poller stays idle until `STELLAR_GATEWAY_PUBLIC` is set.

### Run

```bash
cargo run
```

### Test

```bash
cargo test
```

Tests cover amount/stroops handling, Horizon payment verification, webhook
signing, and the HTTP API (create, fetch, list/filter, validation).

## API Reference

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
| `webhook_url` | string | ❌ | Valid HTTPS URL |

**Response** `201 Created`
```json
{
  "id": "a1b2c3d4-...",
  "destination_address": "GBBD47IF6LWK7P7...",
  "memo": "A1B2C3D4",
  "amount": "10.00",
  "asset": "XLM",
  "status": "pending",
  "created_at": "2026-04-29T15:00:00"
}
```

> The user must send exactly `amount` of `asset` to `destination_address` with `memo` set as the transaction memo.

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
  "updated_at": "2026-04-29T15:00:00"
}
```

**Status values**

| Status | Meaning |
|---|---|
| `pending` | Awaiting payment |
| `completed` | Payment confirmed on-chain |
| `failed` | Partial payment or verification failed |

---

### `GET /payments`

List payments, newest first.

**Query parameters**

| Param | Description | Default |
|---|---|---|
| `status` | Filter by `pending`, `completed`, or `failed` | all |
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

```json
200 OK — { "status": "ok" }
```

## Payment Flow

```
1. Developer calls POST /payments
2. StellarGate returns { destination_address, memo, amount }
3. End user sends payment via any Stellar wallet
4. StellarGate listener detects the transaction on Horizon
5. Verifies: correct memo + amount + asset
6. Updates payment status to "completed"
7. POSTs webhook event to developer's webhook_url
```

## Webhook Events

```json
{
  "event": "payment.success",
  "payment_id": "a1b2c3d4-...",
  "tx_hash": "abc123...",
  "amount": "10.00",
  "paid_amount": "10.00",
  "asset": "XLM"
}
```

Webhooks are signed with `X-StellarGate-Signature` (HMAC-SHA256) so you can verify authenticity.

## Project Structure

```
src/
├── main.rs          # Entry point, server startup, poller spawn, graceful shutdown
├── lib.rs           # Shared state and module exports
├── config.rs        # Environment configuration
├── db.rs            # Database queries (SQLite)
├── money.rs         # Stroops-based amount parsing/validation
├── horizon.rs       # Horizon polling listener + payment verification
├── webhook.rs       # HMAC-SHA256 signed webhook dispatch
└── api/
    ├── mod.rs       # Axum router, layers (CORS/trace/body-limit), 404 fallback
    └── payments.rs  # Payment handlers (create, get, list)

tests/
└── api_tests.rs     # Integration tests
```

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
