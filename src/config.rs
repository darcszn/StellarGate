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

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub network: String,
    pub horizon_url: String,
    pub gateway_public: String,
    pub gateway_secret: String,
    pub usdc_issuer: String,
    pub webhook_secret: String,
    pub webhook_retry_attempts: u32,
    pub webhook_retry_delay_ms: u64,
    pub poll_interval_secs: u64,
    /// How long a payment intent stays `pending` before the expiry sweeper
    /// transitions it to `expired`. Counted from the intent's `created_at`.
    pub payment_ttl_secs: u64,
    /// Comma-separated list of allowed CORS origins, e.g. `https://app.example.com`.
    /// Required when `STELLAR_NETWORK=public`; optional (falls back to permissive) on testnet.
    pub cors_allowed_origins: Vec<String>,
    pub listener_mode: ListenerMode,
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
            usdc_issuer: env_or(
                "USDC_ISSUER",
                "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
            ),
            webhook_secret: env_or("WEBHOOK_SECRET", "default-secret"),
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3),
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000),
            poll_interval_secs: parse_env("POLL_INTERVAL_SECS", 10),
            payment_ttl_secs: parse_env("PAYMENT_TTL_SECS", 3600),
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
            .field("usdc_issuer", &self.usdc_issuer)
            .field("webhook_secret", &"***")
            .field("webhook_retry_attempts", &self.webhook_retry_attempts)
            .field("webhook_retry_delay_ms", &self.webhook_retry_delay_ms)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("listener_mode", &self.listener_mode)
            .finish()
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
            usdc_issuer: "GISSUER".into(),
            webhook_secret: "webhook-hmac-secret".into(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
            poll_interval_secs: 10,
            payment_ttl_secs: 3600,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
        };
        let output = format!("{cfg:?}");
        assert!(!output.contains("super-secret-key"), "gateway_secret must not appear in Debug output");
        assert!(!output.contains("webhook-hmac-secret"), "webhook_secret must not appear in Debug output");
        assert!(output.contains("***"), "redacted marker must appear in Debug output");
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
