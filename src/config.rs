use anyhow::Result;

/// How the service detects incoming on-chain payments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerMode {
    /// Subscribe to Horizon's Server-Sent-Events payment stream for near
    /// real-time settlement, with the interval poller running alongside as a
    /// reconciler for any events missed during reconnects.
    Stream,
    /// Only run the interval poller; no streaming connection is opened.
    Poll,
}

impl ListenerMode {
    /// Parse `STELLAR_LISTENER_MODE` from a raw env-var value.
    ///
    /// - Empty / unset → defaults to `Stream` (no error).
    /// - `"stream"` or `"poll"` (case-insensitive) → the chosen mode.
    /// - Any other non-empty value → `Err`, which aborts boot with a clear
    ///   message rather than silently falling back to a different mode.
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::Stream),
            "stream" => Ok(Self::Stream),
            "poll" => Ok(Self::Poll),
            other => Err(anyhow::anyhow!(
                "STELLAR_LISTENER_MODE={other:?} is not a recognised value. \
                 Valid values are \"stream\" or \"poll\". \
                 Fix the environment variable or remove it to use the default (\"stream\")."
            )),
        }
    }
}

/// A Stellar asset the gateway is configured to accept.
///
/// `issuer` is `None` for the native XLM asset; all other assets require an
/// issuer address. Configure via `ACCEPTED_ASSETS` as comma-separated entries
/// of the form `CODE` (native) or `CODE:ISSUER`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedAsset {
    pub code: String,
    pub issuer: Option<String>,
}

impl AcceptedAsset {
    pub(crate) fn parse_list(raw: &str) -> Vec<Self> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|entry| {
                if let Some((code, issuer)) = entry.split_once(':') {
                    AcceptedAsset {
                        code: code.trim().to_uppercase(),
                        issuer: Some(issuer.trim().to_string()),
                    }
                } else {
                    AcceptedAsset {
                        code: entry.trim().to_uppercase(),
                        issuer: None,
                    }
                }
            })
            .collect()
    }

    pub fn default_list() -> Vec<Self> {
        vec![
            AcceptedAsset {
                code: "XLM".into(),
                issuer: None,
            },
            AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
            },
        ]
    }
}

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub network: String,
    pub horizon_url: String,
    pub gateway_public: String,
    pub gateway_secret: String,
    /// Assets the gateway will accept, validated on POST /payments and in verify().
    /// Configure via ACCEPTED_ASSETS=XLM,USDC:GISSUER (comma-separated).
    pub accepted_assets: Vec<AcceptedAsset>,
    pub webhook_secret: String,
    pub webhook_retry_attempts: u32,
    pub webhook_retry_delay_ms: u64,
    /// Per-attempt timeout for outbound webhook POSTs, in seconds. Each
    /// delivery attempt is bounded independently, so a slow receiver can't
    /// hold up the retry loop (or the reconciler) for more than this value.
    /// Defaults to 10 seconds — short enough to keep retries responsive while
    /// giving receivers a fair window to process the request.
    pub webhook_timeout_secs: u64,
    /// How often (seconds) the background redrive worker scans for stuck
    /// webhook deliveries (`pending`/`failed` rows left behind by a process
    /// that exited mid-delivery, or a receiver that was down when retries
    /// were exhausted). The worker's first pass runs immediately on startup,
    /// so a restart redrives without waiting a full interval.
    pub webhook_redrive_interval_secs: u64,
    /// Maximum number of redrive HTTP attempts in flight at once.
    pub webhook_redrive_concurrency: usize,
    /// Total attempts (inline + redrive) before a delivery is left `failed`
    /// permanently.
    pub webhook_redrive_max_attempts: u32,
    /// How long (seconds) a delivery must sit idle since its last attempt (or
    /// creation) before the redrive worker will touch it. Must comfortably
    /// exceed the worst-case inline delivery time
    /// (`webhook_retry_attempts * (webhook_timeout_secs + webhook_retry_delay_ms)`)
    /// so the worker never races a `dispatch()` call that is still in flight
    /// for the same row.
    pub webhook_redrive_grace_secs: i64,
    pub poll_interval_secs: u64,
    /// How long a payment intent stays `pending` before the expiry sweeper
    /// transitions it to `expired`. Counted from the intent's `created_at`.
    pub payment_ttl_secs: u64,
    /// Maximum number of requests per second allowed per client IP before the
    /// rate-limit middleware responds with `429 Too Many Requests`.
    pub rate_limit_requests_per_sec: u32,
    /// Maximum number of SQLite connections in the pool.
    /// WAL mode allows one writer + many readers, so keeping this modest avoids
    /// contention. Defaults to 10.
    pub db_pool_max_connections: u32,
    /// How long (ms) SQLite waits for a lock before returning SQLITE_BUSY.
    /// Must be > 0 to avoid immediate lock errors under concurrent writes.
    pub db_busy_timeout_ms: u64,
    /// Comma-separated list of allowed CORS origins, e.g. `https://app.example.com`.
    /// Required when `STELLAR_NETWORK=public`; optional (falls back to permissive) on testnet.
    pub cors_allowed_origins: Vec<String>,
    pub listener_mode: ListenerMode,
    /// Bypasses the SSRF guard's loopback/link-local/private/reserved IP check
    /// on `webhook_url` (the DNS resolution and http(s)-scheme check still
    /// run). Only for local development and tests that target a loopback mock
    /// server — never enable this in production.
    pub webhook_allow_private_targets: bool,
    /// Shared secret required (via the `X-Admin-Secret` header) to call
    /// `POST /merchants`. Empty disables provisioning entirely — the endpoint
    /// rejects every request rather than falling back to an open default.
    pub admin_provisioning_secret: String,
    /// Per-request timeout for the whole API, in seconds. A request whose
    /// handler hasn't produced a response within this window is aborted with
    /// `408 Request Timeout`, so a slow client or a stuck handler can't tie up
    /// a connection indefinitely. Defaults to 30 seconds.
    pub request_timeout_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:stellargate.db".to_string());
        let network = std::env::var("STELLAR_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let horizon_url = std::env::var("STELLAR_HORIZON_URL")
            .unwrap_or_else(|_| "https://horizon-testnet.stellar.org".to_string());
        let gateway_public =
            std::env::var("STELLAR_GATEWAY_PUBLIC").unwrap_or_else(|_| "UNCONFIGURED".to_string());
        let gateway_secret = Self::validate_gateway_secret(
            std::env::var("STELLAR_GATEWAY_SECRET").unwrap_or_default(),
            &gateway_public,
        )?;
        let webhook_secret = Self::validate_webhook_secret(std::env::var("WEBHOOK_SECRET"))?;

        let cors_allowed_origins: Vec<String> = {
            let raw_origins: Vec<String> = std::env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();

            // Validate every configured origin now so a typo aborts boot with
            // a clear message instead of silently removing the origin from the
            // allowlist (or producing an empty allowlist with no error).
            for origin in &raw_origins {
                origin.parse::<axum::http::HeaderValue>().map_err(|e| {
                    anyhow::anyhow!(
                        "CORS_ALLOWED_ORIGINS contains an invalid origin {origin:?}: {e}. \
                         Fix or remove the bad entry."
                    )
                })?;
            }
            raw_origins
        };

        let config = Self {
            port: parse_env("PORT", 3000)?,
            database_url,
            network,
            horizon_url,
            gateway_public,
            gateway_secret,
            accepted_assets: {
                let raw = std::env::var("ACCEPTED_ASSETS").unwrap_or_default();
                if raw.is_empty() {
                    AcceptedAsset::default_list()
                } else {
                    AcceptedAsset::parse_list(&raw)
                }
            },
            webhook_secret,
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3)?,
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000)?,
            webhook_timeout_secs: parse_env("WEBHOOK_TIMEOUT_SECS", 10)?,
            webhook_redrive_interval_secs: parse_env("WEBHOOK_REDRIVE_INTERVAL_SECS", 30)?,
            webhook_redrive_concurrency: parse_env("WEBHOOK_REDRIVE_CONCURRENCY", 4)?,
            webhook_redrive_max_attempts: parse_env("WEBHOOK_REDRIVE_MAX_ATTEMPTS", 8)?,
            webhook_redrive_grace_secs: parse_env("WEBHOOK_REDRIVE_GRACE_SECS", 60)?,
            poll_interval_secs: parse_env("POLL_INTERVAL_SECS", 10)?,
            payment_ttl_secs: parse_env("PAYMENT_TTL_SECS", 3600)?,
            rate_limit_requests_per_sec: parse_env("RATE_LIMIT_REQUESTS_PER_SEC", 10)?,
            db_pool_max_connections: parse_env("DB_POOL_MAX_CONNECTIONS", 10)?,
            db_busy_timeout_ms: parse_env("DB_BUSY_TIMEOUT_MS", 5000)?,
            cors_allowed_origins,
            listener_mode: ListenerMode::parse(
                &std::env::var("STELLAR_LISTENER_MODE").unwrap_or_default(),
            )?,
            webhook_allow_private_targets: parse_env("WEBHOOK_ALLOW_PRIVATE_TARGETS", false)?,
            admin_provisioning_secret: env_or("ADMIN_PROVISIONING_SECRET", ""),
            request_timeout_secs: parse_env("REQUEST_TIMEOUT_SECS", 30)?,
        };
        config.validate_addresses()?;
        config.validate_timing()?;
        Ok(config)
    }

    /// True once a real gateway wallet has been configured. Until then the
    /// Horizon poller stays idle rather than scanning the placeholder account.
    pub fn gateway_configured(&self) -> bool {
        !self.gateway_public.is_empty() && self.gateway_public != "UNCONFIGURED"
    }

    /// Reject configured Stellar addresses — the gateway account and any asset
    /// issuers — that are not valid strkeys, so a typo fails fast at boot rather
    /// than silently producing unpayable intents. The unconfigured placeholder
    /// is left alone; the poller stays idle until a real key is provided.
    fn validate_addresses(&self) -> Result<()> {
        if self.gateway_configured() {
            crate::strkey::validate_account_id(&self.gateway_public).map_err(|e| {
                anyhow::anyhow!(
                    "STELLAR_GATEWAY_PUBLIC ({}) is not a valid Stellar account address: {e}",
                    self.gateway_public
                )
            })?;
        }
        for asset in &self.accepted_assets {
            if let Some(issuer) = &asset.issuer {
                crate::strkey::validate_account_id(issuer).map_err(|e| {
                    anyhow::anyhow!(
                        "issuer for asset {} ({}) is not a valid Stellar account address: {e}",
                        asset.code,
                        issuer
                    )
                })?;
            }
        }
        Ok(())
    }

    /// Cross-validate timing fields to catch nonsensical combinations that
    /// would cause silent misbehaviour at runtime:
    ///
    /// - `POLL_INTERVAL_SECS == 0` → infinite tight loop, 100 % CPU
    /// - `PAYMENT_TTL_SECS == 0` → every intent expires the moment it is created
    /// - `PAYMENT_TTL_SECS < POLL_INTERVAL_SECS` → intents expire before the
    ///   poller ever scans them, so payments land but are never matched
    /// - `WEBHOOK_RETRY_ATTEMPTS == 0` → webhooks are never delivered
    /// - `WEBHOOK_RETRY_DELAY_MS == 0` with retries > 1 → retries hammer the
    ///   target endpoint with no back-off
    /// - `REQUEST_TIMEOUT_SECS == 0` → every request is aborted immediately
    fn validate_timing(&self) -> Result<()> {
        if self.poll_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "POLL_INTERVAL_SECS must be > 0 (got 0). \
                 A zero interval creates a tight polling loop at 100% CPU."
            ));
        }

        if self.payment_ttl_secs == 0 {
            return Err(anyhow::anyhow!(
                "PAYMENT_TTL_SECS must be > 0 (got 0). \
                 A zero TTL expires every payment intent immediately on creation."
            ));
        }

        if self.payment_ttl_secs < self.poll_interval_secs {
            return Err(anyhow::anyhow!(
                "PAYMENT_TTL_SECS ({}) must be >= POLL_INTERVAL_SECS ({}). \
                 With the current settings, a payment intent would expire before \
                 the poller ever gets a chance to detect it.",
                self.payment_ttl_secs,
                self.poll_interval_secs
            ));
        }

        if self.webhook_retry_attempts == 0 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_RETRY_ATTEMPTS must be > 0 (got 0). \
                 Zero attempts means webhooks are silently never delivered."
            ));
        }

        if self.webhook_retry_attempts > 1 && self.webhook_retry_delay_ms == 0 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_RETRY_DELAY_MS must be > 0 when WEBHOOK_RETRY_ATTEMPTS ({}) > 1. \
                 A zero delay causes retry bursts that hammer the target endpoint.",
                self.webhook_retry_attempts
            ));
        }

        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "REQUEST_TIMEOUT_SECS must be > 0 (got 0). \
                 A zero timeout would abort every request immediately."
            ));
        }

        Ok(())
    }

    fn validate_webhook_secret(raw_secret: Result<String, std::env::VarError>) -> Result<String> {
        let secret = match raw_secret {
            Ok(s) => s,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "WEBHOOK_SECRET environment variable is missing"
                ))
            }
        };

        if secret.is_empty() {
            return Err(anyhow::anyhow!("WEBHOOK_SECRET cannot be empty"));
        }
        if secret.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET cannot contain only whitespace"
            ));
        }
        // Reject known placeholder values that might be copied verbatim from
        // .env.example or documentation.
        const WEBHOOK_PLACEHOLDERS: &[&str] = &[
            "default-secret",
            "your_webhook_signing_secret",
            "REPLACE_ME_webhook_signing_secret",
        ];
        if WEBHOOK_PLACEHOLDERS.contains(&secret.as_str()) || secret.starts_with("REPLACE_ME_") {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET is set to a known placeholder value ({:?}). \
                 Replace it with a strong, randomly-generated secret.",
                secret
            ));
        }
        if secret.len() < 32 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET must be at least 32 characters long (got {})",
                secret.len()
            ));
        }

        Ok(secret)
    }

    /// Validate `STELLAR_GATEWAY_SECRET` at boot.
    ///
    /// - Empty is allowed when the gateway public key is also unconfigured
    ///   (development / read-only mode).
    /// - The placeholder value from `.env.example` (`SXXX…` or `REPLACE_ME_*`) is always
    ///   rejected — it would silently sign nothing but gives operators false
    ///   confidence that the key is set.
    fn validate_gateway_secret(secret: String, gateway_public: &str) -> Result<String> {
        let configured = !gateway_public.is_empty() && gateway_public != "UNCONFIGURED";

        // Reject the classic .env.example placeholder: starts with 'S' and
        // the rest are all 'X's (e.g. SXXXXXXX…56 chars).
        if !secret.is_empty()
            && secret.starts_with('S')
            && secret.chars().skip(1).all(|c| c == 'X')
        {
            return Err(anyhow::anyhow!(
                "STELLAR_GATEWAY_SECRET is set to a placeholder value from .env.example. \
                 Replace it with your real Stellar secret key."
            ));
        }

        // Reject any REPLACE_ME_ placeholder.
        if secret.starts_with("REPLACE_ME_") {
            return Err(anyhow::anyhow!(
                "STELLAR_GATEWAY_SECRET is set to a placeholder value ({:?}). \
                 Replace it with your real Stellar secret key.",
                secret
            ));
        }

        // If a real public key has been configured, a secret key must also be present.
        if configured && secret.is_empty() {
            return Err(anyhow::anyhow!(
                "STELLAR_GATEWAY_SECRET is required when STELLAR_GATEWAY_PUBLIC is set. \
                 Set STELLAR_GATEWAY_SECRET to the corresponding secret key."
            ));
        }

        Ok(secret)
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("database_url", &self.database_url)
            .field("network", &self.network)
            .field("horizon_url", &self.horizon_url)
            .field("gateway_public", &self.gateway_public)
            .field("gateway_secret", &"***")
            .field("accepted_assets", &self.accepted_assets)
            .field("webhook_secret", &"***")
            .field("webhook_retry_attempts", &self.webhook_retry_attempts)
            .field("webhook_retry_delay_ms", &self.webhook_retry_delay_ms)
            .field("webhook_timeout_secs", &self.webhook_timeout_secs)
            .field(
                "webhook_redrive_interval_secs",
                &self.webhook_redrive_interval_secs,
            )
            .field(
                "webhook_redrive_concurrency",
                &self.webhook_redrive_concurrency,
            )
            .field(
                "webhook_redrive_max_attempts",
                &self.webhook_redrive_max_attempts,
            )
            .field(
                "webhook_redrive_grace_secs",
                &self.webhook_redrive_grace_secs,
            )
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("payment_ttl_secs", &self.payment_ttl_secs)
            .field(
                "rate_limit_requests_per_sec",
                &self.rate_limit_requests_per_sec,
            )
            .field("db_pool_max_connections", &self.db_pool_max_connections)
            .field("db_busy_timeout_ms", &self.db_busy_timeout_ms)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("listener_mode", &self.listener_mode)
            .field(
                "webhook_allow_private_targets",
                &self.webhook_allow_private_targets,
            )
            .field("admin_provisioning_secret", &"***")
            .field("request_timeout_secs", &self.request_timeout_secs)
            .finish()
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse an env var into `T`.
///
/// - If the variable is absent, `default` is returned.
/// - If the variable is present but cannot be parsed, boot is aborted with a
///   clear error message instead of silently falling back to the default.
///   This prevents misconfigured values (e.g. a typo in `PAYMENT_TTL_SECS`)
///   from going unnoticed in production.
fn parse_env<T>(key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(raw) => raw.parse::<T>().map_err(|e| {
            anyhow::anyhow!(
                "invalid value for {key}={raw:?}: {e}. \
                 Fix the environment variable or remove it to use the default."
            )
        }),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let cfg = Config {
            port: 3000,
            database_url: "sqlite:test.db".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "GPUBLIC".into(),
            gateway_secret: "super-secret-key".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: "webhook-hmac-secret".into(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            poll_interval_secs: 10,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: "admin-super-secret".into(),
            request_timeout_secs: 30,
        };
        let output = format!("{cfg:?}");
        assert!(
            !output.contains("super-secret-key"),
            "gateway_secret must not appear in Debug output"
        );
        assert!(
            !output.contains("webhook-hmac-secret"),
            "webhook_secret must not appear in Debug output"
        );
        assert!(
            !output.contains("admin-super-secret"),
            "admin_provisioning_secret must not appear in Debug output"
        );
        assert!(
            output.contains("***"),
            "redacted marker must appear in Debug output"
        );
    }

    #[test]
    fn parse_accepted_assets_from_env_string() {
        let assets = AcceptedAsset::parse_list("XLM,USDC:GISSUER,EURC:GISSUER2");
        assert_eq!(assets.len(), 3);
        assert_eq!(
            assets[0],
            AcceptedAsset {
                code: "XLM".into(),
                issuer: None
            }
        );
        assert_eq!(
            assets[1],
            AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GISSUER".into())
            }
        );
        assert_eq!(
            assets[2],
            AcceptedAsset {
                code: "EURC".into(),
                issuer: Some("GISSUER2".into())
            }
        );
    }

    fn sample_config() -> Config {
        Config {
            port: 3000,
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "UNCONFIGURED".into(),
            gateway_secret: String::new(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: String::new(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            poll_interval_secs: 10,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: String::new(),
            request_timeout_secs: 30,
        }
    }

    #[test]
    fn validate_addresses_passes_for_unconfigured_gateway_and_default_issuer() {
        // The placeholder gateway is skipped; the default USDC issuer is valid.
        assert!(sample_config().validate_addresses().is_ok());
    }

    #[test]
    fn validate_addresses_accepts_a_real_gateway_key() {
        let mut cfg = sample_config();
        cfg.gateway_public = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into();
        assert!(cfg.validate_addresses().is_ok());
    }

    #[test]
    fn validate_addresses_rejects_a_corrupted_gateway_key() {
        let mut cfg = sample_config();
        // A valid key with one character flipped — a realistic typo.
        cfg.gateway_public = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLB5".into();
        let err = cfg.validate_addresses().unwrap_err().to_string();
        assert!(err.contains("STELLAR_GATEWAY_PUBLIC"), "got: {err}");
    }

    #[test]
    fn validate_addresses_rejects_an_invalid_issuer() {
        let mut cfg = sample_config();
        cfg.accepted_assets = vec![AcceptedAsset {
            code: "USDC".into(),
            issuer: Some("GNOTAREALISSUER".into()),
        }];
        let err = cfg.validate_addresses().unwrap_err().to_string();
        assert!(err.contains("USDC"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_missing() {
        let err = Config::validate_webhook_secret(Err(std::env::VarError::NotPresent))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("environment variable is missing"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_webhook_secret_empty() {
        let err = Config::validate_webhook_secret(Ok("".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be empty"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_whitespace() {
        let err = Config::validate_webhook_secret(Ok("   ".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot contain only whitespace"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_default() {
        let err = Config::validate_webhook_secret(Ok("default-secret".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("known placeholder value"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_webhook_secret_short() {
        let err = Config::validate_webhook_secret(Ok("too-short".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("must be at least 32 characters long"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_webhook_secret_valid() {
        let secret = "a-very-long-and-secure-webhook-signing-secret-32-chars";
        let res = Config::validate_webhook_secret(Ok(secret.into())).unwrap();
        assert_eq!(res, secret);
    }

    fn run_with_env<F>(env_vars: &[(&str, Option<&str>)], f: F)
    where
        F: FnOnce(),
    {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();

        // Backup current env values
        let backups: Vec<(String, Option<String>)> = env_vars
            .iter()
            .map(|(key, _)| (key.to_string(), std::env::var(key).ok()))
            .collect();

        // Set new values
        for &(key, val) in env_vars {
            if let Some(v) = val {
                std::env::set_var(key, v);
            } else {
                std::env::remove_var(key);
            }
        }

        // Run the test logic
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        // Restore backups
        for (key, val) in backups {
            if let Some(v) = val {
                std::env::set_var(key, v);
            } else {
                std::env::remove_var(key);
            }
        }

        if let Err(err) = res {
            std::panic::resume_unwind(err);
        }
    }

    #[test]
    fn startup_fails_in_production_if_webhook_secret_missing() {
        run_with_env(
            &[
                ("STELLAR_NETWORK", Some("public")),
                ("WEBHOOK_SECRET", None),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("WEBHOOK_SECRET environment variable is missing"),
                    "got: {err}"
                );
            },
        );
    }

    #[test]
    fn startup_succeeds_with_valid_configuration() {
        run_with_env(
            &[
                ("STELLAR_NETWORK", Some("public")),
                (
                    "WEBHOOK_SECRET",
                    Some("a-very-long-and-secure-webhook-signing-secret-32-chars"),
                ),
                ("DATABASE_URL", Some("sqlite::memory:")),
                (
                    "STELLAR_GATEWAY_PUBLIC",
                    Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"),
                ),
                (
                    "STELLAR_GATEWAY_SECRET",
                    Some("SCZANGBA5RLKJHTBF4RJNRJMZWI4VKTHCRKOVAH7LRZZPZHHZWATAWBN"),
                ),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.network, "public");
                assert_eq!(
                    cfg.webhook_secret,
                    "a-very-long-and-secure-webhook-signing-secret-32-chars"
                );
            },
        );
    }

    // ── validate_timing ──────────────────────────────────────────────────────

    fn timing_config() -> Config {
        let mut cfg = sample_config();
        cfg.poll_interval_secs = 10;
        cfg.payment_ttl_secs = 3600;
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_retry_delay_ms = 5000;
        cfg
    }

    #[test]
    fn timing_valid_defaults_pass() {
        assert!(timing_config().validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_zero_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("POLL_INTERVAL_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_ttl() {
        let mut cfg = timing_config();
        cfg.payment_ttl_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("PAYMENT_TTL_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_ttl_shorter_than_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 60;
        cfg.payment_ttl_secs = 30; // < poll interval
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(
            err.contains("PAYMENT_TTL_SECS") && err.contains("POLL_INTERVAL_SECS"),
            "got: {err}"
        );
    }

    #[test]
    fn timing_allows_ttl_equal_to_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 60;
        cfg.payment_ttl_secs = 60; // equal is fine
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_zero_retry_attempts() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_RETRY_ATTEMPTS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_delay_with_multiple_retries() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_retry_delay_ms = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_RETRY_DELAY_MS"), "got: {err}");
    }

    #[test]
    fn timing_allows_zero_delay_with_single_attempt() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 1;
        cfg.webhook_retry_delay_ms = 0; // no retries, so no burst
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn startup_fails_on_ttl_shorter_than_poll_interval_via_env() {
        run_with_env(
            &[
                (
                    "WEBHOOK_SECRET",
                    Some("a-very-long-and-secure-webhook-signing-secret-32-chars"),
                ),
                ("POLL_INTERVAL_SECS", Some("300")),
                ("PAYMENT_TTL_SECS", Some("60")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("PAYMENT_TTL_SECS") || err.contains("POLL_INTERVAL_SECS"),
                    "got: {err}"
                );
            },
        );
    }

    // ── ListenerMode::parse ──────────────────────────────────────────────────

    #[test]
    fn listener_mode_empty_defaults_to_stream() {
        assert_eq!(ListenerMode::parse("").unwrap(), ListenerMode::Stream);
    }

    #[test]
    fn listener_mode_stream_parses() {
        assert_eq!(ListenerMode::parse("stream").unwrap(), ListenerMode::Stream);
        assert_eq!(ListenerMode::parse("STREAM").unwrap(), ListenerMode::Stream);
    }

    #[test]
    fn listener_mode_poll_parses() {
        assert_eq!(ListenerMode::parse("poll").unwrap(), ListenerMode::Poll);
        assert_eq!(ListenerMode::parse("POLL").unwrap(), ListenerMode::Poll);
    }

    #[test]
    fn listener_mode_invalid_aborts_boot() {
        let err = ListenerMode::parse("streem").unwrap_err().to_string();
        assert!(
            err.contains("STELLAR_LISTENER_MODE"),
            "error should name the variable; got: {err}"
        );
        assert!(
            err.contains("streem"),
            "error should echo the bad value; got: {err}"
        );
    }

    // ── validate_gateway_secret ──────────────────────────────────────────────

    #[test]
    fn gateway_secret_empty_allowed_when_unconfigured() {
        let res = Config::validate_gateway_secret(String::new(), "UNCONFIGURED");
        assert!(res.is_ok());
    }

    #[test]
    fn gateway_secret_placeholder_rejected() {
        let placeholder = "S".to_string() + &"X".repeat(55);
        let err = Config::validate_gateway_secret(placeholder, "UNCONFIGURED")
            .unwrap_err()
            .to_string();
        assert!(err.contains("placeholder value"), "got: {err}");
    }

    #[test]
    fn gateway_secret_required_when_public_key_set() {
        let err = Config::validate_gateway_secret(
            String::new(),
            "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("STELLAR_GATEWAY_SECRET is required"),
            "got: {err}"
        );
    }

    #[test]
    fn gateway_secret_valid_accepted() {
        // A real-looking secret key (not all-X after S)
        let res = Config::validate_gateway_secret(
            "SCZANGBA5RLKJHTBF4RJNRJMZWI4VKTHCRKOVAH7LRZZPZHHZWATAWBN".into(),
            "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
        );
        assert!(res.is_ok());
    }
}
