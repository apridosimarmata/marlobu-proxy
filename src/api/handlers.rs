use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::session::{Session, SessionManager, SessionStatus};

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
    pub conflicts: Vec<Conflict>,
}

#[derive(Debug, Serialize)]
pub struct Conflict {
    pub table: String,
    pub row_id: String,
    pub conflict_type: String,
    pub session_value: Option<serde_json::Value>,
    pub current_value: Option<serde_json::Value>,
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
    // TODO: Implement actual conflict detection and apply
    match state.session_manager.update_status(id, SessionStatus::Approved).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::to_value(ApproveResponse {
                status: "approved".to_string(),
                applied: 0,
            })
            .unwrap()),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::to_value(ErrorResponse {
                    error: e.to_string(),
                })
                .unwrap(),
            ),
        ),
    }
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

