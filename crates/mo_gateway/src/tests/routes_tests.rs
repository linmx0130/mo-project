//! Unit tests for the `routes` module — production code lives in
//! `mo_gateway/src/routes.rs`. Wired from there with `#[cfg(test)] #[path = "tests/routes_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use mo_core::PermissionRequestItem;
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
        // Three models so the model-switch endpoint has something to switch
        // to/from (and a collapse test can go mock → other → third);
        // sessions are inserted with `mock-model`.
        models: vec![
            mo_core::ModelConfig {
                base_url: "http://127.0.0.1:9001".into(),
                name: "mock-model".into(),
                token: None,
                nickname: None,
                context_window: None,
            },
            mo_core::ModelConfig {
                base_url: "http://127.0.0.1:9002".into(),
                name: "other-model".into(),
                token: None,
                nickname: None,
                context_window: None,
            },
            mo_core::ModelConfig {
                base_url: "http://127.0.0.1:9003".into(),
                name: "third-model".into(),
                token: None,
                nickname: None,
                context_window: None,
            },
        ],
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
        tools: vec![],
        skills: vec![],
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
        model: "mock-model".to_string(),
    }
}

fn model_change(from: &str, to: &str) -> JournalEventKind {
    JournalEventKind::ModelChange {
        from: from.to_string(),
        to: to.to_string(),
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

fn permission_request() -> JournalEventKind {
    JournalEventKind::PermissionRequest {
        request_id: "p1".to_string(),
        tool: None,
        operation: None,
        path: None,
        items: vec![PermissionRequestItem {
            call_id: "call_1".to_string(),
            tool: "read_file".to_string(),
            operation: "read".to_string(),
            path: "/etc/hostname".to_string(),
            arguments: r#"{"path":"/etc/hostname"}"#.to_string(),
        }],
    }
}

/// A pre-batch single-item request (no `items` field): journals written
/// before permission batching.
fn legacy_permission_request() -> JournalEventKind {
    JournalEventKind::PermissionRequest {
        request_id: "p1".to_string(),
        tool: Some("read_file".to_string()),
        operation: Some("read".to_string()),
        path: Some("/etc/hostname".to_string()),
        items: Vec::new(),
    }
}

fn permission_answered(allowed: bool) -> JournalEventKind {
    JournalEventKind::PermissionAnswered {
        request_id: "p1".to_string(),
        tool: None,
        operation: None,
        path: None,
        allowed: None,
        decisions: vec![PermissionDecision {
            call_id: "call_1".to_string(),
            tool: "read_file".to_string(),
            operation: "read".to_string(),
            path: "/etc/hostname".to_string(),
            allowed,
        }],
    }
}

/// POST the user's decisions on a pending `permission_request` with a JSON
/// body. `decisions` maps each held call's id to Allow/Deny (the batched
/// contract); pass `allowed` instead for the legacy single-item contract.
async fn post_permission_answer(
    app: &Arc<AppState>,
    id: &str,
    request_id: &str,
    decisions: Option<&BTreeMap<String, bool>>,
    allowed: Option<bool>,
) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let mut body = serde_json::json!({ "request_id": request_id });
    if let Some(decisions) = decisions {
        body["decisions"] = serde_json::to_value(decisions).unwrap();
    }
    if let Some(allowed) = allowed {
        body["allowed"] = serde_json::json!(allowed);
    }
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/permission/answer"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// The journal of a terminal session where file tools requested access to
/// paths outside the allowed roots (one batched request with two items)
/// and the user has not decided yet.
fn pending_permission_journal(session: &Session) {
    append_kinds(
        &session.journal_path,
        &[
            user_msg("read the file"),
            system_prompt(Mode::Build),
            assistant_msg("let me read it"),
            JournalEventKind::PermissionRequest {
                request_id: "p1".to_string(),
                tool: None,
                operation: None,
                path: None,
                items: vec![
                    PermissionRequestItem {
                        call_id: "call_1".to_string(),
                        tool: "read_file".to_string(),
                        operation: "read".to_string(),
                        path: "/etc/hostname".to_string(),
                        arguments: r#"{"path":"/etc/hostname"}"#.to_string(),
                    },
                    PermissionRequestItem {
                        call_id: "call_2".to_string(),
                        tool: "read_file".to_string(),
                        operation: "read".to_string(),
                        path: "/etc/passwd".to_string(),
                        arguments: r#"{"path":"/etc/passwd"}"#.to_string(),
                    },
                ],
            },
        ],
    );
}

/// Answering a pending batched permission request (Allow / Deny per path)
/// journals a `PermissionAnswered` carrying one `PermissionDecision` per
/// item, and respawns the worker (session back to pending).
#[tokio::test]
async fn answer_permission_journals_decision_and_respawns() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);

    // Allow call_1, deny call_2.
    let decisions = BTreeMap::from([("call_1".to_string(), true), ("call_2".to_string(), false)]);
    let (status, body) =
        post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "pending", "body: {body}");
    assert!(
        body["pid"].is_number(),
        "worker should be respawned: {body}"
    );

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    match &kinds[4] {
        JournalEventKind::PermissionAnswered {
            request_id,
            decisions,
            ..
        } => {
            assert_eq!(request_id, "p1");
            assert_eq!(decisions.len(), 2, "decisions: {decisions:#?}");
            assert_eq!(decisions[0].call_id, "call_1");
            assert_eq!(decisions[0].tool, "read_file");
            assert_eq!(decisions[0].operation, "read");
            assert_eq!(decisions[0].path, "/etc/hostname");
            assert!(decisions[0].allowed);
            assert_eq!(decisions[1].call_id, "call_2");
            assert_eq!(decisions[1].path, "/etc/passwd");
            assert!(!decisions[1].allowed);
        }
        other => panic!("expected permission_answered at seq 4, got: {other:?}"),
    }

    // A second answer (the request is resolved) is a conflict.
    let (status, _) = post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Deny journals the same event with `allowed: false` per decision.
#[tokio::test]
async fn answer_permission_denies() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);

    let decisions = BTreeMap::from([("call_1".to_string(), false), ("call_2".to_string(), false)]);
    let (status, _) = post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let kinds = read_kinds(&app, "s1");
    match &kinds[4] {
        JournalEventKind::PermissionAnswered { decisions, .. } => {
            assert!(decisions.iter().all(|d| !d.allowed));
        }
        other => panic!("expected permission_answered, got: {other:?}"),
    }
}

/// Answering with no pending request (or an already-resolved one) is a
/// conflict; a mismatched request id is a bad request; batched decisions
/// must cover exactly the pending items (missing or unknown call ids are
/// bad requests).
#[tokio::test]
async fn answer_permission_validates_state_and_request_id() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);

    // No request ever made -> conflict.
    append_kinds(&session.journal_path, &[user_msg("hi")]);
    let decisions = BTreeMap::from([("call_1".to_string(), true)]);
    let (status, _) = post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A request that was already answered is not pending -> conflict.
    append_kinds(
        &session.journal_path,
        &[permission_request(), permission_answered(true)],
    );
    let (status, _) = post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A pending request with a mismatched id is a bad request.
    append_kinds(&session.journal_path, &[permission_request()]);
    let (status, body) =
        post_permission_answer(&app.state, "s1", "p9", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("unknown request id"),
        "body: {body}"
    );
    // Nothing new was journaled (the pending request is still the last
    // event — no answer was appended).
    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    assert!(
        matches!(
            kinds.last(),
            Some(JournalEventKind::PermissionRequest { .. })
        ),
        "no answer may be journaled: {kinds:#?}"
    );

    // A pending request whose decisions miss an item is a bad request.
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);
    let partial = BTreeMap::from([("call_1".to_string(), true)]);
    let (status, body) = post_permission_answer(&app.state, "s1", "p1", Some(&partial), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("missing decision for call"),
        "body: {body}"
    );

    // Unknown call ids are rejected too.
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);
    let unknown = BTreeMap::from([
        ("call_1".to_string(), true),
        ("call_2".to_string(), false),
        ("call_9".to_string(), true),
    ]);
    let (status, body) = post_permission_answer(&app.state, "s1", "p1", Some(&unknown), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("unknown call id"), "body: {body}");

    // An empty decisions object is rejected (every path must be decided).
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);
    let empty = BTreeMap::new();
    let (status, body) = post_permission_answer(&app.state, "s1", "p1", Some(&empty), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("missing decisions"),
        "body: {body}"
    );
}

/// Legacy single-item requests (pre-batch journals) keep the old
/// `allowed` contract.
#[tokio::test]
async fn answer_permission_legacy_single_item_allowed_contract() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("read the file"),
            system_prompt(Mode::Build),
            assistant_msg("let me read it"),
            legacy_permission_request(),
        ],
    );
    let (status, body) = post_permission_answer(&app.state, "s1", "p1", None, Some(true)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "pending", "body: {body}");
    let kinds = read_kinds(&app, "s1");
    match &kinds[4] {
        JournalEventKind::PermissionAnswered {
            tool,
            operation,
            path,
            allowed,
            decisions,
            ..
        } => {
            assert_eq!(tool.as_deref(), Some("read_file"));
            assert_eq!(operation.as_deref(), Some("read"));
            assert_eq!(path.as_deref(), Some("/etc/hostname"));
            assert_eq!(*allowed, Some(true));
            assert!(decisions.is_empty(), "decisions: {decisions:#?}");
        }
        other => panic!("expected legacy permission_answered, got: {other:?}"),
    }

    // A legacy request without `allowed` is a bad request.
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[user_msg("read"), legacy_permission_request()],
    );
    let (status, body) = post_permission_answer(&app.state, "s1", "p1", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("missing allowed"), "body: {body}");
}

/// Answering while the session is running is refused.
#[tokio::test]
async fn answer_permission_while_running_conflicts() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    pending_permission_journal(&session);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
    }
    let decisions = BTreeMap::from([("call_1".to_string(), true)]);
    let (status, _) = post_permission_answer(&app.state, "s1", "p1", Some(&decisions), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

async fn post_model_switch(
    app: &Arc<AppState>,
    id: &str,
    model: &str,
) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/model"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"model":"{model}"}}"#)))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// POST a session-creation request with a JSON body.
async fn post_create_session(app: &Arc<AppState>, body: &str) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Switching a terminal session's model updates only the DB row — nothing
/// is journaled at switch time. The `ModelChange` notice lands right before
/// the next followup instead (like the mode change), so switches that never
/// affect a run leave no trace.
#[tokio::test]
async fn switch_model_updates_db_and_does_not_journal() {
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

    let (status, body) = post_model_switch(&app.state, "s1", "other-model").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model"], "other-model", "body: {body}");

    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert_eq!(row.model, "other-model");
    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 3, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModelChange { .. })),
        "no model change may be journaled at switch time: {kinds:#?}"
    );

    // Switching to the same model is a no-op (no extra DB write).
    let (status, _) = post_model_switch(&app.state, "s1", "other-model").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read_kinds(&app, "s1").len(), 3);
}

/// Unknown model -> 400, unknown session -> 404, running session -> 409;
/// a rejected switch never touches the DB model.
#[tokio::test]
async fn switch_model_rejects_unknown_and_running() {
    let app = test_app();
    let _session = insert_session(&app.state, "s1", Mode::Build);

    let (status, _) = post_model_switch(&app.state, "s1", "nope-model").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_model_switch(&app.state, "nope", "other-model").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
    }
    let (status, _) = post_model_switch(&app.state, "s1", "other-model").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert_eq!(
        row.model, "mock-model",
        "rejected switches must not change the model"
    );
}

/// The happy path of the followup journal: a single `ModelChange` notice
/// immediately before the user message, describing the switch from the
/// model of the last run to the session's current model.
#[tokio::test]
async fn send_message_injects_model_change_when_switched() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    // The journal records a completed run under `mock-model` (the model the
    // session was created with).
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
        db::update_model(&conn, "s1", "other-model").unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    assert_eq!(
        kinds[3],
        model_change("mock-model", "other-model"),
        "one model_change notice right before the followup: {kinds:#?}"
    );
    assert_eq!(kinds[4], user_msg("continue"));
}

/// Multiple switches before a single followup collapse into one
/// `ModelChange` describing the *final* model (intermediate switches never
/// affected a run).
#[tokio::test]
async fn multiple_model_switches_collapse_into_one_model_change() {
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
    // Simulate the switches: mock-model (last run) → other-model →
    // third-model.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_model(&conn, "s1", "other-model").unwrap();
        db::update_model(&conn, "s1", "third-model").unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    let model_changes: Vec<&JournalEventKind> = kinds
        .iter()
        .filter(|k| matches!(k, JournalEventKind::ModelChange { .. }))
        .collect();
    assert_eq!(model_changes.len(), 1, "kinds: {kinds:#?}");
    assert_eq!(
        kinds[3],
        model_change("mock-model", "third-model"),
        "the single notice must describe the final model: {kinds:#?}"
    );
    assert_eq!(kinds[4], user_msg("continue"));
}

/// No switch: the last-run model equals the session model, so no
/// ModelChange is journaled.
#[tokio::test]
async fn no_model_switch_injects_nothing() {
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
            .any(|k| matches!(k, JournalEventKind::ModelChange { .. })),
        "no model change expected: {kinds:#?}"
    );
    assert_eq!(kinds[3], user_msg("continue"));
}

/// Switching back to the model of the last run: the journal's model marker
/// is accurate again, so no notice is needed.
#[tokio::test]
async fn switch_back_to_last_run_model_injects_nothing() {
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
        db::update_model(&conn, "s1", "other-model").unwrap();
        db::update_model(&conn, "s1", "mock-model").unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModelChange { .. })),
        "no model change expected when switching back: {kinds:#?}"
    );
}

/// A session that never ran has no model marker: the upcoming first run
/// journals the SystemPrompt with the current model, so no notice.
#[tokio::test]
async fn never_ran_session_model_switch_injects_nothing() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Build);
    // Only the gateway's initial user message is journaled — the first
    // worker died before journaling the system prompt.
    append_kinds(&session.journal_path, &[user_msg("first message")]);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_model(&conn, "s1", "other-model").unwrap();
    }

    let status = send_followup(&app.state, "s1", "continue").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 2, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::ModelChange { .. })),
        "no model change expected for a never-ran session: {kinds:#?}"
    );
    assert_eq!(kinds[1], user_msg("continue"));
}

/// A previously injected ModelChange doubles as the model marker: after a
/// run under `other-model` (following a `mock-model` first run), switching
/// to `third-model` and following up injects one notice from
/// `other-model` — the `mock-model` SystemPrompt is not scanned past the
/// marker.
#[tokio::test]
async fn previous_model_change_acts_as_marker() {
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
    // Run 1: switched to other-model, followup -> one notice + user msg.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_model(&conn, "s1", "other-model").unwrap();
    }
    let status = send_followup(&app.state, "s1", "followup 1").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::clear_pid(&conn, "s1").unwrap();
        db::update_status(&conn, "s1", SessionStatus::Completed, None).unwrap();
    }
    // Simulate run 1 completing under other-model (the worker answered).
    append_kinds(&session.journal_path, &[assistant_msg("done 1")]);
    // Run 2: switch to third-model, followup.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_model(&conn, "s1", "third-model").unwrap();
    }
    let status = send_followup(&app.state, "s1", "followup 2").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    let model_changes: Vec<&JournalEventKind> = kinds
        .iter()
        .filter(|k| matches!(k, JournalEventKind::ModelChange { .. }))
        .collect();
    // One notice for run 1, one for run 2 — the mock-model marker was never
    // re-scanned.
    assert_eq!(model_changes.len(), 2, "kinds: {kinds:#?}");
    assert_eq!(
        kinds[3],
        model_change("mock-model", "other-model"),
        "kinds: {kinds:#?}"
    );
    // The last two events are the second notice + the followup.
    assert_eq!(
        kinds[kinds.len() - 2],
        model_change("other-model", "third-model"),
        "kinds: {kinds:#?}"
    );
    assert_eq!(kinds[kinds.len() - 1], user_msg("followup 2"));
}

/// Approving a pending mode-change request while the model was switched (in
/// the status bar) records the model change too: a `ModelChange` notice
/// right before the `ModeChange` continuation notice.
#[tokio::test]
async fn approve_mode_change_injects_model_change_when_switched() {
    let app = test_app();
    let session = insert_session(&app.state, "s1", Mode::Plan);
    pending_request_journal(&session);
    // The user switched the model while the request was pending.
    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_model(&conn, "s1", "other-model").unwrap();
    }

    let (status, body) = post_empty_json(&app.state, "s1", "/mode/approve").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["mode"], "build", "body: {body}");
    assert_eq!(body["model"], "other-model", "body: {body}");

    let kinds = read_kinds(&app, "s1");
    // seq 0 user, 1 system_prompt, 2 assistant, 3 mode_change_request,
    // 4 model_change, 5 mode_change (the continuation notice).
    assert_eq!(kinds.len(), 6, "kinds: {kinds:#?}");
    assert_eq!(
        kinds[4],
        model_change("mock-model", "other-model"),
        "kinds: {kinds:#?}"
    );
    assert!(
        matches!(
            &kinds[5],
            JournalEventKind::ModeChange {
                mode: Mode::Build,
                ..
            }
        ),
        "kinds: {kinds:#?}"
    );
}

/// GET /api/tools serves the tool registry for the "New session" checkbox
/// list: name, label, description and a `fixed` flag. The fixed tools are
/// exactly bash + the file operations; the optional tools are not.
#[tokio::test]
async fn list_tools_serves_the_registry() {
    let app = test_app();
    let router = create_router(app.state.clone());
    let request = Request::builder()
        .method("GET")
        .uri("/api/tools")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tools = value.as_array().expect("a JSON array");
    assert_eq!(tools.len(), mo_core::TOOL_NAMES.len(), "tools: {tools:#?}");
    let fixed: Vec<&str> = tools
        .iter()
        .filter(|t| t["fixed"] == true)
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for name in [
        "bash",
        "bash_in_background",
        "read_file",
        "edit_file",
        "create_file",
        "remove_file",
    ] {
        assert!(fixed.contains(&name), "{name} must be fixed");
    }
    for name in [
        "spawn_subagent",
        "load_skill",
        "request_mode_change",
        "ask_user",
    ] {
        assert!(!fixed.contains(&name), "{name} must be toggleable");
    }
    // Every entry carries the label + description the UI renders.
    for tool in tools {
        assert!(tool["label"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(tool["description"].as_str().is_some_and(|s| !s.is_empty()));
    }
}

/// Creating a session with `banned_tools` stores the canonical enabled
/// list on the session row (and echoes it in the response): the fixed
/// tools are always included, the banned toggleable ones are not.
#[tokio::test]
async fn create_session_with_banned_tools_stores_enabled_list() {
    let app = test_app();
    let workdir = app.state.cwd.display().to_string();
    let body = format!(
        r#"{{"workdir":"{workdir}","prompt":"do it","banned_tools":["ask_user","spawn_subagent"]}}"#
    );
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::CREATED, "body: {value}");
    let id = value["id"].as_str().unwrap().to_string();
    let tools: Vec<String> = value["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    // Fixed tools stay, the two banned toggleable tools are gone, the
    // other toggleable tools stay.
    assert!(tools.contains(&"bash".to_string()));
    assert!(tools.contains(&"read_file".to_string()));
    assert!(tools.contains(&"load_skill".to_string()));
    assert!(tools.contains(&"request_mode_change".to_string()));
    assert!(!tools.contains(&"ask_user".to_string()));
    assert!(!tools.contains(&"spawn_subagent".to_string()));

    // The DB row carries the same canonical list (the worker reads it
    // back to filter the schemas it injects).
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, &id).unwrap().unwrap()
    };
    assert_eq!(row.tools, tools);
}

/// Without `banned_tools` every tool is enabled (the default).
#[tokio::test]
async fn create_session_defaults_to_all_tools() {
    let app = test_app();
    let workdir = app.state.cwd.display().to_string();
    let body = format!(r#"{{"workdir":"{workdir}","prompt":"do it"}}"#);
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::CREATED, "body: {value}");
    let tools: Vec<String> = value["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    let expected: Vec<String> = mo_core::TOOL_NAMES.iter().map(|s| s.to_string()).collect();
    assert_eq!(tools, expected);
}

/// Banning a fixed tool (always available) or an unknown tool name is a
/// bad request — no session is created.
#[tokio::test]
async fn create_session_rejects_bad_bans() {
    let app = test_app();
    let workdir = app.state.cwd.display().to_string();
    for (banned, needle) in [
        (r#"["bash"]"#, "always available"),
        (r#"["read_file"]"#, "always available"),
        (r#"["nope"]"#, "unknown tool"),
    ] {
        let body = format!(r#"{{"workdir":"{workdir}","prompt":"do it","banned_tools":{banned}}}"#);
        let (status, value) = post_create_session(&app.state, &body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "banned: {banned}, body: {value}"
        );
        assert!(
            value.to_string().contains(needle),
            "banned: {banned}, body: {value}"
        );
    }
    // Nothing was created.
    let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
    assert!(db::list_sessions(&conn).unwrap().is_empty());
}

/// Write a skill into the test app's agents dir (both layouts work; the
/// helper defaults to the top-level layout).
fn write_skill(app: &TestApp, rel_dir: &str, name: &str, description: &str, body: &str) {
    let dir = app.state.agents_dir.join(rel_dir);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n");
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

/// POST a status-bar skill load with a JSON body.
async fn post_skill_load(
    app: &Arc<AppState>,
    id: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let router = create_router(app.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/skills/load"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// GET /api/skills serves every discovered global skill (both layouts,
/// sorted by name) for the "New session" skill list and the status-bar
/// skill picker: name + description.
#[tokio::test]
async fn list_skills_serves_discovered_skills() {
    let app = test_app();
    write_skill(
        &app,
        "beta-skill",
        "beta-skill",
        "Does beta.",
        "# Beta body",
    );
    write_skill(
        &app,
        "skills/alpha-skill",
        "alpha-skill",
        "Does alpha.",
        "# Alpha body",
    );
    let router = create_router(app.state.clone());
    let request = Request::builder()
        .method("GET")
        .uri("/api/skills")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let skills = value.as_array().expect("a JSON array");
    let names: Vec<&str> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    // Sorted by name; metadata only — the SKILL.md bodies are not served
    // here (the worker reads them from disk when building the prompt).
    assert_eq!(names, ["alpha-skill", "beta-skill"]);
    assert_eq!(skills[0]["description"], "Does alpha.");
    assert_eq!(skills[1]["description"], "Does beta.");
    assert!(!skills[0].to_string().contains("# Alpha body"));

    // An agents dir with no skills serves an empty list.
    let app = test_app();
    let router = create_router(app.state.clone());
    let request = Request::builder()
        .method("GET")
        .uri("/api/skills")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.as_array().expect("a JSON array").len(), 0);
}

/// Creating a session with `skills` stores the force-loaded skill names on
/// the session row (and echoes them in the response); the worker later
/// inlines their full SKILL.md into the system prompt.
#[tokio::test]
async fn create_session_with_skills_stores_them() {
    let app = test_app();
    write_skill(&app, "j-space", "j-space", "Thinks harder.", "# J body");
    write_skill(
        &app,
        "skills/bochi",
        "bochi",
        "Drives Android devices.",
        "# Bochi body",
    );
    let workdir = app.state.cwd.display().to_string();
    let body =
        format!(r#"{{"workdir":"{workdir}","prompt":"do it","skills":["j-space","bochi"]}}"#);
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::CREATED, "body: {value}");
    let id = value["id"].as_str().unwrap().to_string();
    let skills: Vec<String> = value["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert_eq!(skills, ["j-space", "bochi"]);

    // The DB row carries the same list (the worker reads it back when
    // building the system prompt).
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, &id).unwrap().unwrap()
    };
    assert_eq!(row.skills, skills);
}

/// Without `skills` no skill is force-loaded (the default); duplicates in
/// the request are deduplicated.
#[tokio::test]
async fn create_session_defaults_to_no_skills_and_dedupes() {
    let app = test_app();
    write_skill(&app, "alpha", "alpha", "Does alpha.", "# Alpha body");
    let workdir = app.state.cwd.display().to_string();

    let body = format!(r#"{{"workdir":"{workdir}","prompt":"do it"}}"#);
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::CREATED, "body: {value}");
    assert_eq!(
        value["skills"].as_array().expect("skills array").len(),
        0,
        "body: {value}"
    );

    let body = format!(r#"{{"workdir":"{workdir}","prompt":"do it","skills":["alpha","alpha"]}}"#);
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::CREATED, "body: {value}");
    let skills: Vec<String> = value["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert_eq!(skills, ["alpha"], "duplicates must be dropped: {skills:?}");
}

/// An unknown skill name in `skills` is a bad request — no session is
/// created.
#[tokio::test]
async fn create_session_rejects_unknown_skills() {
    let app = test_app();
    let workdir = app.state.cwd.display().to_string();
    let body = format!(r#"{{"workdir":"{workdir}","prompt":"do it","skills":["ghost-skill"]}}"#);
    let (status, value) = post_create_session(&app.state, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {value}");
    assert!(value.to_string().contains("unknown skill"), "body: {value}");
    let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
    assert!(db::list_sessions(&conn).unwrap().is_empty());
}

/// The status-bar skill load: the skill's full SKILL.md is journaled as a
/// new user message (wrapped in the load marker) and the worker respawns —
/// exactly like a followup, but nothing is persisted on the session row.
#[tokio::test]
async fn load_skill_journals_skill_message_and_respawns() {
    let app = test_app();
    write_skill(&app, "j-space", "j-space", "Thinks harder.", "# J body");
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );

    let (status, body) = post_skill_load(&app.state, "s1", r#"{"name":"j-space"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body["status"], "pending", "body: {body}");
    assert!(
        body["pid"].is_number(),
        "worker should be respawned: {body}"
    );

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 4, "kinds: {kinds:#?}");
    match &kinds[3] {
        JournalEventKind::Message(m) if m.role == "user" => {
            assert!(
                m.content.contains("[The user loaded the skill \"j-space\""),
                "content: {}",
                m.content
            );
            assert!(m.content.contains("Treat them as active and follow them."));
            assert!(m.content.contains("# J body"), "content: {}", m.content);
        }
        other => panic!("expected the skill-load user message at seq 3, got: {other:?}"),
    }
    // The load is a one-off message: the session row's skills are untouched.
    let row = {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_session(&conn, "s1").unwrap().unwrap()
    };
    assert!(row.skills.is_empty(), "row skills: {:?}", row.skills);
}

/// Unknown skill -> 400, empty name -> 400, unknown session -> 404, and a
/// running session -> 409; nothing is journaled on a rejected load.
#[tokio::test]
async fn load_skill_rejects_unknown_empty_running() {
    let app = test_app();
    write_skill(&app, "alpha", "alpha", "Does alpha.", "# Alpha body");
    let session = insert_session(&app.state, "s1", Mode::Build);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );

    let (status, value) = post_skill_load(&app.state, "s1", r#"{"name":"ghost"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {value}");
    assert!(
        value.to_string().contains("skill not found"),
        "body: {value}"
    );

    let (status, _) = post_skill_load(&app.state, "s1", r#"{"name":"  "}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_skill_load(&app.state, "nope", r#"{"name":"alpha"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    {
        let conn = app.state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
    }
    let (status, _) = post_skill_load(&app.state, "s1", r#"{"name":"alpha"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Nothing was journaled by any of the rejected loads.
    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 3, "kinds: {kinds:#?}");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, JournalEventKind::Message(m) if m.role == "user" && m.content.contains("Alpha body"))),
        "kinds: {kinds:#?}"
    );
}

/// The status-bar skill load goes through the same followup path as a
/// message: a mode switched since the last run still injects the single
/// mode-change notice right before the loaded skill.
#[tokio::test]
async fn load_skill_injects_mode_change_when_switched() {
    let app = test_app();
    write_skill(&app, "alpha", "alpha", "Does alpha.", "# Alpha body");
    let session = insert_session(&app.state, "s1", Mode::Plan);
    append_kinds(
        &session.journal_path,
        &[
            user_msg("hi"),
            system_prompt(Mode::Build),
            assistant_msg("done"),
        ],
    );

    let (status, _) = post_skill_load(&app.state, "s1", r#"{"name":"alpha"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let kinds = read_kinds(&app, "s1");
    assert_eq!(kinds.len(), 5, "kinds: {kinds:#?}");
    match &kinds[3] {
        JournalEventKind::ModeChange { mode, .. } => assert_eq!(*mode, Mode::Plan),
        other => panic!("expected the mode-change notice at seq 3, got: {other:?}"),
    }
    match &kinds[4] {
        JournalEventKind::Message(m) if m.role == "user" => {
            assert!(m.content.contains("# Alpha body"), "content: {}", m.content);
        }
        other => panic!("expected the skill-load user message at seq 4, got: {other:?}"),
    }
}
