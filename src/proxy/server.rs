//! TCP server for Postgres wire protocol proxy.

use deadpool_postgres::Pool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::proxy::connection::Connection;
use crate::rewriter::QueryCache;

/// Default query cache capacity
const DEFAULT_CACHE_CAPACITY: usize = 10000;

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
    /// Shared query cache
    query_cache: Arc<QueryCache>,
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
            query_cache: Arc::new(QueryCache::new(DEFAULT_CACHE_CAPACITY)),
        }
    }

    /// Run the proxy server
    pub async fn run(mut self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!(addr = %self.listen_addr, backend = %self.backend_addr, "Proxy server listening");

        let backend_addr = Arc::new(self.backend_addr);
        let pool = Arc::new(self.pool);
        let query_cache = self.query_cache;
        let active_connections = Arc::new(AtomicUsize::new(0));

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let backend = Arc::clone(&backend_addr);
                            let pool = Arc::clone(&pool);
                            let cache = Arc::clone(&query_cache);
                            let active = Arc::clone(&active_connections);

                            active.fetch_add(1, Ordering::SeqCst);

                            tokio::spawn(async move {
                                let conn = Connection::new(stream, (*backend).clone(), pool, cache);
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

        // Log cache stats before shutdown
        let stats = query_cache.stats();
        info!(
            cached_queries = stats.len,
            capacity = stats.cap,
            "Query cache stats at shutdown"
        );

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
    }
}
