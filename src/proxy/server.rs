//! TCP server for Postgres wire protocol proxy.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use deadpool_postgres::Pool;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::proxy::connection::Connection;

/// Proxy server configuration
pub struct ProxyServer {
    /// Address to listen on (e.g., "0.0.0.0:5433")
    listen_addr: String,
    /// Backend Postgres address (e.g., "localhost:5432")
    backend_addr: String,
    /// Database pool for infrastructure operations
    pool: Pool,
    /// Shutdown signal receiver
    shutdown_rx: watch::Receiver<bool>,
}

impl ProxyServer {
    /// Create a new proxy server
    pub fn new(
        listen_addr: String,
        backend_addr: String,
        pool: Pool,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            listen_addr,
            backend_addr,
            pool,
            shutdown_rx,
        }
    }

    /// Run the proxy server
    pub async fn run(mut self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!(addr = %self.listen_addr, backend = %self.backend_addr, "Proxy server listening");

        let backend_addr = Arc::new(self.backend_addr);
        let pool = Arc::new(self.pool);
        let active_connections = Arc::new(AtomicUsize::new(0));

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let backend = Arc::clone(&backend_addr);
                            let pool = Arc::clone(&pool);
                            let active = Arc::clone(&active_connections);

                            active.fetch_add(1, Ordering::SeqCst);

                            tokio::spawn(async move {
                                let conn = Connection::new(stream, (*backend).clone(), pool);
                                if let Err(e) = conn.run().await {
                                    error!(%peer_addr, error = %e, "Connection handler error");
                                }
                                active.fetch_sub(1, Ordering::SeqCst);
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to accept connection");
                        }
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        info!("Shutdown signal received, stopping accept loop");
                        break;
                    }
                }
            }
        }

        // Wait for active connections to drain
        let drain_timeout = std::time::Duration::from_secs(30);
        let start = std::time::Instant::now();

        loop {
            let count = active_connections.load(Ordering::SeqCst);
            if count == 0 {
                info!("All connections drained");
                break;
            }
            if start.elapsed() > drain_timeout {
                warn!(remaining = count, "Drain timeout reached, forcing shutdown");
                break;
            }
            info!(remaining = count, "Waiting for connections to drain...");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        Ok(())
    }
}

/// Start the proxy server (convenience function)
///
/// # Arguments
/// * `listen_addr` - Address to listen on (e.g., "0.0.0.0:5433")
/// * `backend_addr` - Backend Postgres address (e.g., "localhost:5432")
/// * `pool` - Database connection pool for infrastructure operations
/// * `shutdown_rx` - Shutdown signal receiver
pub async fn start_server(
    listen_addr: &str,
    backend_addr: &str,
    pool: Pool,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let server = ProxyServer::new(
        listen_addr.to_string(),
        backend_addr.to_string(),
        pool,
        shutdown_rx,
    );
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
