use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Config {
    pub proxy_addr: String,
    pub api_addr: String,
    pub database_url: String,
    pub session_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            proxy_addr: std::env::var("PROXY_ADDR").unwrap_or_else(|_| "0.0.0.0:5433".into()),
            api_addr: std::env::var("API_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5432/marlobu".into()),
            session_ttl_seconds: std::env::var("SESSION_TTL_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
        })
    }
}
