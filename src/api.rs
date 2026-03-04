use std::sync::Arc;

use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::{sessions, AppState};

/// GET /api/sessions
pub async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Value> {
    let sessions = sessions::list_sessions(&state.cfg);
    Json(json!({
        "sessions": sessions,
        "total": sessions.len(),
    }))
}

/// GET /api/sessions/:id
pub async fn get_session(
    Path(session_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, StatusCode> {
    let all = sessions::list_sessions(&state.cfg);
    all.into_iter()
        .find(|s| s.session_id == session_id)
        .map(|s| Json(json!({ "session": s })))
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /api/health
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "claude-progress" }))
}
