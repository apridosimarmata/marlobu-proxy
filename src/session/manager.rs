#![allow(dead_code)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::map_entry)]
#![allow(clippy::unnecessary_get_then_check)]
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::schema::{SchemaError, SchemaManager};
use super::store::{SessionStore, StoreError};

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(Uuid),

    #[error("Session expired: {0}")]
    Expired(Uuid),

    #[error("Session not active: {0} (status: {1})")]
    NotActive(Uuid, String),

    #[error("Schema error: {0}")]
    Schema(#[from] SchemaError),

    #[error("Store error: {0}")]
    Store(#[from] StoreError),

    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
}

pub type SessionResult<T> = Result<T, SessionError>;

/// Session status lifecycle
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    PendingReview,
    Approved,
    Rejected,
    Expired,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::PendingReview => "pending_review",
            SessionStatus::Approved => "approved",
            SessionStatus::Rejected => "rejected",
            SessionStatus::Expired => "expired",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => SessionStatus::Active,
            "pending_review" => SessionStatus::PendingReview,
            "approved" => SessionStatus::Approved,
            "rejected" => SessionStatus::Rejected,
            "expired" => SessionStatus::Expired,
            _ => SessionStatus::Expired,
        }
    }
}

/// State of a table within a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableState {
    pub shadow_created: bool,
    pub view_created: bool,
    pub primary_key: String,
}

/// A database session with copy-on-write isolation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub project_id: String,
    pub schema_name: String,
    pub status: SessionStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub tables: HashMap<String, TableState>,
}

impl Session {
    /// Check if session is expired
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Check if session is active and not expired
    pub fn is_active(&self) -> bool {
        self.status == SessionStatus::Active && !self.is_expired()
    }
}

/// Manages session lifecycle
pub struct SessionManager {
    pool: Pool,
    store: Arc<SessionStore>,
    schema_manager: Arc<SchemaManager>,
    ttl_seconds: i64,
    source_schema: String,
}

impl SessionManager {
    pub fn new(pool: Pool, ttl_seconds: u64) -> Self {
        let store = Arc::new(SessionStore::new(pool.clone()));
        let schema_manager = Arc::new(SchemaManager::new(pool.clone()));

        Self {
            pool,
            store,
            schema_manager,
            ttl_seconds: ttl_seconds as i64,
            source_schema: "public".to_string(),
        }
    }

    /// Set the source schema (default: "public")
    pub fn with_source_schema(mut self, schema: impl Into<String>) -> Self {
        self.source_schema = schema.into();
        self
    }

    /// Initialize the session manager (create metadata tables)
    pub async fn init(&self) -> SessionResult<()> {
        self.store.init().await?;
        info!("Session manager initialized");
        Ok(())
    }

    /// Create a new session
    pub async fn create(&self, project_id: impl Into<String>) -> SessionResult<Session> {
        let project_id = project_id.into();
        let id = Uuid::new_v4();
        let schema_name = format!("session_{}", id.to_string().replace('-', "_"));
        let now = Utc::now();

        // Create the session schema with tracking tables
        self.schema_manager
            .create_session_schema(&schema_name)
            .await?;

        let session = Session {
            id,
            project_id: project_id.clone(),
            schema_name: schema_name.clone(),
            status: SessionStatus::Active,
            created_at: now,
            expires_at: now + Duration::seconds(self.ttl_seconds),
            tables: HashMap::new(),
        };

        // Persist session
        self.store.insert(session.clone()).await?;

        info!(
            session_id = %id,
            project_id = %project_id,
            schema = %schema_name,
            "Created new session"
        );

        Ok(session)
    }

    /// Get a session by ID
    pub async fn get(&self, id: Uuid) -> SessionResult<Session> {
        let session = self.store.get(id).await.map_err(|e| match e {
            StoreError::NotFound(id) => SessionError::NotFound(id),
            other => SessionError::Store(other),
        })?;

        // Check if expired
        if session.is_expired() && session.status == SessionStatus::Active {
            // Mark as expired
            let mut expired = session.clone();
            expired.status = SessionStatus::Expired;
            let _ = self.store.update(&expired).await;
            return Err(SessionError::Expired(id));
        }

        Ok(session)
    }

    /// Get an active session, returning error if not active
    pub async fn get_active(&self, id: Uuid) -> SessionResult<Session> {
        let session = self.get(id).await?;

        if !session.is_active() {
            return Err(SessionError::NotActive(
                id,
                session.status.as_str().to_string(),
            ));
        }

        Ok(session)
    }

    /// Destroy a session and clean up resources
    pub async fn destroy(&self, id: Uuid) -> SessionResult<()> {
        let session = self.store.get(id).await.map_err(|e| match e {
            StoreError::NotFound(id) => SessionError::NotFound(id),
            other => SessionError::Store(other),
        })?;

        // Drop the schema with all tables
        self.schema_manager
            .drop_session_schema(&session.schema_name)
            .await?;

        // Remove from store
        self.store.delete(id).await?;

        info!(
            session_id = %id,
            schema = %session.schema_name,
            "Destroyed session"
        );

        Ok(())
    }

    /// Create shadow table for a specific table (lazy creation)
    pub async fn create_shadow_table(
        &self,
        session_id: Uuid,
        table_name: &str,
    ) -> SessionResult<TableState> {
        let mut session = self.get_active(session_id).await?;

        // Check if already created
        if let Some(state) = session.tables.get(table_name) {
            if state.shadow_created {
                return Ok(state.clone());
            }
        }

        // Get primary key from source table
        let primary_key = self
            .schema_manager
            .get_primary_key(&self.source_schema, table_name)
            .await?;

        // Create the shadow table
        self.schema_manager
            .create_shadow_table(&session.schema_name, &self.source_schema, table_name)
            .await?;

        let state = TableState {
            shadow_created: true,
            view_created: false,
            primary_key,
        };

        // Update session
        session.tables.insert(table_name.to_string(), state.clone());
        self.store.update(&session).await?;

        debug!(
            session_id = %session_id,
            table = table_name,
            "Created shadow table"
        );

        Ok(state)
    }

    /// Ensure view exists for a table
    pub async fn ensure_view(
        &self,
        session_id: Uuid,
        table_name: &str,
    ) -> SessionResult<TableState> {
        let mut session = self.get_active(session_id).await?;

        // Check if view already created
        if let Some(state) = session.tables.get(table_name) {
            if state.view_created {
                return Ok(state.clone());
            }
        }

        // Ensure shadow table exists first
        let mut state = if session.tables.get(table_name).is_some() {
            session.tables.get(table_name).unwrap().clone()
        } else {
            self.create_shadow_table(session_id, table_name).await?
        };

        // Refresh session after potential shadow table creation
        session = self.get_active(session_id).await?;

        // Create the union view
        self.schema_manager
            .create_union_view(
                &session.schema_name,
                &self.source_schema,
                table_name,
                &state.primary_key,
            )
            .await?;

        state.view_created = true;

        // Update session
        session.tables.insert(table_name.to_string(), state.clone());
        self.store.update(&session).await?;

        debug!(
            session_id = %session_id,
            table = table_name,
            "Created union view"
        );

        Ok(state)
    }

    /// Update session status
    pub async fn update_status(
        &self,
        session_id: Uuid,
        status: SessionStatus,
    ) -> SessionResult<Session> {
        let mut session = self.store.get(session_id).await.map_err(|e| match e {
            StoreError::NotFound(id) => SessionError::NotFound(id),
            other => SessionError::Store(other),
        })?;

        session.status = status;
        self.store.update(&session).await?;

        info!(
            session_id = %session_id,
            status = session.status.as_str(),
            "Updated session status"
        );

        Ok(session)
    }

    /// Extend session TTL
    pub async fn extend(
        &self,
        session_id: Uuid,
        additional_seconds: i64,
    ) -> SessionResult<Session> {
        let mut session = self.get_active(session_id).await?;

        session.expires_at += Duration::seconds(additional_seconds);
        self.store.update(&session).await?;

        debug!(
            session_id = %session_id,
            new_expiry = %session.expires_at,
            "Extended session TTL"
        );

        Ok(session)
    }

    /// List sessions for a project
    pub async fn list_by_project(&self, project_id: &str) -> SessionResult<Vec<Session>> {
        Ok(self.store.list_by_project(project_id).await?)
    }

    /// Clean up expired sessions
    pub async fn cleanup_expired(&self) -> SessionResult<usize> {
        let sessions = self.store.list_needing_cleanup().await?;
        let count = sessions.len();

        for session in sessions {
            if let Err(e) = self.destroy(session.id).await {
                warn!(
                    session_id = %session.id,
                    status = session.status.as_str(),
                    error = %e,
                    "Failed to cleanup session"
                );
            }
        }

        if count > 0 {
            info!(count, "Cleaned up sessions");
        }

        Ok(count)
    }

    /// Get the schema name for a session
    pub fn schema_name(session_id: Uuid) -> String {
        format!("session_{}", session_id.to_string().replace('-', "_"))
    }

    /// Get access to the underlying store
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Get access to the schema manager
    pub fn schema_manager(&self) -> &SchemaManager {
        &self.schema_manager
    }

    /// Get the connection pool
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}
