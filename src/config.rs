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
    /// Comma-separated list of allowed CORS origins, e.g. `https://app.example.com`.
    /// Required when `STELLAR_NETWORK=public`; optional (falls back to permissive) on testnet.
    pub cors_allowed_origins: Vec<String>,
    pub listener_mode: ListenerMode,
    /// Rate limit for `POST /payments` (requests per second per IP).
    pub rate_limit_requests_per_sec: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            port: parse_env("PORT", 3000),
            database_url: env_or("DATABASE_URL", "sqlite:stellargate.db"),
            network: env_or("STELLAR_NETWORK", "testnet"),
            horizon_url: env_or("STELLAR_HORIZON_URL", "https://horizon-testnet.stellar.org"),
            gateway_public: env_or("STELLAR_GATEWAY_PUBLIC", "UNCONFIGURED"),
            gateway_secret: env_or("STELLAR_GATEWAY_SECRET", ""),
            accepted_assets: {
                let raw = std::env::var("ACCEPTED_ASSETS").unwrap_or_default();
                if raw.is_empty() {
                    AcceptedAsset::default_list()
                } else {
                    AcceptedAsset::parse_list(&raw)
                }
            },
            webhook_secret: env_or("WEBHOOK_SECRET", "default-secret"),
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3),
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000),
            poll_interval_secs: parse_env("POLL_INTERVAL_SECS", 10),
            payment_ttl_secs: parse_env("PAYMENT_TTL_SECS", 3600),
            rate_limit_requests_per_sec: parse_env("RATE_LIMIT_RPS", 10),
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
            rate_limit_requests_per_sec: parse_env("RATE_LIMIT_REQUESTS_PER_SEC", 10),
        })
    }

    /// True once a real gateway wallet has been configured. Until then the
    /// Horizon poller stays idle rather than scanning the placeholder account.
    pub fn gateway_configured(&self) -> bool {
        !self.gateway_public.is_empty() && self.gateway_public != "UNCONFIGURED"
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
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("listener_mode", &self.listener_mode)
            .field(
                "rate_limit_requests_per_sec",
                &self.rate_limit_requests_per_sec,
            )
            .finish()
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            rate_limit_requests_per_sec: 10,
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
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
