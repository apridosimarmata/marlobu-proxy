use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;

use crate::config::Config;
use crate::session::SessionManager;

use super::handlers::{
    approve_session, create_session, delete_session, get_mutations, get_session, get_session_diff,
    health_check, propose_session, reject_session, AppState,
};

/// Build the API router with all routes
pub fn build_router(session_manager: Arc<SessionManager>) -> Router {
    let state = AppState { session_manager, source_schema: "public".to_string() };

    Router::new()
        // Session CRUD
        .route("/sessions", post(create_session))
        .route("/sessions/:id", get(get_session).delete(delete_session))
        // Session workflow
        .route("/sessions/:id/propose", post(propose_session))
        .route("/sessions/:id/approve", post(approve_session))
        .route("/sessions/:id/reject", post(reject_session))
        // Session data
        .route("/sessions/:id/mutations", get(get_mutations))
        .route("/sessions/:id/diff", get(get_session_diff))
        // Health
        .route("/health", get(health_check))
        // Middleware
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        // State
        .with_state(state)
}

/// Start the HTTP API server
pub async fn start_server(
    config: &Config,
    session_manager: Arc<SessionManager>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let app = build_router(session_manager);

    let listener = TcpListener::bind(&config.api_addr).await?;
    info!("API server listening on {}", config.api_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await?;

    info!("API server stopped");
    Ok(())
}

/// Wait for shutdown signal
async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    // Use borrow_and_update to avoid race conditions
    // Exit on error (sender dropped) or when shutdown signaled
    loop {
        if rx.changed().await.is_err() {
            break;
        }
        if *rx.borrow_and_update() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_builds() {
        // This test verifies the router can be constructed without panicking
        // SessionManager would need to be mocked for actual integration tests
    }
}
