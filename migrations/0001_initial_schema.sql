CREATE TABLE IF NOT EXISTS payments (
    id TEXT PRIMARY KEY,
    merchant_id TEXT NOT NULL DEFAULT 'anonymous',
    destination_address TEXT NOT NULL,
    memo TEXT NOT NULL UNIQUE,
    amount TEXT NOT NULL,
    asset TEXT NOT NULL DEFAULT 'XLM',
    status TEXT NOT NULL DEFAULT 'pending',
    webhook_url TEXT,
    tx_hash TEXT,
    paid_amount TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+1 hour'))
);

CREATE INDEX IF NOT EXISTS idx_payments_memo ON payments(memo);
CREATE INDEX IF NOT EXISTS idx_payments_status ON payments(status);
CREATE INDEX IF NOT EXISTS idx_payments_created_id ON payments(created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id TEXT PRIMARY KEY,
    payment_id TEXT NOT NULL,
    url TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    last_attempt TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Durable key/value state — used by the Horizon poller to persist its
-- paging cursor so it resumes exactly where it left off across restarts.
CREATE TABLE IF NOT EXISTS kv_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Idempotency keys for payment creation. A key is unique per merchant and
-- maps to the payment id minted for the first request that used it.
CREATE TABLE IF NOT EXISTS idempotency_keys (
    merchant_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (merchant_id, idempotency_key)
);
