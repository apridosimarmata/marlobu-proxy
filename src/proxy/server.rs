//! TCP server for Postgres wire protocol proxy.

use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::proxy::connection::Connection;

/// Proxy server configuration
pub struct ProxyServer {
    /// Address to listen on (e.g., "0.0.0.0:5433")
    listen_addr: String,
    /// Backend Postgres address (e.g., "localhost:5432")
    backend_addr: String,
}

impl ProxyServer {
    /// Create a new proxy server
    pub fn new(listen_addr: String, backend_addr: String) -> Self {
        Self {
            listen_addr,
            backend_addr,
        }
    }

    /// Run the proxy server
    pub async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!(addr = %self.listen_addr, backend = %self.backend_addr, "Proxy server listening");

        let backend_addr = Arc::new(self.backend_addr);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let backend = Arc::clone(&backend_addr);
                    tokio::spawn(async move {
                        let conn = Connection::new(stream, (*backend).clone());
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
///
/// # Example
/// ```ignore
/// use marlobu_proxy::proxy::start_server;
///
/// #[tokio::main]
/// async fn main() {
///     start_server("0.0.0.0:5433", "localhost:5432").await.unwrap();
/// }
/// ```
pub async fn start_server(listen_addr: &str, backend_addr: &str) -> anyhow::Result<()> {
    let server = ProxyServer::new(listen_addr.to_string(), backend_addr.to_string());
    server.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_server_creation() {
        let server = ProxyServer::new("0.0.0.0:5433".to_string(), "localhost:5432".to_string());
        assert_eq!(server.listen_addr, "0.0.0.0:5433");
        assert_eq!(server.backend_addr, "localhost:5432");
    }
}
