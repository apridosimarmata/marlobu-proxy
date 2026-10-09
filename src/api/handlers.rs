use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

use crate::approval::conflict::{check_row_hash_conflicts, ConflictType};
use crate::approval::diff::{generate_session_diff, SessionDiff};
use crate::session::{Session, SessionManager, SessionStatus};
// ============================================================================
// SQL Identifier Validation
// ============================================================================

fn validate_identifier(ident: &str) -> Result<(), String> {
    if ident.is_empty() { return Err("identifier cannot be empty".to_string()); }
    if !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!("identifier contains invalid characters: {}", ident));
    }
    Ok(())
}

fn safe_quote_ident(ident: &str) -> Result<String, String> {
    validate_identifier(ident)?;
    Ok(format!("\"{}\"", ident.replace('"', "\"\""))
}

// ============================================================================
// Application State
// ============================================================================

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub session_manager: Arc<SessionManager>,
}

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub project_id: String,
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub schema_name: String,
    pub expires_at: DateTime<Utc>,
    pub connection_string: String,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub session_id: String,
    pub project_id: String,
    pub schema_name: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ApproveResponse {
    pub status: String,
    pub applied: usize,
}

#[derive(Debug, Serialize)]
pub struct ConflictResponse {
    pub status: String,
    pub conflicts: Vec<ConflictDetail>,
}

#[derive(Debug, Serialize)]
pub struct ConflictDetail {
    pub table: String,
    pub pk_value: String,
    pub conflict_type: String,
    pub captured_hash: String,
    pub current_hash: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MutationsResponse {
    pub session_id: String,
    pub mutations: Vec<Mutation>,
}

#[derive(Debug, Serialize)]
pub struct Mutation {
    pub id: String,
    pub table: String,
    pub operation: String,
    pub row_id: Option<String>,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /sessions - Create a new session
pub async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let session = state
        .session_manager
        .create(&request.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: session.id.to_string(),
            schema_name: session.schema_name.clone(),
            expires_at: session.expires_at,
            connection_string: format!(
                "postgresql://postgres:postgres@localhost:5433/marlobu_playground?options=-c%20marlobu_session%3D{}",
                session.id
            ),
        }),
    ))
}

/// GET /sessions/:id - Get session details
pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let session = state
        .session_manager
        .get(id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

    Ok(Json(SessionResponse {
        session_id: session.id.to_string(),
        project_id: session.project_id.clone(),
        schema_name: session.schema_name.clone(),
        status: session.status.as_str().to_string(),
        created_at: session.created_at,
        expires_at: session.expires_at,
    }))
}

/// DELETE /sessions/:id - Delete session
pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.session_manager.destroy(id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    Ok(Json(MessageResponse {
        message: format!("Session {} deleted", id),
    }))
}

/// POST /sessions/:id/propose - Submit session for review
pub async fn propose_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .session_manager
        .update_status(id, SessionStatus::PendingReview)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

    Ok(Json(MessageResponse {
        message: format!("Session {} submitted for review", id),
    }))
}

/// POST /sessions/:id/approve - Approve and apply session changes
pub async fn approve_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    // 1. Get session and verify it's in PendingReview status
    let session = match state.session_manager.get(id).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::to_value(ErrorResponse {
                    error: e.to_string(),
                }).unwrap()),
            );
        }
    };

    if session.status != SessionStatus::PendingReview {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::to_value(ErrorResponse {
                error: format!(
                    "Session must be in pending_review status to approve (current: {})",
                    session.status.as_str()
                ),
            }).unwrap()),
        );
    }

    // 2. Get database client from pool
    let client = match state.session_manager.pool().get().await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to get database connection");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(ErrorResponse {
                    error: "Database connection error".to_string(),
                }).unwrap()),
            );
        }
    };

    // 3. Check for conflicts using row hash comparison
    let conflicts = match check_row_hash_conflicts(&client, &session.schema_name, "public").await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, session_id = %id, "Failed to check conflicts");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(ErrorResponse {
                    error: format!("Conflict check failed: {}", e),
                }).unwrap()),
            );
        }
    };

    // 4. If conflicts exist, return them to client
    if !conflicts.is_empty() {
        info!(
            session_id = %id,
            conflict_count = conflicts.len(),
            "Conflicts detected, cannot approve"
        );

        let conflict_details: Vec<ConflictDetail> = conflicts
            .into_iter()
            .map(|c| ConflictDetail {
                table: c.table,
                pk_value: c.pk_value,
                conflict_type: match c.conflict_type {
                    ConflictType::RowModified => "row_modified".to_string(),
                    ConflictType::RowDeleted => "row_deleted".to_string(),
                    ConflictType::InsertCollision => "insert_collision".to_string(),
                },
                captured_hash: c.captured_hash,
                current_hash: c.current_hash,
            })
            .collect();

        return (
            StatusCode::CONFLICT,
            Json(serde_json::to_value(ConflictResponse {
                status: "conflicts".to_string(),
                conflicts: conflict_details,
            }).unwrap()),
        );
    }

    // 5. No conflicts - apply changes from shadow to production
    let applied = match apply_shadow_to_production(&client, &session).await {
        Ok(count) => count,
        Err(e) => {
            error!(error = %e, session_id = %id, "Failed to apply changes");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(ErrorResponse {
                    error: format!("Failed to apply changes: {}", e),
                }).unwrap()),
            );
        }
    };

    // 6. Update session status to Approved
    if let Err(e) = state.session_manager.update_status(id, SessionStatus::Approved).await {
        error!(error = %e, session_id = %id, "Failed to update session status");
        // Changes already applied, log error but continue
    }

    // 7. Drop the session schema after successful apply
    if let Err(e) = state.session_manager.schema_manager().drop_session_schema(&session.schema_name).await {
        error!(error = %e, session_id = %id, "Failed to drop session schema");
        // Not critical, schema can be cleaned up later
    }

    info!(
        session_id = %id,
        applied = applied,
        "Session approved and changes applied"
    );

    (
        StatusCode::OK,
        Json(serde_json::to_value(ApproveResponse {
            status: "approved".to_string(),
            applied,
        }).unwrap()),
    )
}

/// Apply shadow table changes to production
async fn apply_shadow_to_production(
    client: &deadpool_postgres::Client,
    session: &Session,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let mut total_applied = 0;

    for (table_name, table_state) in &session.tables {
        if !table_state.shadow_created {
            continue;
        }

        let pk = &table_state.primary_key;
        let schema = &session.schema_name;

        let quoted_table = safe_quote_ident(table_name).map_err(|e| format!("Invalid table: {}", e))?;
        let quoted_schema = safe_quote_ident(schema).map_err(|e| format!("Invalid schema: {}", e))?;
        let quoted_pk = safe_quote_ident(pk).map_err(|e| format!("Invalid pk: {}", e))?;

        // Apply INSERTs (rows in shadow not in prod)
        let insert_sql = format!(
            r#"
            INSERT INTO public.{table}
            SELECT s.* FROM {schema}.{table} s
            WHERE NOT EXISTS (
                SELECT 1 FROM public.{table} p WHERE p.{pk} = s.{pk}
            )
            "#,
            schema = quoted_schema,
            table = quoted_table,
            pk = quoted_pk,
        );
        let inserted = client.execute(&insert_sql, &[]).await?;
        total_applied += inserted as usize;

        // Apply UPDATEs (rows in both shadow and prod)
        // Get columns for update
        let columns: Vec<String> = client
            .query(
                r#"
                SELECT column_name
                FROM information_schema.columns
                WHERE table_schema = 'public'
                  AND table_name = $1
                  AND column_name != $2
                ORDER BY ordinal_position
                "#,
                &[&table_name, &pk],
            )
            .await?
            .iter()
            .map(|r| r.get("column_name"))
            .collect();

        if !columns.is_empty() {
            let set_clause = columns
                .iter()
                .map(|col| {
                    let q = safe_quote_ident(col)?;
                    Ok(format!("{} = s.{}", q, q))
                })
                .collect::<Result<Vec<_>, String>>()?
                .join(", ");

            let update_sql = format!(
                r#"
                UPDATE public.{table} p
                SET {set_clause}
                FROM {schema}.{table} s
                WHERE p.{pk} = s.{pk}
                "#,
                schema = quoted_schema,
                table = quoted_table,
                pk = quoted_pk,
                set_clause = set_clause,
            );
            let updated = client.execute(&update_sql, &[]).await?;
            total_applied += updated as usize;
        }

        // Apply DELETEs (check _mlb_deletes table if it exists)
        let delete_sql = format!(
            r#"
            DELETE FROM public.{table}
            WHERE {pk}::text IN (
                SELECT pk_value FROM "{schema}"._mlb_deletes WHERE table_name = $1
            )
            "#,
            schema = quoted_schema,
            table = quoted_table,
            pk = quoted_pk,
        );
        // Ignore error if _mlb_deletes doesn't exist
        if let Ok(deleted) = client.execute(&delete_sql, &[&table_name]).await {
            total_applied += deleted as usize;
        }
    }

    Ok(total_applied)
}

/// POST /sessions/:id/reject - Reject and discard session
pub async fn reject_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .session_manager
        .update_status(id, SessionStatus::Rejected)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

    Ok(Json(MessageResponse {
        message: format!("Session {} rejected and discarded", id),
    }))
}

/// GET /sessions/:id/mutations - Get staged mutations for session
pub async fn get_mutations(
    State(_state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MutationsResponse>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement mutation tracking
    Ok(Json(MutationsResponse {
        session_id: id.to_string(),
        mutations: vec![],
    }))
}

/// GET /health - Health check endpoint
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        timestamp: Utc::now(),
    })
}
