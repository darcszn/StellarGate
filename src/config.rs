use anyhow::Result;

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
    /// Comma-separated list of allowed CORS origins, e.g. `https://app.example.com`.
    /// Required when `STELLAR_NETWORK=public`; optional (falls back to permissive) on testnet.
    pub cors_allowed_origins: Vec<String>,
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
            cors_allowed_origins: std::env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        })
    }

    /// True once a real gateway wallet has been configured. Until then the
    /// Horizon poller stays idle rather than scanning the placeholder account.
    pub fn gateway_configured(&self) -> bool {
        !self.gateway_public.is_empty() && self.gateway_public != "UNCONFIGURED"
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
