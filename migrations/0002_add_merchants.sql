-- Merchants are provisioned via POST /merchants.
-- api_key_hash stores SHA-256(raw_key) in hex so the plaintext key is never
-- persisted; the raw key is returned once at creation time and never again.
CREATE TABLE IF NOT EXISTS merchants (
    id TEXT PRIMARY KEY,
    api_key_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
