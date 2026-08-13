//! Unit tests for the `routes` module — production code lives in
//! `mo_gateway/src/routes.rs`. Wired from there with `#[cfg(test)] #[path = "tests/routes_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

/// A gateway under test: real AppState + DB in a tempdir, with a
/// no-op worker binary so `send_message`'s respawn completes without
/// running an agent.
struct TestApp {
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

fn test_app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    // A real, executable no-op worker: `spawn_and_patch` spawns it and
    // records its pid, so the session stays `pending` instead of being
    // flipped to `failed` on a missing binary.
    let worker_bin = dir.path().join("stub_worker.sh");
    std::fs::write(&worker_bin, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&worker_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&worker_bin, perms).unwrap();
    let conn = mo_core::open_db(&data_dir.join("mo.db")).unwrap();
    let state = Arc::new(AppState {
        data_dir,
        db: Mutex::new(conn),
        worker_bin,
        cwd: dir.path().to_path_buf(),
        agents_dir: dir.path().join("agents"),
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
        models: Vec::new(),
    });
    TestApp { state, _dir: dir }
}

/// Insert a terminal session row (as if created and completed) with the
/// given mode.
fn insert_session(state: &AppState, id: &str, mode: Mode) -> Session {
    let now = chrono::Utc::now().to_rfc3339();
    let session = Session {
        id: id.to_string(),
        parent_id: None,
        workdir: state.cwd.display().to_string(),
        prompt: "test session".to_string(),
        model: "mock-model".to_string(),
        status: SessionStatus::Completed,
        mode,
        pid: None,
        journal_path: state
            .data_dir
            .join("sessions")
            .join(id)
            .join("journal.jsonl")
            .display()
            .to_string(),
        created_at: now.clone(),
        updated_at: now,
        heartbeat_at: None,
        error: None,
    };
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    db::create_session(&conn, &session).unwrap();
    session
}

fn append_kinds(journal_path: &str, kinds: &[JournalEventKind]) {
    let mut journal = JournalWriter::open(Path::new(journal_path)).unwrap();
    for kind in kinds {
        journal.append(kind.clone()).unwrap();
    }
}

fn read_kinds(app: &TestApp, id: &str) -> Vec<JournalEventKind> {
    mo_core::read_events(
        &app.state
            .data_dir
            .join("sessions")
            .join(id)
            .join("journal.jsonl"),
    )
    .unwrap()
    .into_iter()
    .map(|e| e.kind)
    .collect()
}

fn user_msg(content: &str) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: "user".to_string(),
        content: content.to_string(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

fn assistant_msg(content: &str) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: "assistant".to_string(),
        content: content.to_string(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

fn system_prompt(mode: Mode) -> JournalEventKind {
    JournalEventKind::SystemPrompt {
        content: format!("You are in {:?} mode.", mode).to_lowercase(),
        mode,
    }
}

async fn send_followup(app: &Arc<AppState>, id: &str, content: &str) -> StatusCode {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/messages"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

/// The happy path of the followup journal: a mode-change notice
/// immediately before the user message, in the session's current mode.
#[tokio::test]
async fn send_message_injects_mode_change_when_switched() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    // The journal records a completed run under Build mode.
    append_kinds(
        &session.journal_path,
        &[
            user_msg("plan the thing"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    // Exactly one ModeChange (Plan — the session's current mode),
    // immediately before the followup user message.
    match &kinds[3] {
        JournalEventKind::ModeChange { mode, content } => {
            assert_eq!(*mode, Mode::Plan);
            assert!(
                content.contains("[Session mode changed to plan]"),
                "mode-change content must carry the bracket prefix: {content}"
            );
            assert!(
                content.contains("/sessions/s1/tmp"),
                "mode-change content must mention the scratch dir: {content}"
            );
        }
        other => panic!("expected mode_change at seq 3, got: {other:?}"),
    }
    match &kinds[4] {
        JournalEventKind::Message(m) if m.role == "user" && m.content == "continue" => {}
        other => panic!("expected the followup user message at seq 4, got: {other:?}"),
    }
}

/// Multiple switches before a single followup collapse into one
/// ModeChange describing the *final* mode (intermediate switches never
/// affected a run).
#[tokio::test]
async fn multiple_switches_before_one_followup_produce_one_mode_change() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Explore);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );
    // Simulate the switches: Build (last run) → Plan → Explore.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_mode(&conn, "s1", Mode::Plan).unwrap();
        db::update_mode(&conn, "s1", Mode::Explore).unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    let mode_changes: Vec<&JournalEventKind> = kinds
        .iter()
        .filter(|k| matches!(k, JournalEventKind::ModeChange { .. }))
        .collect();
    assert_eq!(mode_changes.len(), 1, "kinds: {kinds:#?}");
    match mode_changes[0] {
        JournalEventKind::ModeChange { mode, .. } => {
            assert_eq!(
                *mode,
                Mode::Explore,
                "the single notice must describe the final mode"
            );
        }
        _ => unreachable!(),
    }
    // And it sits directly before the followup user message.
    assert_eq!(kinds[4], user_msg("continue"));
}

/// No switch: the last-run mode equals the session mode, so no
/// ModeChange is journaled.
#[tokio::test]
async fn no_switch_injects_nothing() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModeChange { .. })),
        "no mode change expected: {kinds:#?}"
    );
    assert_eq!(kinds[3], user_msg("continue"));
}

/// Switching back to the mode of the last run: the journaled system
/// prompt's framing is accurate again, so no notice is needed.
#[tokio::test]
async fn switch_back_to_last_run_mode_injects_nothing() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_mode(&conn, "s1", Mode::Plan).unwrap();
        db::update_mode(&conn, "s1", Mode::Build).unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModeChange { .. })),
        "no mode change expected when switching back: {kinds:#?}"
    );
}

/// A session that never ran has no mode marker: the upcoming first run
/// builds its system prompt from the current mode, so no notice.
#[tokio::test]
async fn never_ran_session_switched_injects_nothing() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Explore);
    // Only the gateway's initial user message is journaled — the first
    // worker died before journaling the system prompt.
    append_kinds(&session.journal_path, &[user_msg("first message")]);

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 2, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModeChange { .. })),
        "no mode change expected for a never-ran session: {kinds:#?}"
    );
    assert_eq!(kinds[1], user_msg("continue"));
}

/// A previously injected ModeChange doubles as the mode marker: after a
/// Plan run (following a Build first run), switching to Explore and
/// following up injects one Explore notice — the Build system prompt is
/// not scanned past the Plan marker.
#[tokio::test]
async fn previous_mode_change_acts_as_marker() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );
    // Run 1: switched to Plan, followup -> one Plan notice + user msg.
    let status = send_followup(&app.state, "s1", "plan followup").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::clear_pid(&conn, "s1").unwrap();
        db::update_status(&conn, "s1", SessionStatus::Completed, None).unwrap();
    }
    // Simulate run 2 completing under Plan (the worker answered).
    append_kinds(&session.journal_path, &[assistant_msg("plan done")]);
    // Run 3: switch to Explore, followup.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_mode(&conn, "s1", Mode::Explore).unwrap();
    }
    let status = send_followup(&app.state, "s1", "explore followup").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    let mode_changes: Vec<&JournalEventKind> = kinds
        .iter()
        .filter(|k| matches!(k, JournalEventKind::ModeChange { .. }))
        .collect();
    // One notice for the Plan run, one for the Explore run — the Build
    // marker was never re-scanned.
    assert_eq!(mode_changes.len(), 2, "kinds: {kinds:#?}");
    let modes: Vec<Mode> = mode_changes
        .iter()
        .map(|k| match k {
            JournalEventKind::ModeChange { mode, .. } => *mode,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(modes, vec![Mode::Plan, Mode::Explore]);
    // The last two events are the Explore notice + the followup.
    match &kinds[kinds.len() - 2] {
        JournalEventKind::ModeChange { mode, .. } => assert_eq!(*mode, Mode::Explore),
        other => panic!("expected the Explore notice, got: {other:?}"),
    }
    assert_eq!(kinds[kinds.len() - 1], user_msg("explore followup"));
}

fn mode_change_request(mode: Mode, message: &str) -> JournalEventKind {
    JournalEventKind::ModeChangeRequest {
        mode,
        message: message.to_string(),
    }
}

async fn post_empty_json(
    app: &Arc<AppState>,
    id: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}{path}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// The journal of a completed Plan-mode run where the agent requested a
/// switch to Build (via request_mode_change) and the user has not
/// answered yet.
fn pending_request_journal(session: &Session) {
    append_kinds(
        &session.journal_path,
        &[
            user_msg("plan the thing"),
            system_prompt(Mode::Build),
            assistant_msg("here is the plan"),
            mode_change_request(Mode::Build, "may I switch to build mode?"),
        ],
    );
}

/// Approving a pending request switches the session's mode to the
/// requested one, journals exactly one ModeChange notice (the single
/// mode-change message the model receives to continue), and respawns
/// the worker (session back to pending).
#[tokio::test]
async fn approve_mode_change_switches_mode_and_journals_notice() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    pending_request_journal(&session);

    let (status, body) = post_empty_json(&app.state, "s1", "/mode/approve").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["mode"], "build", "body: {body}");
    assert_eq!(body["status"], "pending", "body: {body}");

    // The DB row carries the new mode.
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert_eq!(row.mode, Mode::Build);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    // Exactly one ModeChange appended after the request, with the
    // approval text (bracket prefix + scratch dir + approval sentence).
    match &kinds[4] {
        JournalEventKind::ModeChange { mode, content } => {
            assert_eq!(*mode, Mode::Build);
            assert!(
                content.contains("[Session mode changed to build]"),
                "content: {content}"
            );
            assert!(
                content.contains("approved your request"),
                "content: {content}"
            );
            // Build mode has no scratch dir to write to — the notice
            // must not send the model looking for one.
            assert!(!content.contains("/sessions/s1/tmp"), "content: {content}");
        }
        other => panic!("expected mode_change at seq 4, got: {other:?}"),
    }
    // No user message follows the notice — the notice is the single
    // message that continues the run.
    assert_eq!(kinds.len(), 5);
}

/// Approving with no pending request (the journal has no mode marker or
/// the last one is a resolution) is a conflict.
#[tokio::test]
async fn approve_mode_change_without_pending_request_conflicts() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    // Completed run, no request ever made.
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );
    let (status, _) = post_empty_json(&app.state, "s1", "/mode/approve").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(session.mode, Mode::Plan);

    // A request that was already resolved (rejected) is not pending.
    append_kinds(
        &session.journal_path,
        &[
            mode_change_request(Mode::Build, "may I?"),
            JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build },
        ],
    );
    let (status, _) = post_empty_json(&app.state, "s1", "/mode/approve").await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The mode was never switched.
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert_eq!(row.mode, Mode::Plan);
}

/// Approving while the session is running is refused.
#[tokio::test]
async fn approve_mode_change_while_running_conflicts() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    pending_request_journal(&session);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
    }
    let (status, _) = post_empty_json(&app.state, "s1", "/mode/approve").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Rejecting a pending request journals a ModeChangeRequestDeclined
/// marker and leaves the mode untouched — nothing is sent to the LLM.
#[tokio::test]
async fn reject_mode_change_journals_declined_and_keeps_mode() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    pending_request_journal(&session);

    let (status, body) = post_empty_json(&app.state, "s1", "/mode/reject").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "plan", "body: {body}");
    assert_eq!(body["status"], "completed", "body: {body}");

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    assert_eq!(
        kinds[4],
        JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build }
    );
    // No ModeChange, no user message: the request was UI-only.
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModeChange { .. }))
    );
    // The DB mode is untouched.
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert_eq!(row.mode, Mode::Plan);
}

/// Rejecting with no pending request is a conflict.
#[tokio::test]
async fn reject_mode_change_without_pending_request_conflicts() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );
    let (status, _) = post_empty_json(&app.state, "s1", "/mode/reject").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

fn ask_user_request() -> JournalEventKind {
    JournalEventKind::AskUserRequest {
        question: mo_core::AskUserQuestion {
            question_id: "q1".to_string(),
            question_title: "Select a language".to_string(),
            question_text: "Which one?".to_string(),
            options: vec![
                mo_core::AskUserOption {
                    option_title: "C++".to_string(),
                    option_text: "Fast".to_string(),
                },
                mo_core::AskUserOption {
                    option_title: "Python".to_string(),
                    option_text: "Easy".to_string(),
                },
            ],
        },
    }
}

/// POST the user's answer to a pending `ask_user_request` with a JSON body.
async fn post_ask_answer(
    app: &Arc<AppState>,
    id: &str,
    answers: &str,
) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/ask/answer"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"answers":{answers}}}"#)))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// The journal of a terminal session where the agent asked a clarification
/// question (via ask_user) and the user has not answered yet.
fn pending_ask_journal(session: &Session) {
    append_kinds(
        &session.journal_path,
        &[
            user_msg("which language?"),
            system_prompt(Mode::Build),
            assistant_msg("let me ask the user"),
            ask_user_request(),
        ],
    );
}

/// Answering a pending question (option or free text) journals an
/// `AskUserAnswered` event with the answers as a JSON object keyed by
/// question_id and respawns the worker (session back to pending).
#[tokio::test]
async fn answer_ask_user_journals_answers_and_respawns() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_ask_journal(&session);

    // Option pick: the answer value is the chosen option's title.
    let (status, body) = post_ask_answer(&app.state, "s1", r#"{"q1":"C++"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "pending", "body: {body}");
    assert!(
        body["pid"].is_number(),
        "worker should be respawned: {body}"
    );

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    match &kinds[4] {
        JournalEventKind::AskUserAnswered { answers } => {
            assert_eq!(answers.len(), 1, "answers: {answers:?}");
            assert_eq!(answers.get("q1").map(String::as_str), Some("C++"));
        }
        other => panic!("expected ask_user_answered at seq 4, got: {other:?}"),
    }

    // A second answer (the request is resolved) is a conflict.
    let (status, _) = post_ask_answer(&app.state, "s1", r#"{"q1":"Python"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Free-text answers work too (the user did not pick any option).
#[tokio::test]
async fn answer_ask_user_accepts_free_text() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_ask_journal(&session);

    let (status, _) = post_ask_answer(&app.state, "s1", r#"{"q1":"Rust"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let kinds = read_kinds(&app, "s1");
    match &kinds[4] {
        JournalEventKind::AskUserAnswered { answers } => {
            assert_eq!(answers.get("q1").map(String::as_str), Some("Rust"));
        }
        other => panic!("expected ask_user_answered, got: {other:?}"),
    }
}

/// Answering with no pending question (or an already-resolved one) is a
/// conflict; unknown ids, missing answers and empty answers are bad
/// requests.
#[tokio::test]
async fn answer_ask_user_validates_state_and_answers() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);

    // No request ever made -> conflict.
    append_kinds(&session.journal_path, &[user_msg("hi")]);
    let (status, _) = post_ask_answer(&app.state, "s1", r#"{"q1":"Rust"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A request that was already answered is not pending -> conflict.
    append_kinds(
        &session.journal_path,
        &[
            ask_user_request(),
            JournalEventKind::AskUserAnswered {
                answers: BTreeMap::from([("q1".to_string(), "Rust".to_string())]),
            },
        ],
    );
    let (status, _) = post_ask_answer(&app.state, "s1", r#"{"q1":"Rust"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Answering while the session is running is refused.
#[tokio::test]
async fn answer_ask_user_while_running_conflicts() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_ask_journal(&session);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
    }
    let (status, _) = post_ask_answer(&app.state, "s1", r#"{"q1":"Rust"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Malformed answer payloads are bad requests: unknown question ids,
/// missing answers, empty answers, and a totally empty answers object.
#[tokio::test]
async fn answer_ask_user_rejects_bad_answers() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_ask_journal(&session);

    for (answers, needle) in [
        (r#"{"q2":"Rust"}"#, "unknown question id"),
        (r#"{}"#, "missing answer"),
        (r#"{"q1":"   "}"#, "must not be empty"),
        (r#"{"q1":"Rust","q2":"Go"}"#, "unknown question id"),
    ] {
        let (status, body) = post_ask_answer(&app.state, "s1", answers).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "answers: {answers}");
        assert!(
            body.to_string().contains(needle),
            "answers: {answers}, body: {body}"
        );
    }
    // Nothing was journaled (the pending request is still pending).
    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::AskUserAnswered { .. })),
        "no answer may be journaled: {kinds:#?}"
    );
}
