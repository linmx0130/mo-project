//! HTTP routes. Same skeleton as the rite-rsjs backend prototype:
//! permissive CORS, trace layer, state behind `Arc`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as PathParam, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use mo_core::{
    AskUserMarker, JournalEvent, JournalEventKind, JournalMessage, JournalWriter, Mode, Session,
    SessionStatus, db,
};
use serde::{Deserialize, Serialize};
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
        .route("/api/meta", get(meta))
        .route("/api/models", get(list_models))
        .route("/api/modes", get(list_modes))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/history", get(history))
        .route("/api/sessions/{id}/events", get(sse::events))
        .route("/api/sessions/{id}/messages", post(send_message))
        .route("/api/sessions/{id}/mode", post(switch_mode))
        .route("/api/sessions/{id}/mode/approve", post(approve_mode_change))
        .route("/api/sessions/{id}/mode/reject", post(reject_mode_change))
        .route("/api/sessions/{id}/ask/answer", post(answer_ask_user))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn root() -> &'static str {
    "mo gateway — agent harness API"
}

#[derive(Serialize)]
struct MetaResponse {
    /// Absolute path of the directory the gateway process was started in —
    /// the frontend pre-fills it as the default session workdir.
    cwd: String,
}

/// GET /api/meta — static gateway metadata (currently just the cwd).
async fn meta(State(state): State<Arc<AppState>>) -> Json<MetaResponse> {
    Json(MetaResponse {
        cwd: state.cwd.display().to_string(),
    })
}

/// A model the user can pick when creating a session. `nickname` is the
/// optional human-readable label from the config; `default` marks the first
/// model in the config, which is what a session uses when none is chosen.
#[derive(Serialize)]
struct ModelInfo {
    nickname: Option<String>,
    name: String,
    base_url: String,
    default: bool,
}

/// GET /api/models — the configured models, first one default.
async fn list_models(State(state): State<Arc<AppState>>) -> Json<Vec<ModelInfo>> {
    let mut default = true;
    Json(
        state
            .models
            .iter()
            .map(|model| {
                let is_default = default;
                default = false;
                ModelInfo {
                    nickname: model.nickname.clone(),
                    name: model.name.clone(),
                    base_url: model.base_url.clone(),
                    default: is_default,
                }
            })
            .collect(),
    )
}

/// GET /api/modes — the built-in session modes (name, label, description,
/// tool set, write policy) for the new-session picker and the status-bar
/// switcher.
async fn list_modes() -> Json<Vec<mo_core::ModeInfo>> {
    Json(mo_core::MODES.to_vec())
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    workdir: String,
    prompt: String,
    /// Model name from `GET /api/models`; empty/absent picks the default
    /// (first) model.
    model: Option<String>,
    /// Session mode from `GET /api/modes`; empty/absent picks `build`.
    /// The mode frames the system prompt journaled on the first run and
    /// the write sandbox of every run.
    mode: Option<String>,
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
    // Resolve the mode: the client picks one by name from /api/modes;
    // absent/empty means `build` (the default, and the legacy behavior).
    let mode = match &payload.mode {
        Some(raw) if !raw.trim().is_empty() => {
            raw.parse::<Mode>().map_err(ApiError::bad_request)?
        }
        _ => Mode::Build,
    };

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let journal_path = state
        .data_dir
        .join("sessions")
        .join(&id)
        .join("journal.jsonl");
    // Resolve the model: the client picks one by name from /api/models;
    // absent/empty means the default (first) model from the config. The
    // resolved model's env vars are passed to the worker at spawn.
    let model = match &payload.model {
        Some(name) if !name.trim().is_empty() => state
            .find_model(name)
            .ok_or_else(|| ApiError::bad_request(format!("unknown model: {name}")))?,
        _ => state
            .default_model()
            .ok_or_else(|| ApiError::internal("no models configured"))?,
    };
    // The `prompt` column doubles as the session title (sidebar + header
    // render it). A new session gets a timestamped placeholder right away;
    // a separate gateway-side LLM call replaces it with a generated title
    // shortly after (see `title::generate_title`), updating the DB in place.
    // The first user message itself is journaled below, so it stays in the
    // history either way.
    let first_message = payload.prompt;
    let mut session = Session {
        id: id.clone(),
        parent_id: None,
        workdir: workdir.display().to_string(),
        prompt: crate::title::placeholder_title(),
        model: model.name.clone(),
        status: SessionStatus::Pending,
        mode,
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

    // Journal the initial user message before the worker starts: the worker
    // rebuilds its conversation context from the journal, so this is what
    // makes the journal a self-contained history (and followups work the
    // same way, via POST /api/sessions/:id/messages).
    {
        let mut journal =
            JournalWriter::open(Path::new(&session.journal_path)).map_err(ApiError::internal)?;
        journal
            .append(JournalEventKind::Message(JournalMessage {
                role: "user".to_string(),
                content: first_message.clone(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
            .map_err(ApiError::internal)?;
    }

    spawn_and_patch(&state, &mut session);

    // Fire-and-forget title generation: a short, separate LLM call so the
    // session does not stay stuck with the placeholder when a model is
    // configured. The DB (and with it the sidebar/header) updates when the
    // title lands; failures keep the placeholder.
    crate::title::spawn_title_generation(state.clone(), id.clone(), first_message);

    Ok((StatusCode::CREATED, Json(session)))
}

/// Spawn the worker for a session and update the session struct with the
/// resulting pid — or mark it `failed` (DB + struct) when spawning fails.
fn spawn_and_patch(state: &AppState, session: &mut Session) {
    let id = session.id.clone();
    match process::spawn_worker(state, session) {
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
}

#[derive(Deserialize)]
struct SendMessageRequest {
    content: String,
}

/// POST /api/sessions/:id/messages — continue a terminal session with a new
/// user message: journal it, reset the session to `pending`, and spawn a
/// fresh worker that picks up the full journal history.
async fn send_message(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
    Json(payload): Json<SendMessageRequest>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    if payload.content.trim().is_empty() {
        return Err(ApiError::bad_request("message must not be empty"));
    }
    let (status, pid, journal_path, mode) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (
                session.status,
                session.pid,
                session.journal_path,
                session.mode,
            ),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        return Err(ApiError::conflict("session is already running"));
    }
    // A just-cancelled session's worker may still be dying (cancel records
    // the status before the kill finishes); refuse to overlap it.
    if pid.is_some_and(process::is_pid_alive) {
        return Err(ApiError::conflict("session worker is still shutting down"));
    }

    // Journal the user message; the worker's context is rebuilt from the
    // journal, so this is what continues the conversation.
    //
    // The session's mode may have been switched since the last run. The
    // system prompt is journaled once and never rebuilt, so the model would
    // otherwise keep the old mode's framing. When the mode differs from the
    // mode of the last run, inject a single mode-change message right before
    // the followup. "Mode of the last run" is the last *mode marker* in the
    // journal — the journaled `SystemPrompt` (mode of the first run) or a
    // previously injected `ModeChange` (every run after one happened under
    // its mode), scanned from the end. Consequences:
    //   - multiple switches before one followup → one message, describing
    //     the current mode (intermediate switches never affected a run);
    //   - switching back to the mode of the last run → no message;
    //   - a session that never ran has no marker → no message (the upcoming
    //     first run builds its system prompt from the current mode).
    {
        let mut journal =
            JournalWriter::open(Path::new(&journal_path)).map_err(ApiError::internal)?;
        let last_mode = mo_core::read_events(Path::new(&journal_path))
            .map_err(ApiError::internal)?
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                JournalEventKind::SystemPrompt { mode, .. } => Some(*mode),
                JournalEventKind::ModeChange { mode, .. } => Some(*mode),
                _ => None,
            });
        if last_mode.is_some_and(|last| last != mode) {
            // The session scratch dir (`<data_dir>/sessions/<id>/tmp`, the
            // same deterministic path the worker creates and canonicalizes
            // on the first run) — embedded in the message text so the model
            // is told where it *may* write.
            let scratch = state.data_dir.join("sessions").join(&id).join("tmp");
            let scratch = scratch.canonicalize().unwrap_or(scratch);
            journal
                .append(JournalEventKind::ModeChange {
                    mode,
                    content: mo_core::mode_change_message(mode, &scratch),
                })
                .map_err(ApiError::internal)?;
        }
        journal
            .append(JournalEventKind::Message(JournalMessage {
                role: "user".to_string(),
                content: payload.content.clone(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
            .map_err(ApiError::internal)?;
    }

    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        // Clear the stale pid of the dead worker so the liveness check does
        // not flip the freshly-queued session to `failed`, then queue it.
        // The title is untouched: followups never rename a session (new
        // sessions get their title from the gateway-side generator at
        // creation).
        db::clear_pid(&conn, &id).map_err(ApiError::internal)?;
        db::update_status(&conn, &id, SessionStatus::Pending, None).map_err(ApiError::internal)?;
    }

    let mut session = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, &id)
            .map_err(ApiError::internal)?
            .expect("session row exists")
    };
    spawn_and_patch(&state, &mut session);

    Ok((StatusCode::ACCEPTED, Json(session)))
}

#[derive(Deserialize)]
struct SwitchModeRequest {
    mode: String,
}

/// POST /api/sessions/:id/mode — switch the session's mode once it is
/// terminal. The system prompt is journaled at the first run and is never
/// rebuilt, so switching only changes the write-sandbox policy of
/// subsequent runs (Build = codebase writable; Plan/Explore = codebase
/// read-only, writes go to the session scratch dir).
async fn switch_mode(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
    Json(payload): Json<SwitchModeRequest>,
) -> ApiResult<Json<Session>> {
    let mode = payload
        .mode
        .parse::<Mode>()
        .map_err(ApiError::bad_request)?;
    let status = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => session.status,
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        return Err(ApiError::conflict(
            "cannot switch mode while the session is running",
        ));
    }
    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_mode(&conn, &id, mode).map_err(ApiError::internal)?;
    }
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let session = db::get_session(&conn, &id)
        .map_err(ApiError::internal)?
        .expect("session row exists");
    Ok(Json(session))
}

/// POST /api/sessions/:id/mode/approve — the user approved a pending
/// `request_mode_change` request in the UI (the agent asked, via the
/// `request_mode_change` tool, to switch the session's mode).
///
/// Switches the session's mode to the requested one, journals a single
/// `ModeChange` notice (the "single mode change message" the model receives
/// to continue the task in the new mode), and respawns the worker. The
/// session must be terminal and the journal's last mode marker must be a
/// pending request (a `ModeChangeRequest` with no `ModeChange` /
/// `ModeChangeRequestDeclined` after it).
async fn approve_mode_change(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let (status, pid, journal_path) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (session.status, session.pid, session.journal_path),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        return Err(ApiError::conflict("session is already running"));
    }
    // A just-cancelled session's worker may still be dying; refuse to
    // overlap it, exactly like `send_message` does.
    if pid.is_some_and(process::is_pid_alive) {
        return Err(ApiError::conflict("session worker is still shutting down"));
    }
    let events = mo_core::read_events(Path::new(&journal_path)).map_err(ApiError::internal)?;
    let requested = match mo_core::last_mode_marker(&events) {
        Some(mo_core::ModeMarker::RequestPending { mode }) => mode,
        _ => {
            return Err(ApiError::conflict(
                "no pending mode change request to approve",
            ));
        }
    };

    // Switch the session's mode: the upcoming worker run's write sandbox
    // follows the new mode. The journaled system prompt stays as journaled
    // (never rebuilt), which is why the ModeChange notice below is needed.
    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_mode(&conn, &id, requested).map_err(ApiError::internal)?;
    }
    // The session scratch dir (`<data_dir>/sessions/<id>/tmp`, the same
    // deterministic path the worker creates and canonicalizes on the first
    // run) — embedded in the notice text so the model is told where it
    // *may* write, exactly like the followup mode-change notice.
    let scratch = state.data_dir.join("sessions").join(&id).join("tmp");
    let scratch = scratch.canonicalize().unwrap_or(scratch);
    {
        let mut journal =
            JournalWriter::open(Path::new(&journal_path)).map_err(ApiError::internal)?;
        // The single mode-change message: the worker maps this event to a
        // user-role message, so the model sees the new mode's framing and
        // the approval directly before continuing the task.
        journal
            .append(JournalEventKind::ModeChange {
                mode: requested,
                content: mo_core::mode_change_approved_message(requested, &scratch),
            })
            .map_err(ApiError::internal)?;
    }

    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::clear_pid(&conn, &id).map_err(ApiError::internal)?;
        db::update_status(&conn, &id, SessionStatus::Pending, None).map_err(ApiError::internal)?;
    }

    let mut session = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, &id)
            .map_err(ApiError::internal)?
            .expect("session row exists")
    };
    spawn_and_patch(&state, &mut session);

    Ok((StatusCode::ACCEPTED, Json(session)))
}

/// POST /api/sessions/:id/mode/reject — the user rejected a pending
/// `request_mode_change` request in the UI.
///
/// Journals a `ModeChangeRequestDeclined` marker that resolves the request
/// (the session's mode is *not* switched and nothing is sent to the LLM —
/// the request was UI-only), so the request stops being pending and the
/// frontend unfreezes the composer. The session stays terminal in its
/// current mode. 409 unless the journal's last mode marker is a pending
/// request.
async fn reject_mode_change(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<Json<Session>> {
    let (status, pid, journal_path) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (session.status, session.pid, session.journal_path),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        return Err(ApiError::conflict("session is already running"));
    }
    if pid.is_some_and(process::is_pid_alive) {
        return Err(ApiError::conflict("session worker is still shutting down"));
    }
    let events = mo_core::read_events(Path::new(&journal_path)).map_err(ApiError::internal)?;
    let requested = match mo_core::last_mode_marker(&events) {
        Some(mo_core::ModeMarker::RequestPending { mode }) => mode,
        _ => {
            return Err(ApiError::conflict(
                "no pending mode change request to reject",
            ));
        }
    };
    {
        let mut journal =
            JournalWriter::open(Path::new(&journal_path)).map_err(ApiError::internal)?;
        journal
            .append(JournalEventKind::ModeChangeRequestDeclined { mode: requested })
            .map_err(ApiError::internal)?;
    }
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let session = db::get_session(&conn, &id)
        .map_err(ApiError::internal)?
        .expect("session row exists");
    Ok(Json(session))
}

#[derive(Deserialize)]
struct AnswerAskUserRequest {
    /// The answers as a JSON object keyed by `question_id`; each value is
    /// the chosen option's `option_title` or the user's typed text.
    answers: BTreeMap<String, String>,
}

/// POST /api/sessions/:id/ask/answer — the user answered a pending
/// `ask_user_request` in the UI (picked an option or typed free text).
///
/// Validates the answers against the pending question (exactly its
/// `question_id`, non-empty, no unknown ids), journals an `AskUserAnswered`
/// event, and respawns the worker — the history rebuild maps the event to a
/// user-role message carrying the answers, so the model continues with the
/// answer. The session must be terminal and the journal's last ask-user
/// marker must be a pending request.
async fn answer_ask_user(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
    Json(payload): Json<AnswerAskUserRequest>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let (status, pid, journal_path) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (session.status, session.pid, session.journal_path),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    if status == SessionStatus::Running || status == SessionStatus::Pending {
        return Err(ApiError::conflict("session is already running"));
    }
    if pid.is_some_and(process::is_pid_alive) {
        return Err(ApiError::conflict("session worker is still shutting down"));
    }
    let events = mo_core::read_events(Path::new(&journal_path)).map_err(ApiError::internal)?;
    // The pending question: the last ask-user marker is a request, and the
    // request whose id the answer keys. (Stage 1: one question per request.)
    let question_id = match mo_core::last_ask_user_marker(&events) {
        Some(AskUserMarker::RequestPending) => events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                JournalEventKind::AskUserRequest { question } => Some(question.question_id.clone()),
                _ => None,
            })
            .expect("a pending ask-user marker implies an ask_user_request"),
        _ => {
            return Err(ApiError::conflict(
                "no pending clarification question to answer",
            ));
        }
    };
    // Validate: every answer must be non-empty and key the pending question;
    // unknown ids are rejected (there is exactly one question to answer).
    let mut answers = BTreeMap::new();
    for (key, value) in &payload.answers {
        if *key != question_id {
            return Err(ApiError::bad_request(format!(
                "unknown question id: {key} (the pending question is {question_id})"
            )));
        }
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ApiError::bad_request(format!(
                "answer for {key} must not be empty"
            )));
        }
        answers.insert(key.clone(), trimmed.to_string());
    }
    if !answers.contains_key(&question_id) {
        return Err(ApiError::bad_request(format!(
            "missing answer for {question_id}"
        )));
    }

    {
        let mut journal =
            JournalWriter::open(Path::new(&journal_path)).map_err(ApiError::internal)?;
        journal
            .append(JournalEventKind::AskUserAnswered { answers })
            .map_err(ApiError::internal)?;
    }

    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::clear_pid(&conn, &id).map_err(ApiError::internal)?;
        db::update_status(&conn, &id, SessionStatus::Pending, None).map_err(ApiError::internal)?;
    }

    let mut session = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, &id)
            .map_err(ApiError::internal)?
            .expect("session row exists")
    };
    spawn_and_patch(&state, &mut session);

    Ok((StatusCode::ACCEPTED, Json(session)))
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<Session>>> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = db::list_sessions(&conn).map_err(ApiError::internal)?;
    Ok(Json(sessions))
}

/// GET /api/sessions/:id — detail plus a liveness check: a session marked
/// `running` whose worker died (pid gone) or stalled (process alive but no
/// heartbeats) is flipped to `failed`.
async fn get_session(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<Json<Session>> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = match db::get_session(&conn, &id).map_err(ApiError::internal)? {
        Some(session) => session,
        None => return Err(ApiError::not_found("session not found")),
    };
    // Liveness check: a session whose worker died is flipped to failed,
    // whether it ever reached `running` or not; a worker that is alive but
    // stopped heartbeating (wedged async runtime) is flipped too, so the
    // session never gets stuck at `running` forever.
    if session.status == SessionStatus::Running || session.status == SessionStatus::Pending {
        let dead = session.pid.is_some_and(|pid| !process::is_pid_alive(pid));
        let stalled = process::is_heartbeat_stale(&session.heartbeat_at);
        if dead || stalled {
            let error = if dead {
                process::worker_died_error(&state.data_dir, &id)
            } else {
                process::worker_stalled_error(&state.data_dir, &id, &session.heartbeat_at)
            };
            let _ = db::update_status(&conn, &id, SessionStatus::Failed, Some(error.clone()));
            session.status = SessionStatus::Failed;
            session.error = Some(error);
        }
    }
    Ok(Json(session))
}

/// DELETE /api/sessions/:id — permanently remove a session: stop a running
/// worker (SIGTERM → SIGKILL its process group), delete the per-session
/// directory from disk (journal, worker log, ...), then drop the DB row.
/// Subagent sessions spawned by it are removed with it (rows + directories)
/// — the group kill already stops their workers, and they are hidden from
/// the session list, so the user could not otherwise reach them.
/// Returns 204 No Content on success, 404 for an unknown session.
async fn delete_session(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
) -> ApiResult<StatusCode> {
    let (pid, status) = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => (session.pid, session.status),
            None => return Err(ApiError::not_found("session not found")),
        }
    };
    // Stop a live worker before touching its files so nothing keeps writing
    // to the journal while the directory is removed.
    if (status == SessionStatus::Running || status == SessionStatus::Pending)
        && let Some(pid) = pid
    {
        process::cancel_session_pid(pid).await;
    }
    // Remove the subagent trees first: a failure here leaves the session
    // fully intact (rows + files), so the client can retry.
    delete_descendants(&state, &id);
    // Remove the per-session directory first: a failure here leaves the
    // session fully intact (row + files), so the client can retry.
    process::remove_session_dir(&state.data_dir, &id)
        .map_err(|e| ApiError::internal(format!("failed to remove session directory: {e}")))?;
    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::delete_session(&conn, &id).map_err(ApiError::internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Recursively remove a session's subagents (children, grandchildren, …):
/// their per-session directories and DB rows are deleted with the parent so
/// no orphaned subagent rows accumulate (subagents are hidden from the
/// session list). Their workers are already dead — subagent workers inherit
/// the parent worker's process group, which `cancel_session_pid` killed.
fn delete_descendants(state: &AppState, id: &str) {
    let children = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::list_children(&conn, id).unwrap_or_default()
    };
    for child in children {
        delete_descendants(state, &child.id);
        let _ = process::remove_session_dir(&state.data_dir, &child.id);
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = db::delete_session(&conn, &child.id);
    }
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
        // Mark the session cancelled *before* killing: the kill has a grace
        // period, and while it is in flight the liveness check would see a
        // dead pid with a `running` status and race the session to `failed`.
        // The session's subagents are marked cancelled too (their workers
        // die with the parent's process group), so their rows never stay
        // `running` with a dead pid.
        {
            let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::update_status(&conn, &id, SessionStatus::Cancelled, None);
        }
        cancel_descendants(&state, &id);
        if let Some(pid) = pid {
            process::cancel_session_pid(pid).await;
        }
    }
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let session = db::get_session(&conn, &id).map_err(ApiError::internal)?;
    match session {
        Some(session) => Ok(Json(session)),
        None => Err(ApiError::not_found("session not found")),
    }
}

/// Recursively mark a session's subagents (children, grandchildren, …)
/// `cancelled`. The process-group kill in `cancel_session_pid` already
/// stops their workers — subagent workers inherit the parent worker's
/// process group — so this only keeps their DB rows from staying
/// `running` with a dead pid (and lets a subagent modal's SSE show
/// `cancelled` instead of flipping to `failed`).
fn cancel_descendants(state: &AppState, id: &str) {
    let children = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::list_children(&conn, id).unwrap_or_default()
    };
    for child in children {
        if child.status == SessionStatus::Running || child.status == SessionStatus::Pending {
            let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::update_status(&conn, &child.id, SessionStatus::Cancelled, None);
        }
        cancel_descendants(state, &child.id);
    }
}

// Unit tests live in `mo_gateway/src/tests/routes_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/routes_tests.rs"]
mod tests;
