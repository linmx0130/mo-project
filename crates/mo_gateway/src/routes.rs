//! HTTP routes. Same skeleton as the rite-rsjs backend prototype:
//! permissive CORS, trace layer, state behind `Arc`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as PathParam, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use mo_core::{JournalEvent, Session, SessionStatus, db};
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::error::{ApiError, ApiResult};
use crate::process;
use crate::sse;
use crate::state::AppState;

pub fn create_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::permissive();
    Router::new()
        .route("/", get(root))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}", get(get_session))
        .route("/api/sessions/{id}/history", get(history))
        .route("/api/sessions/{id}/events", get(sse::events))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn root() -> &'static str {
    "mo gateway — agent harness API"
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    workdir: String,
    prompt: String,
}

/// POST /api/sessions — validate workdir, insert the session row, spawn the
/// worker, return the session.
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateSessionRequest>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    if payload.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    let workdir = PathBuf::from(&payload.workdir);
    if !workdir.is_dir() {
        return Err(ApiError::bad_request(format!(
            "workdir is not a directory: {}",
            payload.workdir
        )));
    }
    let workdir = workdir
        .canonicalize()
        .map_err(|e| ApiError::bad_request(format!("cannot resolve workdir: {e}")))?;

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let journal_path = state
        .data_dir
        .join("sessions")
        .join(&id)
        .join("journal.jsonl");
    let model = std::env::var("MO_MODEL_NAME").unwrap_or_default();
    let mut session = Session {
        id: id.clone(),
        parent_id: None,
        workdir: workdir.display().to_string(),
        prompt: payload.prompt,
        model,
        status: SessionStatus::Pending,
        pid: None,
        journal_path: journal_path.display().to_string(),
        created_at: now.clone(),
        updated_at: now,
        heartbeat_at: None,
        error: None,
    };

    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::create_session(&conn, &session).map_err(ApiError::internal)?;
    }

    match process::spawn_worker(&state, &session) {
        Ok(pid) => {
            session.pid = Some(pid);
            let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::set_pid(&conn, &id, pid);
        }
        Err(e) => {
            warn!(session = %id, "failed to spawn worker: {e}");
            let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::update_status(
                &conn,
                &id,
                SessionStatus::Failed,
                Some(format!("failed to spawn worker: {e}")),
            );
            session.status = SessionStatus::Failed;
            session.error = Some(format!("failed to spawn worker: {e}"));
        }
    }

    Ok((StatusCode::CREATED, Json(session)))
}

/// GET /api/sessions — newest first.
async fn list_sessions(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<Session>>> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = db::list_sessions(&conn).map_err(ApiError::internal)?;
    Ok(Json(sessions))
}

/// GET /api/sessions/:id — detail plus a liveness check: a session marked
/// `running` whose pid is dead is flipped to `failed` ("worker died").
async fn get_session(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<Json<Session>> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = match db::get_session(&conn, &id).map_err(ApiError::internal)? {
        Some(session) => session,
        None => return Err(ApiError::not_found("session not found")),
    };
    if session.status == SessionStatus::Running
        && session.pid.is_some_and(|pid| !process::is_pid_alive(pid))
    {
        let _ = db::update_status(
            &conn,
            &id,
            SessionStatus::Failed,
            Some("worker died".to_string()),
        );
        session.status = SessionStatus::Failed;
        session.error = Some("worker died".to_string());
    }
    Ok(Json(session))
}

#[derive(Deserialize)]
struct HistoryQuery {
    after_seq: Option<u64>,
}

/// GET /api/sessions/:id/history?after_seq=N — journal events after N.
async fn history(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<JournalEvent>>> {
    let journal_path = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => session.journal_path,
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    let events = match query.after_seq {
        None => mo_core::read_events(Path::new(&journal_path)),
        Some(after) => mo_core::read_events_after(Path::new(&journal_path), after),
    }
    .map_err(ApiError::internal)?;
    Ok(Json(events))
}

/// POST /api/sessions/:id/cancel — SIGTERM/SIGKILL the worker tree, mark
/// the session cancelled.
async fn cancel(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<Json<Session>> {
    let (pid, status) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (session.pid, session.status),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        if let Some(pid) = pid {
            process::cancel_session_pid(pid).await;
        }
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = db::update_status(&conn, &id, SessionStatus::Cancelled, None);
    }
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let session = db::get_session(&conn, &id).map_err(ApiError::internal)?;
    match session {
        Some(session) => Ok(Json(session)),
        None => Err(ApiError::not_found("session not found")),
    }
}
