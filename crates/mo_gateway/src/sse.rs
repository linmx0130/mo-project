//! SSE endpoint: per-connection tail over the session journal plus DB
//! status polling.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path as PathParam, Query, State},
    response::sse::{Event, Sse},
};
use futures_util::Stream;
use mo_core::{JournalEventKind, SessionStatus, db, read_events_after};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::error::{ApiError, ApiResult};
use crate::process;
use crate::state::AppState;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Polls with no new journal events after a terminal status before closing.
const IDLE_POLLS_TO_CLOSE: u32 = 3;

#[derive(Deserialize)]
pub struct EventsQuery {
    pub after_seq: Option<u64>,
}

/// GET /api/sessions/:id/events
///
/// Streams journal events with `seq > after_seq` as SSE `data:` payloads
/// (the whole journal when no cursor is given). Every poll also checks the
/// session row: DB-only status transitions (worker died, cancelled) are
/// synthesized as `StatusChange` events with `"seq": null`. The stream
/// closes once the status is terminal and the journal is drained.
pub async fn events(
    State(state): State<Arc<AppState>>,
    PathParam(id): PathParam<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let journal_path = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        match db::get_session(&conn, &id).map_err(ApiError::internal)? {
            Some(session) => session.journal_path,
            None => return Err(ApiError::not_found("session not found")),
        }
    };

    let db_state = state.clone();
    let stream = async_stream::stream! {
        // Cursor into the journal. `synced` is false until the first poll,
        // which reads the whole journal: it seeds journal_status (so a
        // terminal status the client already saw via history is never
        // re-synthesized) while only emitting events after the cursor.
        let mut cursor: Option<u64> = query.after_seq;
        let mut synced = false;
        let mut journal_status: Option<SessionStatus> = None;
        let mut idle_polls: u32 = 0;
        let mut interval = tokio::time::interval(POLL_INTERVAL);

        loop {
            interval.tick().await;

            // 1. Journal tail: emit new events, track last journaled status.
            let mut any_new = false;
            let events = if synced {
                read_events_after(
                    std::path::Path::new(&journal_path),
                    cursor.unwrap_or(0),
                )
            } else {
                mo_core::read_events(std::path::Path::new(&journal_path))
            };
            match events {
                Ok(events) => {
                    for event in events {
                        if let JournalEventKind::StatusChange { status, .. } = &event.kind {
                            journal_status = Some(*status);
                        }
                        if !synced
                            && cursor.is_some_and(|c| event.seq <= c)
                        {
                            continue;
                        }
                        cursor = Some(event.seq);
                        any_new = true;
                        let payload = serde_json::to_string(&event).unwrap_or_default();
                        yield Ok(Event::default().data(payload));
                    }
                    synced = true;
                }
                Err(e) => warn!(session = %id, "journal read error: {e}"),
            }

            // 2. Session row: liveness check, then synthesize DB-only
            //    status transitions (never regress the journaled status).
            let row = {
                let conn = db_state.db.lock().unwrap_or_else(|e| e.into_inner());
                db::get_session(&conn, &id).ok().flatten()
            };
            let Some(row) = row else {
                warn!(session = %id, "session row missing during SSE; closing");
                break;
            };
            if (row.status == SessionStatus::Running
                || row.status == SessionStatus::Pending)
                && row.pid.is_some_and(|pid| !process::is_pid_alive(pid))
            {
                let conn = db_state.db.lock().unwrap_or_else(|e| e.into_inner());
                let _ = db::update_status(
                    &conn,
                    &id,
                    SessionStatus::Failed,
                    Some(process::worker_died_error(&db_state.data_dir, &id)),
                );
            }
            let should_synthesize = match journal_status {
                None => row.status.rank() > SessionStatus::Pending.rank(),
                Some(js) => row.status.rank() > js.rank(),
            };
            if should_synthesize {
                journal_status = Some(row.status);
                any_new = true;
                let payload = json!({
                    "seq": null,
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "synthetic": true,
                    "kind": {
                        "kind": "status_change",
                        "status": row.status.as_str(),
                        "error": row.error,
                    }
                });
                yield Ok(Event::default().data(payload.to_string()));
            }

            // 3. Close when terminal and the journal is drained.
            if row.status.is_terminal() {
                idle_polls += 1;
                if idle_polls >= IDLE_POLLS_TO_CLOSE {
                    break;
                }
            } else if any_new {
                idle_polls = 0;
            }
        }
    };

    Ok(Sse::new(stream))
}
