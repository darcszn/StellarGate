use anyhow::Result;
use sqlx::{Pool, Row, Sqlite};

pub type Db = Pool<Sqlite>;

/// Normalize a raw SQLite timestamp to strict RFC 3339 UTC with a Z suffix.
///
/// Handles both legacy rows (`"2026-04-29 15:00:00"` / `"2026-04-29T15:00:00"`)
/// and already-correct rows (`"2026-04-29T15:00:00Z"`). Any value that doesn't
/// look like a 19-character datetime is returned unchanged so we never silently
/// corrupt unexpected data.
fn normalize_ts(raw: &str) -> String {
    let s = raw.trim();
    // Already has an explicit offset/Z — nothing to do.
    if s.ends_with('Z') || s.contains('+') {
        return s.to_string();
    }
    // Replace the space separator with T if present, then append Z.
    if s.len() == 19 {
        let with_t = s.replacen(' ', "T", 1);
        return format!("{with_t}Z");
    }
    s.to_string()
}

pub async fn migrate(pool: &Db) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS payments (
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
        )",
    )
    .execute(pool)
    .await?;

    /* Bring pre-existing payment tables up to schema. New databases already have
    `expires_at` from the CREATE TABLE above; older ones need it added in
    place. SQLite rejects a non-constant DEFAULT on ALTER ... ADD COLUMN, so we
    add it nullable and backfill below. */
    let has_expires_at: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'expires_at'",
    )
    .fetch_one(pool)
    .await?;
    if has_expires_at == 0 {
        sqlx::query("ALTER TABLE payments ADD COLUMN expires_at TEXT")
            .execute(pool)
            .await?;
    }
    /* Backfill any row without an expiry (legacy rows, or rows inserted in the
    brief window before the column existed). `created_at + 1h` mirrors the
    default TTL; SQLite's date functions accept the stored RFC 3339 `Z` form. */
    sqlx::query(
        "UPDATE payments
            SET expires_at = strftime('%Y-%m-%dT%H:%M:%SZ', created_at, '+1 hour')
          WHERE expires_at IS NULL",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_memo ON payments(memo)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_status ON payments(status)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_payments_created_id ON payments(created_at DESC, id DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webhook_deliveries (
            id TEXT PRIMARY KEY,
            payment_id TEXT NOT NULL,
            url TEXT NOT NULL,
            payload TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_attempt TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* Durable key/value state — used by the Horizon poller to persist its
    paging cursor so it resumes exactly where it left off across restarts. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS kv_state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* Merchants are provisioned via POST /merchants. The raw API key is never
    stored; only its SHA-256 hex digest is persisted so a DB breach does not
    expose live credentials. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS merchants (
            id TEXT PRIMARY KEY,
            api_key_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* Idempotency keys for payment creation. A key is unique per merchant and
    maps to the payment id minted for the first request that used it, so a
    client retrying after a network blip gets the original payment back
    instead of a duplicate intent. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS idempotency_keys (
            merchant_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            payment_id TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            PRIMARY KEY (merchant_id, idempotency_key)
        )",
    )
    .execute(pool)
    .await?;

    /* Normalise legacy rows that were written by the old datetime('now') default,
    which produced "YYYY-MM-DD HH:MM:SS" (space, no Z). Safe to run on every
    startup — the WHERE clause skips rows that are already RFC 3339. */
    for tbl_col in [
        ("payments", "created_at"),
        ("payments", "updated_at"),
        ("webhook_deliveries", "created_at"),
    ] {
        let sql = format!(
            "UPDATE {} SET {col} = replace({col}, ' ', 'T') || 'Z' WHERE {col} NOT LIKE '%T%'",
            tbl_col.0,
            col = tbl_col.1
        );
        sqlx::query(&sql).execute(pool).await?;
    }

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Payment {
    pub id: String,
    pub merchant_id: String,
    pub destination_address: String,
    pub memo: String,
    pub amount: String,
    pub asset: String,
    pub status: String,
    pub webhook_url: Option<String>,
    pub tx_hash: Option<String>,
    pub paid_amount: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// When this intent stops being `pending` and is swept to `expired`.
    pub expires_at: String,
}

fn row_to_payment(row: &sqlx::sqlite::SqliteRow) -> Payment {
    Payment {
        id: row.get("id"),
        merchant_id: row.get("merchant_id"),
        destination_address: row.get("destination_address"),
        memo: row.get("memo"),
        amount: row.get("amount"),
        asset: row.get("asset"),
        status: row.get("status"),
        webhook_url: row.get("webhook_url"),
        tx_hash: row.get("tx_hash"),
        paid_amount: row.get("paid_amount"),
        created_at: normalize_ts(&row.get::<String, _>("created_at")),
        updated_at: normalize_ts(&row.get::<String, _>("updated_at")),
        expires_at: normalize_ts(&row.get::<String, _>("expires_at")),
    }
}

/// Fields needed to insert a new payment intent.
pub struct NewPayment<'a> {
    pub id: &'a str,
    pub merchant_id: &'a str,
    pub destination_address: &'a str,
    pub memo: &'a str,
    pub amount: &'a str,
    pub asset: &'a str,
    pub webhook_url: Option<&'a str>,
    /// Seconds from now until the intent expires. The expiry timestamp is
    /// computed by SQLite at insert time as `now + ttl_secs`.
    pub ttl_secs: i64,
}

pub async fn create_payment(pool: &Db, new: NewPayment<'_>) -> Result<Payment> {
    /* Compute the expiry as `now + ttl_secs` in SQLite so it shares the exact
    clock and RFC 3339 format as created_at. */
    let ttl_modifier = format!("{:+} seconds", new.ttl_secs);
    sqlx::query(
        "INSERT INTO payments (id, merchant_id, destination_address, memo, amount, asset, webhook_url, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now',?))",
    )
    .bind(new.id)
    .bind(new.merchant_id)
    .bind(new.destination_address)
    .bind(new.memo)
    .bind(new.amount)
    .bind(new.asset)
    .bind(new.webhook_url)
    .bind(&ttl_modifier)
    .execute(pool)
    .await?;

    get_payment(pool, new.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Payment not found after insert"))
}

/// Look up the payment id previously minted for `(merchant_id, key)`, if any.
pub async fn find_payment_id_by_idempotency_key(
    pool: &Db,
    merchant_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT payment_id FROM idempotency_keys WHERE merchant_id = ? AND idempotency_key = ?",
    )
    .bind(merchant_id)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Record the payment id minted for `(merchant_id, key)`. If the key already
/// exists (e.g. a concurrent request won the race), the existing mapping is left
/// untouched and the winning payment id is returned; otherwise `payment_id` is
/// stored and returned.
pub async fn save_idempotency_key(
    pool: &Db,
    merchant_id: &str,
    key: &str,
    payment_id: &str,
) -> Result<String> {
    sqlx::query(
        "INSERT INTO idempotency_keys (merchant_id, idempotency_key, payment_id)
         VALUES (?, ?, ?)
         ON CONFLICT(merchant_id, idempotency_key) DO NOTHING",
    )
    .bind(merchant_id)
    .bind(key)
    .bind(payment_id)
    .execute(pool)
    .await?;

    // Re-read so a concurrent insert that won the race returns the canonical id.
    let stored = find_payment_id_by_idempotency_key(pool, merchant_id, key)
        .await?
        .unwrap_or_else(|| payment_id.to_string());
    Ok(stored)
}

pub async fn get_payment(pool: &Db, id: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn list_payments(
    pool: &Db,
    merchant_id: &str,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Payment>, i64)> {
    let (rows, total) = if let Some(s) = status {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? AND status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(merchant_id)
        .bind(s)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM payments WHERE merchant_id = ? AND status = ?",
        )
        .bind(merchant_id)
        .bind(s)
        .fetch_one(pool)
        .await?;

        (rows, total)
    } else {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(merchant_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE merchant_id = ?")
            .bind(merchant_id)
            .fetch_one(pool)
            .await?;

        (rows, total)
    };

    Ok((rows.iter().map(row_to_payment).collect(), total))
}

pub async fn list_payments_keyset(
    pool: &Db,
    merchant_id: &str,
    status: Option<&str>,
    limit: i64,
    cursor: Option<(&str, &str)>,
) -> Result<Vec<Payment>> {
    let rows = match (status, cursor) {
        (None, None) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (None, Some((ts, cid))) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments
             WHERE merchant_id = ? AND (created_at < ? OR (created_at = ? AND id < ?))
             ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(ts)
            .bind(ts)
            .bind(cid)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (Some(s), None) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? AND status = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(s)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (Some(s), Some((ts, cid))) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments
             WHERE merchant_id = ? AND status = ? AND (created_at < ? OR (created_at = ? AND id < ?))
             ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(s)
            .bind(ts)
            .bind(ts)
            .bind(cid)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.iter().map(row_to_payment).collect())
}

/// All payments still awaiting confirmation or top-up, oldest first. Rows whose
/// TTL has elapsed are excluded even if the sweeper hasn't transitioned them
/// yet, so an overdue intent is never polled.
pub async fn list_pending(pool: &Db) -> Result<Vec<Payment>> {
    let rows = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE status IN ('pending', 'underpaid')
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_payment).collect())
}

/// Transition every watchable payment whose TTL has elapsed to `expired`,
/// returning the rows that were swept so the caller can fire `payment.expired`
/// webhooks. Each row is updated with a guard on a watchable status so a payment
/// that settles concurrently is left untouched and not double-reported.
pub async fn expire_overdue(pool: &Db) -> Result<Vec<Payment>> {
    let overdue = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE status IN ('pending', 'underpaid')
           AND expires_at <= strftime('%Y-%m-%dT%H:%M:%SZ','now')
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut expired = Vec::new();
    for row in &overdue {
        let mut payment = row_to_payment(row);
        let result = sqlx::query(
            "UPDATE payments
                SET status = 'expired',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
              WHERE id = ? AND status IN ('pending', 'underpaid')",
        )
        .bind(&payment.id)
        .execute(pool)
        .await?;

        /* Only report rows we actually transitioned; a concurrent settlement
        may have flipped the status out from under us. */
        if result.rows_affected() == 1 {
            payment.status = "expired".to_string();
            expired.push(payment);
        }
    }

    Ok(expired)
}

pub async fn find_pending_by_memo(pool: &Db, memo: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE memo = ?
           AND status IN ('pending', 'underpaid')
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')",
    )
    .bind(memo)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn update_payment_status(
    pool: &Db,
    id: &str,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE payments SET status = ?, tx_hash = ?, paid_amount = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?",
    )
    .bind(status)
    .bind(tx_hash)
    .bind(paid_amount)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read a value from the durable key/value state table, if present.
pub async fn get_state(pool: &Db, key: &str) -> Result<Option<String>> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM kv_state WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

/// Insert or update a value in the durable key/value state table.
pub async fn set_state(pool: &Db, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO kv_state (key, value, updated_at)
         VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn memo_exists(pool: &Db, memo: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE memo = ?")
        .bind(memo)
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn save_webhook_delivery(
    pool: &Db,
    id: &str,
    payment_id: &str,
    url: &str,
    payload: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, payment_id, url, payload) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(payment_id)
    .bind(url)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_webhook_delivery(
    pool: &Db,
    id: &str,
    status: &str,
    attempts: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET status = ?, attempts = ?, last_attempt = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?",
    )
    .bind(status)
    .bind(attempts)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub payment_id: String,
    pub url: String,
    pub payload: String,
    pub status: String,
    pub attempts: i64,
    pub last_attempt: Option<String>,
    pub created_at: String,
}

fn row_to_webhook_delivery(row: &sqlx::sqlite::SqliteRow) -> WebhookDelivery {
    WebhookDelivery {
        id: row.get("id"),
        payment_id: row.get("payment_id"),
        url: row.get("url"),
        payload: row.get("payload"),
        status: row.get("status"),
        attempts: row.get("attempts"),
        last_attempt: row.get("last_attempt"),
        created_at: normalize_ts(&row.get::<String, _>("created_at")),
    }
}

/// Get all webhook deliveries for a payment, ordered by created_at descending.
pub async fn list_webhook_deliveries(pool: &Db, payment_id: &str) -> Result<Vec<WebhookDelivery>> {
    let rows = sqlx::query(
        "SELECT id, payment_id, url, payload, status, attempts, last_attempt, created_at
         FROM webhook_deliveries WHERE payment_id = ? ORDER BY created_at DESC",
    )
    .bind(payment_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_webhook_delivery).collect())
}

/// Get a specific webhook delivery by id.
pub async fn get_webhook_delivery(pool: &Db, id: &str) -> Result<Option<WebhookDelivery>> {
    let row = sqlx::query(
        "SELECT id, payment_id, url, payload, status, attempts, last_attempt, created_at
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_webhook_delivery))
}

/// Probe database connectivity. Returns `Ok(())` if the pool can execute a
/// trivial query, or `Err` if the database is unreachable.
pub async fn ping(pool: &Db) -> Result<()> {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(pool)
        .await?;
    Ok(())
}

/* ---------------------------------------------------------------------------
Merchant API-key management
--------------------------------------------------------------------------- */

/// Hash a raw API key with SHA-256, returning the hex digest.
/// This is the only representation stored in the database.
fn hash_api_key(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// Create a merchant row. Returns the merchant `id` — the raw key must be
/// shown to the user by the caller and is not recoverable afterward.
pub async fn create_merchant(pool: &Db, id: &str, raw_key: &str) -> Result<()> {
    let hash = hash_api_key(raw_key);
    sqlx::query("INSERT INTO merchants (id, api_key_hash) VALUES (?, ?)")
        .bind(id)
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Look up a merchant by their raw API key. Returns `None` if the key does
/// not match any registered merchant.
pub async fn find_merchant_by_key(pool: &Db, raw_key: &str) -> Result<Option<String>> {
    let hash = hash_api_key(raw_key);
    let id: Option<String> = sqlx::query_scalar("SELECT id FROM merchants WHERE api_key_hash = ?")
        .bind(hash)
        .fetch_optional(pool)
        .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn memory_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        pool
    }

    fn new_payment<'a>(id: &'a str, memo: &'a str, ttl_secs: i64) -> NewPayment<'a> {
        NewPayment {
            id,
            merchant_id: "m",
            destination_address: "GGATEWAY",
            memo,
            amount: "10",
            asset: "XLM",
            webhook_url: None,
            ttl_secs,
        }
    }

    #[tokio::test]
    async fn create_sets_expiry_from_ttl() {
        let pool = memory_db().await;
        // A one-hour TTL lands the expiry strictly in the future...
        let live = create_payment(&pool, new_payment("a", "MEMOA", 3600))
            .await
            .unwrap();
        assert!(live.expires_at > live.created_at);
        // ...while a negative TTL produces an already-overdue expiry.
        let dead = create_payment(&pool, new_payment("b", "MEMOB", -10))
            .await
            .unwrap();
        assert!(dead.expires_at < dead.created_at);
    }

    #[tokio::test]
    async fn list_pending_excludes_overdue_even_before_sweep() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("live", "MEMOL", 3600))
            .await
            .unwrap();
        create_payment(&pool, new_payment("dead", "MEMOD", -10))
            .await
            .unwrap();

        let pending = list_pending(&pool).await.unwrap();
        let ids: Vec<&str> = pending.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    #[tokio::test]
    async fn underpaid_payment_remains_findable_for_topup() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("partial", "MEMOP", 3600))
            .await
            .unwrap();
        update_payment_status(&pool, "partial", "underpaid", "TX1", "3")
            .await
            .unwrap();

        let found = find_pending_by_memo(&pool, "MEMOP").await.unwrap().unwrap();
        assert_eq!(found.id, "partial");
        assert_eq!(found.status, "underpaid");
        assert_eq!(found.paid_amount.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn overdue_underpaid_payment_expires() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("partial-dead", "MEMOX", -10))
            .await
            .unwrap();
        update_payment_status(&pool, "partial-dead", "underpaid", "TX1", "3")
            .await
            .unwrap();

        assert!(find_pending_by_memo(&pool, "MEMOX")
            .await
            .unwrap()
            .is_none());

        let expired = expire_overdue(&pool).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "partial-dead");
        assert_eq!(expired[0].status, "expired");
    }

    #[tokio::test]
    async fn expire_overdue_transitions_and_is_idempotent() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("live", "MEMOL", 3600))
            .await
            .unwrap();
        create_payment(&pool, new_payment("dead", "MEMOD", -10))
            .await
            .unwrap();

        // First sweep expires exactly the overdue intent and returns it.
        let expired = expire_overdue(&pool).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "dead");
        assert_eq!(expired[0].status, "expired");

        // GET /payments/:id reflects the expired status.
        let fetched = get_payment(&pool, "dead").await.unwrap().unwrap();
        assert_eq!(fetched.status, "expired");
        // The live intent is untouched.
        assert_eq!(
            get_payment(&pool, "live").await.unwrap().unwrap().status,
            "pending"
        );

        // A second sweep is a no-op — nothing is double-reported.
        assert_eq!(expire_overdue(&pool).await.unwrap().len(), 0);
    }
}
