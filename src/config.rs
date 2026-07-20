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
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "poll" => Self::Poll,
            "stream" => Self::Stream,
            other => {
                tracing::warn!("invalid STELLAR_LISTENER_MODE={other:?}, using \"stream\"");
                Self::Stream
            }
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
    /// Shared secret required (via the `X-Admin-Secret` header) to call
    /// `POST /merchants`. Empty disables provisioning entirely — the endpoint
    /// rejects every request rather than falling back to an open default.
    pub admin_provisioning_secret: String,
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
        let gateway_secret = std::env::var("STELLAR_GATEWAY_SECRET").unwrap_or_default();
        let webhook_secret = Self::validate_webhook_secret(std::env::var("WEBHOOK_SECRET"))?;

        let config = Self {
            port: parse_env("PORT", 3000),
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
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3),
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000),
            poll_interval_secs: parse_env("POLL_INTERVAL_SECS", 10),
            payment_ttl_secs: parse_env("PAYMENT_TTL_SECS", 3600),
            rate_limit_requests_per_sec: parse_env("RATE_LIMIT_REQUESTS_PER_SEC", 10),
            db_pool_max_connections: parse_env("DB_POOL_MAX_CONNECTIONS", 10),
            db_busy_timeout_ms: parse_env("DB_BUSY_TIMEOUT_MS", 5000),
            cors_allowed_origins: std::env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            listener_mode: ListenerMode::parse(
                &std::env::var("STELLAR_LISTENER_MODE").unwrap_or_default(),
            ),
            admin_provisioning_secret: env_or("ADMIN_PROVISIONING_SECRET", ""),
        };
        config.validate_addresses()?;
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
        if secret == "default-secret" {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET cannot equal \"default-secret\""
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
            .field("admin_provisioning_secret", &"***")
            .finish()
    }
}

/// Parse an env var into `T`, falling back to `default` (and warning) when the
/// variable is set but unparseable, so a typo never silently breaks behaviour.
fn parse_env<T>(key: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    match std::env::var(key) {
        Ok(raw) => raw.parse().unwrap_or_else(|_| {
            tracing::warn!("invalid value for {key}={raw:?}, using default");
            default
        }),
        Err(_) => default,
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
            poll_interval_secs: 10,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            admin_provisioning_secret: "admin-super-secret".into(),
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
            poll_interval_secs: 10,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            admin_provisioning_secret: String::new(),
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
            err.contains("cannot equal \"default-secret\""),
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
}
