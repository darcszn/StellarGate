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
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
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

    // Normalise legacy rows that were written by the old datetime('now') default,
    // which produced "YYYY-MM-DD HH:MM:SS" (space, no Z). Safe to run on every
    // startup — the WHERE clause skips rows that are already RFC 3339.
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
}

pub async fn create_payment(pool: &Db, new: NewPayment<'_>) -> Result<Payment> {
    sqlx::query(
        "INSERT INTO payments (id, merchant_id, destination_address, memo, amount, asset, webhook_url)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.id)
    .bind(new.merchant_id)
    .bind(new.destination_address)
    .bind(new.memo)
    .bind(new.amount)
    .bind(new.asset)
    .bind(new.webhook_url)
    .execute(pool)
    .await?;

    get_payment(pool, new.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Payment not found after insert"))
}

pub async fn get_payment(pool: &Db, id: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at
         FROM payments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn list_payments(
    pool: &Db,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Payment>, i64)> {
    let (rows, total) = if let Some(s) = status {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at
             FROM payments WHERE status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(s)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE status = ?")
            .bind(s)
            .fetch_one(pool)
            .await?;

        (rows, total)
    } else {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at
             FROM payments ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
            .fetch_one(pool)
            .await?;

        (rows, total)
    };

    Ok((rows.iter().map(row_to_payment).collect(), total))
}

/// All payments still awaiting confirmation, oldest first. Used by the Horizon
/// poller to decide which memos to watch for on-chain.
pub async fn list_pending(pool: &Db) -> Result<Vec<Payment>> {
    let rows = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at
         FROM payments WHERE status = 'pending' ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_payment).collect())
}

pub async fn find_pending_by_memo(pool: &Db, memo: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at
         FROM payments WHERE memo = ? AND status = 'pending'",
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

pub async fn update_webhook_delivery(pool: &Db, id: &str, status: &str, attempts: i64) -> Result<()> {
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
        created_at: row.get("created_at"),
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
