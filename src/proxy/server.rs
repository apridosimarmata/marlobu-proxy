//! TCP server for Postgres wire protocol proxy.

use std::sync::Arc;
use deadpool_postgres::Pool;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::proxy::connection::Connection;

/// Proxy server configuration
pub struct ProxyServer {
    /// Address to listen on (e.g., "0.0.0.0:5433")
    listen_addr: String,
    /// Backend Postgres address (e.g., "localhost:5432")
    backend_addr: String,
    /// Database pool for infrastructure operations
    pool: Pool,
}

impl ProxyServer {
    /// Create a new proxy server
    pub fn new(listen_addr: String, backend_addr: String, pool: Pool) -> Self {
        Self {
            listen_addr,
            backend_addr,
            pool,
        }
    }

    /// Run the proxy server
    pub async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!(addr = %self.listen_addr, backend = %self.backend_addr, "Proxy server listening");

        let backend_addr = Arc::new(self.backend_addr);
        let pool = Arc::new(self.pool);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let backend = Arc::clone(&backend_addr);
                    let pool = Arc::clone(&pool);
                    tokio::spawn(async move {
                        let conn = Connection::new(stream, (*backend).clone(), pool);
                        if let Err(e) = conn.run().await {
                            error!(%peer_addr, error = %e, "Connection handler error");
                        }
                    });
                }
                Err(e) => {
                    error!(error = %e, "Failed to accept connection");
                }
            }
        }
    }
}

/// Start the proxy server (convenience function)
///
/// # Arguments
/// * `listen_addr` - Address to listen on (e.g., "0.0.0.0:5433")
/// * `backend_addr` - Backend Postgres address (e.g., "localhost:5432")
/// * `pool` - Database connection pool for infrastructure operations
pub async fn start_server(listen_addr: &str, backend_addr: &str, pool: Pool) -> anyhow::Result<()> {
    let server = ProxyServer::new(listen_addr.to_string(), backend_addr.to_string(), pool);
    server.run().await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_proxy_server_basic() {
        // Basic sanity test - actual server tests require integration testing
        assert!(true);
    }
}
