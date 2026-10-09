use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use deadpool_postgres::{Config as PgConfig, Runtime};

mod proxy;
mod session;
mod rewriter;
mod sandbox;
mod approval;
mod api;
mod config;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "marlobu_proxy=debug,info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv().ok();

    let config = config::Config::from_env()?;

    tracing::info!("Starting Marlobu proxy");
    tracing::info!("Proxy listening on {}", config.proxy_addr);
    tracing::info!("API listening on {}", config.api_addr);
    tracing::info!("Backend database: {}", config.database_url);

    // Create database pool
    let mut pg_config = PgConfig::new();
    pg_config.url = Some(config.database_url.clone());
    let pool = pg_config.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)?;

    // Create and initialize session manager
    let session_manager = Arc::new(session::SessionManager::new(pool, config.session_ttl_seconds));
    session_manager.init().await?;

    // Extract backend address from DATABASE_URL
    let backend_addr = extract_backend_addr(&config.database_url);

    // Start API server in background
    let api_config = config.clone();
    let api_session_mgr = Arc::clone(&session_manager);
    let api_handle = tokio::spawn(async move {
        if let Err(e) = api::start_server(&api_config, api_session_mgr).await {
            tracing::error!("API server error: {}", e);
        }
    });

    // Start proxy server
    let proxy_addr = config.proxy_addr.clone();
    let proxy_handle = tokio::spawn(async move {
        if let Err(e) = proxy::start_server(&proxy_addr, &backend_addr).await {
            tracing::error!("Proxy server error: {}", e);
        }
    });

    // Wait for both
    tokio::select! {
        _ = api_handle => tracing::warn!("API server stopped"),
        _ = proxy_handle => tracing::warn!("Proxy server stopped"),
    }

    Ok(())
}

/// Extract host:port from a PostgreSQL connection URL
fn extract_backend_addr(database_url: &str) -> String {
    // Parse postgresql://user:pass@host:port/db
    if let Some(after_at) = database_url.split('@').nth(1) {
        if let Some(host_port) = after_at.split('/').next() {
            return host_port.to_string();
        }
    }
    // Fallback
    "localhost:5432".to_string()
}
