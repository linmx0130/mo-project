//! Integration tests over the gateway router with a temp data dir and stub
//! worker binaries.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mo_core::{
    JournalEventKind, JournalWriter, Mode, ModelConfig, Session, SessionStatus, db, open_db,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use mo_gateway::routes::create_router;
use mo_gateway::state::AppState;

fn test_models() -> Vec<ModelConfig> {
    vec![
        ModelConfig {
            base_url: "http://127.0.0.1:9001".into(),
            name: "default-model".into(),
            token: Some("tok-1".into()),
            nickname: Some("alpha".into()),
            context_window: Some(32768),
        },
        ModelConfig {
            base_url: "http://127.0.0.1:9002".into(),
            name: "second-model".into(),
            token: None,
            nickname: None,
            context_window: None,
        },
    ]
}

fn write_stub_worker(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("stub_worker.sh");
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn setup(sleep: bool) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let data_dir = dir.path().join("data");
    let worker_bin = if sleep {
        write_stub_worker(dir.path(), "#!/bin/sh\nsleep 30\n")
    } else {
        write_stub_worker(dir.path(), "#!/bin/sh\nexit 0\n")
    };
    let conn = mo_core::open_db(&data_dir.join("mo.db")).unwrap();
    let state = Arc::new(AppState {
        data_dir,
        db: Mutex::new(conn),
        worker_bin,
        cwd: std::env::current_dir().unwrap(),
        agents_dir: dir.path().join("agents"),
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
        models: test_models(),
    });
    (dir, create_router(state))
}

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            builder.body(Body::from(value.to_string())).unwrap()
        }
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn meta_reports_gateway_cwd() {
    let (_dir, app) = setup(false);
    let (status, meta) = request(&app, Method::GET, "/api/meta", None).await;
    assert_eq!(status, StatusCode::OK);
    let cwd = meta["cwd"].as_str().expect("meta.cwd should be a string");
    assert_eq!(cwd, std::env::current_dir().unwrap().to_string_lossy());
}

#[tokio::test]
async fn create_list_get_cancel_flow() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");

    // POST /api/sessions
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "hello agent" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["status"], "pending");
    let id = session["id"].as_str().unwrap().to_string();
    let pid = session["pid"].as_u64().unwrap() as u32;
    assert!(process_is_alive(pid), "stub worker should be running");

    // GET /api/sessions
    let (status, sessions) = request(&app, Method::GET, "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["id"], id);

    // GET /api/sessions/:id
    let (status, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["id"], id);

    // POST /api/sessions/:id/cancel
    let (status, cancelled) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");

    // The stub must actually be dead shortly after cancel.
    for _ in 0..20 {
        if !process_is_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !process_is_alive(pid),
        "stub worker should have been killed"
    );
}

#[tokio::test]
async fn worker_died_is_marked_failed() {
    let (_dir, app) = setup(false); // stub exits immediately
    let workdir = _dir.path().join("work");
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "quick exit" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = session["id"].as_str().unwrap().to_string();

    // Simulate what a real worker does: mark itself running.
    let data_dir = _dir.path().join("data");
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    db::update_status(&conn, &id, SessionStatus::Running, None).unwrap();
    drop(conn);

    // The stub exits immediately, so the liveness check should flip it to
    // failed once the process is reaped.
    let mut seen_failed = false;
    for _ in 0..50 {
        let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
        if detail["status"] == "failed" {
            assert!(detail["error"].as_str().unwrap().contains("worker died"));
            seen_failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seen_failed, "session was never marked failed");
}

/// Insert a session row directly (no worker spawn) whose pid is the test
/// process itself — alive — but whose heartbeat is long stale. This models
/// a worker whose process survived while its async runtime froze.
fn insert_stalled_session(data_dir: &Path, id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let session = Session {
        id: id.to_string(),
        parent_id: None,
        workdir: "/tmp".to_string(),
        prompt: "stalled".to_string(),
        model: "default-model".to_string(),
        status: SessionStatus::Running,
        mode: Mode::Build,
        pid: Some(std::process::id()),
        journal_path: data_dir
            .join("sessions")
            .join(id)
            .join("journal.jsonl")
            .display()
            .to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        heartbeat_at: Some((chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339()),
        error: None,
    };
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    db::create_session(&conn, &session).unwrap();
    drop(conn);
}

#[tokio::test]
async fn stalled_worker_is_marked_failed_via_get() {
    let (_dir, app) = setup(false);
    let id = "stalled-get";
    insert_stalled_session(&_dir.path().join("data"), id);

    // The pid (this test process) is alive, so only the stale heartbeat
    // flags the worker as stalled — the session must not stay `running`.
    let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(detail["status"], "failed");
    let error = detail["error"].as_str().unwrap();
    assert!(error.contains("worker stalled"), "got: {error}");
    assert!(error.contains("no heartbeat"), "got: {error}");
}

#[tokio::test]
async fn stalled_worker_is_marked_failed_via_sse() {
    let (_dir, app) = setup(false);
    let id = "stalled-sse";
    insert_stalled_session(&_dir.path().join("data"), id);

    // The SSE tail must synthesize the failed status change on its own (the
    // frontend relies on this to recover a stuck session without a reload).
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"status\":\"failed\""), "got: {text}");
    assert!(text.contains("worker stalled"), "got: {text}");
}

#[tokio::test]
async fn fresh_heartbeat_is_not_flagged_stalled() {
    let (_dir, app) = setup(false);
    let id = "healthy-heartbeat";
    let now = chrono::Utc::now().to_rfc3339();
    let session = Session {
        id: id.to_string(),
        parent_id: None,
        workdir: "/tmp".to_string(),
        prompt: "healthy".to_string(),
        model: "default-model".to_string(),
        status: SessionStatus::Running,
        mode: Mode::Build,
        pid: Some(std::process::id()),
        journal_path: _dir
            .path()
            .join("data")
            .join("sessions")
            .join(id)
            .join("journal.jsonl")
            .display()
            .to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        heartbeat_at: Some(now),
        error: None,
    };
    {
        let conn = open_db(&_dir.path().join("data").join("mo.db")).unwrap();
        db::create_session(&conn, &session).unwrap();
        drop(conn);
    }

    let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(
        detail["status"], "running",
        "a worker with a fresh heartbeat must not be flagged stalled: {detail}"
    );
}

#[tokio::test]
async fn history_returns_events_and_respects_after_seq() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");
    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "hi" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
    journal
        .append(JournalEventKind::Message(mo_core::JournalMessage {
            role: "user".into(),
            content: "hello".into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    journal
        .append(JournalEventKind::StatusChange {
            status: SessionStatus::Completed,
            error: None,
        })
        .unwrap();
    drop(journal);

    // The gateway journals the initial user message at creation (seq 0),
    // then the test appends two more events.
    let (status, events) = request(
        &app,
        Method::GET,
        &format!("/api/sessions/{id}/history"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.as_array().unwrap().len(), 3);
    assert_eq!(events[0]["kind"]["kind"], "message");

    let (status, after) = request(
        &app,
        Method::GET,
        &format!("/api/sessions/{id}/history?after_seq=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after.as_array().unwrap().len(), 1);
    assert_eq!(after[0]["seq"], 2);
    assert_eq!(after[0]["kind"]["kind"], "status_change");
}

#[tokio::test]
async fn new_session_gets_placeholder_title_and_journals_first_message() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(
            json!({ "workdir": workdir.display().to_string(), "prompt": "original first message" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = session["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with("New session - "),
        "session title should start with the placeholder, got: {prompt}"
    );

    // The journal still carries the real first message: the worker rebuilds
    // its context from it, so titling must not touch the conversation.
    let journal_path = session["journal_path"].as_str().unwrap().to_string();
    let events = mo_core::read_events(Path::new(&journal_path)).unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].kind, JournalEventKind::Message(m) if m.role == "user" && m.content == "original first message")
    );
}

#[tokio::test]
async fn rejects_invalid_workdir_and_unknown_session() {
    let (_dir, app) = setup(false);
    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": "/definitely/not/here", "prompt": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": "/tmp", "prompt": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = request(&app, Method::GET, "/api/sessions/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = request(&app, Method::POST, "/api/sessions/nope/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sse_streams_journal_events_and_closes_on_terminal() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");
    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "sse test" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    // Terminal status in the DB (journaled events too, to keep it simple).
    {
        let conn = open_db(&_dir.path().join("data").join("mo.db")).unwrap();
        db::update_status(&conn, &id, SessionStatus::Completed, None).unwrap();
    }
    let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
    journal
        .append(JournalEventKind::MessageDelta {
            content: "streamed ".into(),
            reasoning_content: None,
        })
        .unwrap();
    journal
        .append(JournalEventKind::MessageDelta {
            content: "tokens".into(),
            reasoning_content: Some("think first ".into()),
        })
        .unwrap();
    journal
        .append(JournalEventKind::ToolOutputDelta {
            id: "call_1".into(),
            name: "bash".into(),
            output: "build output\n".into(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(mo_core::JournalMessage {
            role: "assistant".into(),
            content: "streamed tokens".into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    journal
        .append(JournalEventKind::StatusChange {
            status: SessionStatus::Completed,
            error: None,
        })
        .unwrap();
    drop(journal);

    // Open the SSE stream and collect events until it closes.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"role\":\"assistant\""), "got: {text}");
    assert!(text.contains("\"status\":\"completed\""), "got: {text}");
    // Streaming events pass through the SSE tail verbatim.
    assert!(text.contains("\"kind\":\"message_delta\""), "got: {text}");
    assert!(text.contains("streamed tokens"), "got: {text}");
    assert!(
        text.contains("\"kind\":\"tool_output_delta\""),
        "got: {text}"
    );
    assert!(text.contains("build output"), "got: {text}");

    // Connecting with a cursor past every journaled event must NOT
    // re-synthesize the terminal status (the first poll seeds journal_status
    // from the whole journal). The journal now has 6 events: the initial
    // user message (seq 0, gateway), two message deltas (seq 1-2), a tool
    // output delta (seq 3), assistant message (seq 4), status change (seq 5).
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}/events?after_seq=5"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("status_change"),
        "terminal status should not be re-synthesized, got: {text}"
    );
}

/// A running session's journal keeps growing while the SSE connection is
/// open: an event appended *after* the stream connected must still reach the
/// client (the incremental tail reader picks up appended bytes on the next
/// poll) and the stream must still close once the session turns terminal.
#[tokio::test]
async fn sse_streams_events_appended_after_connect() {
    let (_dir, app) = setup(true); // stub worker sleeps: session stays pending
    let workdir = _dir.path().join("work");
    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "live" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    // Open the SSE stream, then keep it open while the journal is appended.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_task = tokio::spawn(async move {
        axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap()
    });

    // Append an event while the stream is already polling.
    let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
    journal
        .append(JournalEventKind::MessageDelta {
            content: "live chunk".into(),
            reasoning_content: None,
        })
        .unwrap();
    drop(journal);

    // Turn the session terminal so the stream drains and closes.
    let conn = open_db(&_dir.path().join("data").join("mo.db")).unwrap();
    db::update_status(&conn, &id, SessionStatus::Completed, None).unwrap();
    drop(conn);

    let body = match tokio::time::timeout(Duration::from_secs(10), body_task).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => panic!("SSE body task failed: {e}"),
        Err(_) => panic!("SSE stream did not close within 10s"),
    };
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"kind\":\"message_delta\""), "got: {text}");
    assert!(text.contains("live chunk"), "got: {text}");
}

#[tokio::test]
async fn followup_message_respawns_worker_and_journals() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "first message" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    // Reject a message while the session is still queued/running.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/messages"),
        Some(json!({ "content": "too soon" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Stop it, then send a followup: the worker respawns and the session is
    // queued as pending again.
    let (_, stopped) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(stopped["status"], "cancelled");

    let (status, resumed) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/messages"),
        Some(json!({ "content": "follow up" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(resumed["status"], "pending");
    let pid = resumed["pid"].as_u64().unwrap() as u32;
    assert!(process_is_alive(pid), "followup worker should be running");

    // The journal holds the initial prompt plus the followup message.
    let events = mo_core::read_events(Path::new(&journal_path)).unwrap();
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[1].kind, JournalEventKind::Message(m) if m.role == "user" && m.content == "follow up")
    );

    // Blank content is rejected.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/messages"),
        Some(json!({ "content": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown session is 404.
    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions/nope/messages",
        Some(json!({ "content": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_session_removes_row_and_disk_files() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");
    let data_dir = _dir.path().join("data");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "to be deleted" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    // The stub exits immediately; wait for the liveness check to mark the
    // session failed so the delete path skips the worker kill.
    let mut terminal = false;
    for _ in 0..50 {
        let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
        if detail["status"] == "failed" {
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(terminal, "session was never marked failed");

    // The session directory (journal + worker log) exists on disk.
    let session_dir = data_dir.join("sessions").join(&id);
    assert!(session_dir.join("journal.jsonl").is_file());
    assert!(session_dir.join("worker.log").exists());

    // DELETE /api/sessions/:id
    let (status, _) = request(&app, Method::DELETE, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // DB row is gone and the session directory is removed from disk.
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    assert!(db::get_session(&conn, &id).unwrap().is_none());
    drop(conn);
    assert!(!session_dir.exists(), "session dir should be removed");

    // The session is gone from the list and the detail endpoint 404s.
    let (status, sessions) = request(&app, Method::GET, "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sessions.as_array().unwrap().len(), 0);
    let (status, _) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_running_session_kills_worker() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");
    let data_dir = _dir.path().join("data");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "run & delete" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let pid = session["pid"].as_u64().unwrap() as u32;
    assert!(process_is_alive(pid), "stub worker should be running");

    let session_dir = data_dir.join("sessions").join(&id);
    assert!(session_dir.is_dir());

    let (status, _) = request(&app, Method::DELETE, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The worker must be dead and the session gone from DB + disk.
    for _ in 0..20 {
        if !process_is_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!process_is_alive(pid), "worker should have been killed");
    assert!(!session_dir.exists(), "session dir should be removed");
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    assert!(db::get_session(&conn, &id).unwrap().is_none());
}

#[tokio::test]
async fn delete_unknown_session_returns_404() {
    let (_dir, app) = setup(false);
    let (status, _) = request(&app, Method::DELETE, "/api/sessions/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Insert a subagent session row (parent set) directly, for the cancel /
/// delete cascade tests.
fn insert_child_session(
    data_dir: &Path,
    id: &str,
    parent_id: &str,
    status: SessionStatus,
) -> Session {
    let now = chrono::Utc::now().to_rfc3339();
    let session = Session {
        id: id.to_string(),
        parent_id: Some(parent_id.to_string()),
        workdir: "/tmp".to_string(),
        prompt: format!("Subagent for {parent_id}"),
        model: "default-model".to_string(),
        status,
        mode: Mode::Build,
        pid: None,
        journal_path: data_dir
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
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    db::create_session(&conn, &session).unwrap();
    drop(conn);
    session
}

/// Cancelling a session must also mark every subagent session it spawned
/// `cancelled` (their workers die with the parent's process group, so the
/// rows must not stay `running` with a dead pid).
#[tokio::test]
async fn cancel_parent_marks_subagents_cancelled() {
    let (_dir, app) = setup(true); // stub worker sleeps
    let workdir = _dir.path().join("work");
    let data_dir = _dir.path().join("data");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "run & cancel" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let pid = session["pid"].as_u64().unwrap() as u32;
    assert!(process_is_alive(pid), "stub worker should be running");
    insert_child_session(&data_dir, "child-cancel", &id, SessionStatus::Running);

    let (status, cancelled) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");

    // The child row is marked cancelled too.
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    let row = db::get_session(&conn, "child-cancel").unwrap().unwrap();
    assert_eq!(row.status, SessionStatus::Cancelled);
}

/// Deleting a session must also remove every subagent session it spawned:
/// their rows and per-session directories are deleted with the parent (they
/// are hidden from the session list, so the user could not otherwise reach
/// them).
#[tokio::test]
async fn delete_parent_removes_subagent_rows_and_dirs() {
    let (_dir, app) = setup(false); // stub exits immediately
    let workdir = _dir.path().join("work");
    let data_dir = _dir.path().join("data");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "parent" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    // The stub exits immediately; wait for the liveness check to mark the
    // parent terminal so the delete path skips the worker kill.
    let mut terminal = false;
    for _ in 0..50 {
        let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
        if detail["status"] == "failed" {
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(terminal, "parent was never marked terminal");

    // A child row plus its session dir on disk.
    insert_child_session(&data_dir, "child-del", &id, SessionStatus::Running);
    let child_dir = data_dir.join("sessions").join("child-del");
    std::fs::create_dir_all(&child_dir).unwrap();
    std::fs::write(child_dir.join("journal.jsonl"), "{}\n").unwrap();

    let (status, _) = request(&app, Method::DELETE, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The child's row and directory are gone with the parent.
    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    assert!(db::get_session(&conn, "child-del").unwrap().is_none());
    drop(conn);
    assert!(!child_dir.exists(), "child dir should be removed");
}

#[tokio::test]
async fn models_endpoint_lists_models_first_is_default() {
    let (_dir, app) = setup(false);
    let (status, models) = request(&app, Method::GET, "/api/models", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = models.as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["name"], "default-model");
    assert_eq!(models[0]["default"], true);
    assert_eq!(models[0]["nickname"], "alpha");
    assert_eq!(models[1]["name"], "second-model");
    assert_eq!(models[1]["default"], false);
    assert!(models[1]["nickname"].is_null());
}

#[tokio::test]
async fn create_session_uses_default_model_and_accepts_model_choice() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");

    // No model -> the default (first) model is stored on the session.
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "hi" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["model"], "default-model");

    // Explicit model name -> stored on the session.
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "hi again",
            "model": "second-model",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["model"], "second-model");

    // Unknown model name -> 400.
    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "hi",
            "model": "nope-model",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn spawn_worker_passes_context_window_env() {
    // A stub worker that dumps its MO_* env to a file; the default model in
    // `test_models` has `context_window = 32768`, which must reach the
    // worker so it can embed the window in `context_usage` events.
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let data_dir = dir.path().join("data");
    let env_file = dir.path().join("env.txt");
    let worker_bin = write_stub_worker(
        dir.path(),
        &format!("#!/bin/sh\nenv | grep MO_ > {}\n", env_file.display()),
    );
    let conn = mo_core::open_db(&data_dir.join("mo.db")).unwrap();
    let state = Arc::new(AppState {
        data_dir,
        db: Mutex::new(conn),
        worker_bin,
        cwd: std::env::current_dir().unwrap(),
        agents_dir: dir.path().join("agents"),
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
        models: test_models(),
    });
    let app = create_router(state);

    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "env check" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut written = false;
    for _ in 0..50 {
        if env_file.exists() {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(written, "stub worker never wrote its env");
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(
        env.contains("MO_CONTEXT_WINDOW=32768"),
        "MO_CONTEXT_WINDOW missing from worker env: {env}"
    );
    assert!(
        env.contains("MO_MODEL_NAME=default-model"),
        "model env missing: {env}"
    );
    // Root sessions are depth 0: the worker must not be told it sits at a
    // subagent depth (which would frame it as "you are a subagent").
    assert!(
        env.contains("MO_SUBAGENT_DEPTH=0"),
        "MO_SUBAGENT_DEPTH must be 0 for root workers: {env}"
    );
}

#[tokio::test]
async fn modes_endpoint_lists_the_three_modes() {
    let (_dir, app) = setup(false);
    let (status, modes) = request(&app, Method::GET, "/api/modes", None).await;
    assert_eq!(status, StatusCode::OK);
    let modes = modes.as_array().unwrap();
    assert_eq!(modes.len(), 3);
    let names: Vec<&str> = modes.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["build", "plan", "explore"]);
    // The UI picker shows a description and the tool set (which now
    // includes request_mode_change and ask_user in every mode).
    for m in modes {
        assert!(!m["label"].as_str().unwrap().is_empty());
        assert!(!m["description"].as_str().unwrap().is_empty());
        assert!(m["tools"].as_array().unwrap().len() == 10);
        assert!(
            m["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "request_mode_change")
        );
        assert!(
            m["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "ask_user")
        );
        assert!(
            m["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "bash_in_background")
        );
    }
}

#[tokio::test]
async fn create_session_accepts_and_defaults_mode() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");

    // No mode -> build (the default and the legacy behavior).
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "hi" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["mode"], "build");

    // Explicit mode -> stored on the session.
    let (status, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "plan it",
            "mode": "plan",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["mode"], "plan");

    // Unknown mode -> 400.
    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "hi",
            "mode": "nope",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn switch_mode_updates_terminal_session_and_rejects_running() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "first" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    // While running (the stub sleeps), switching is rejected.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode"),
        Some(json!({ "mode": "plan" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Stop it, then switch: the mode updates and the session is returned.
    let (_, stopped) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(stopped["status"], "cancelled");
    let (status, switched) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode"),
        Some(json!({ "mode": "explore" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(switched["mode"], "explore");

    // The new mode persists on the row (list + detail see it).
    let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
    assert_eq!(detail["mode"], "explore");

    // Unknown mode -> 400; unknown session -> 404.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode"),
        Some(json!({ "mode": "nope" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        Method::POST,
        "/api/sessions/nope/mode",
        Some(json!({ "mode": "plan" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn approve_mode_change_switches_mode_and_continues() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");

    // A terminal session whose journal records a completed Plan-mode run
    // ending with a pending request to switch to Build.
    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "plan it",
            "mode": "plan",
        })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    // Stop the stub worker, then shape the journal: a completed run + the
    // worker's mode-change request (what the request_mode_change tool
    // journals).
    let (_, stopped) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(stopped["status"], "cancelled");
    {
        let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
        journal
            .append(JournalEventKind::SystemPrompt {
                content: "plan framing".to_string(),
                mode: Mode::Plan,
            })
            .unwrap();
        journal
            .append(JournalEventKind::Message(mo_core::JournalMessage {
                role: "assistant".to_string(),
                content: "here is the plan".to_string(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
            .unwrap();
        journal
            .append(JournalEventKind::ModeChangeRequest {
                mode: Mode::Build,
                message: "may I switch to build mode to implement the plan?".to_string(),
            })
            .unwrap();
    }

    // Approving returns the session queued as pending in the new mode...
    let (status, resumed) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode/approve"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(resumed["mode"], "build");
    assert_eq!(resumed["status"], "pending");

    // ...journals exactly one ModeChange notice (the single mode-change
    // message that continues the run)...
    let events = mo_core::read_events(Path::new(&journal_path)).unwrap();
    // seq 0: initial user message (gateway), 1: system prompt, 2: assistant
    // message, 3: mode_change_request, 4: the mode_change notice.
    assert_eq!(events.len(), 5, "events: {events:#?}");
    match &events[4].kind {
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
        }
        other => panic!("expected mode_change, got: {other:?}"),
    }

    // ...and the DB row carries the new mode.
    let conn = open_db(&_dir.path().join("data").join("mo.db")).unwrap();
    let row = db::get_session(&conn, &id).unwrap().unwrap();
    assert_eq!(row.mode, Mode::Build);
    drop(conn);

    // A second approve (the request is no longer pending) is a conflict.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode/approve"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn reject_mode_change_keeps_mode_and_resolves_request() {
    let (_dir, app) = setup(false);
    let workdir = _dir.path().join("work");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "workdir": workdir.display().to_string(),
            "prompt": "plan it",
            "mode": "plan",
        })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();
    // The stub exits immediately; wait for liveness to flip it terminal.
    let mut terminal = false;
    for _ in 0..50 {
        let (_, detail) = request(&app, Method::GET, &format!("/api/sessions/{id}"), None).await;
        if detail["status"] != "pending" && detail["status"] != "running" {
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(terminal, "session never became terminal");
    {
        let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
        journal
            .append(JournalEventKind::ModeChangeRequest {
                mode: Mode::Build,
                message: "may I switch?".to_string(),
            })
            .unwrap();
    }

    let (status, rejected) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode/reject"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rejected["mode"], "plan");
    // The request is resolved by a declined marker; the mode did not change.
    let events = mo_core::read_events(Path::new(&journal_path)).unwrap();
    // seq 0: initial user message (gateway), 1: mode_change_request,
    // 2: mode_change_request_declined.
    assert_eq!(events.len(), 3, "events: {events:#?}");
    assert!(
        matches!(
            &events[2].kind,
            JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build }
        ),
        "events: {events:#?}"
    );
    let conn = open_db(&_dir.path().join("data").join("mo.db")).unwrap();
    let row = db::get_session(&conn, &id).unwrap().unwrap();
    assert_eq!(row.mode, Mode::Plan);
    drop(conn);

    // Rejecting again (nothing pending) is a conflict.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/mode/reject"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// The user answers a pending clarification question (the agent called
/// `ask_user`): the gateway journals an `AskUserAnswered` event carrying
/// the answers as a JSON object keyed by question_id and resumes the run
/// (session back to pending with a fresh worker).
#[tokio::test]
async fn answer_ask_user_continues_run() {
    let (_dir, app) = setup(true);
    let workdir = _dir.path().join("work");

    let (_, session) = request(
        &app,
        Method::POST,
        "/api/sessions",
        Some(json!({ "workdir": workdir.display().to_string(), "prompt": "which language?" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let journal_path = session["journal_path"].as_str().unwrap().to_string();

    // Stop the stub worker, then shape the journal: a completed run + the
    // worker's pending clarification question (what the ask_user tool
    // journals).
    let (_, stopped) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(stopped["status"], "cancelled");
    {
        let mut journal = JournalWriter::open(Path::new(&journal_path)).unwrap();
        journal
            .append(JournalEventKind::SystemPrompt {
                content: "build framing".to_string(),
                mode: Mode::Build,
            })
            .unwrap();
        journal
            .append(JournalEventKind::Message(mo_core::JournalMessage {
                role: "assistant".to_string(),
                content: "let me ask the user".to_string(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
            .unwrap();
        journal
            .append(JournalEventKind::AskUserRequest {
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
            })
            .unwrap();
    }

    // Answering (a free-text answer here) returns the session queued as
    // pending with a fresh worker...
    let (status, resumed) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/ask/answer"),
        Some(json!({ "answers": { "q1": "Rust" } })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(resumed["status"], "pending");
    assert!(resumed["pid"].is_number(), "worker should be respawned");

    // ...journals the answers keyed by question_id...
    let events = mo_core::read_events(Path::new(&journal_path)).unwrap();
    // seq 0: initial user message (gateway), 1: system prompt, 2: assistant
    // message, 3: ask_user_request, 4: ask_user_answered.
    assert_eq!(events.len(), 5, "events: {events:#?}");
    match &events[4].kind {
        JournalEventKind::AskUserAnswered { answers } => {
            assert_eq!(answers.len(), 1, "answers: {answers:?}");
            assert_eq!(answers.get("q1").map(String::as_str), Some("Rust"));
        }
        other => panic!("expected ask_user_answered, got: {other:?}"),
    }

    // ...and the pending request is resolved: a second answer is a conflict.
    let (status, _) = request(
        &app,
        Method::POST,
        &format!("/api/sessions/{id}/ask/answer"),
        Some(json!({ "answers": { "q1": "Python" } })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
        || (result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}
