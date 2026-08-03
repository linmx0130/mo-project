//! Integration tests over the gateway router with a temp data dir and stub
//! worker binaries.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mo_core::{JournalEventKind, JournalWriter, ModelConfig, SessionStatus, db, open_db};
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
        },
        ModelConfig {
            base_url: "http://127.0.0.1:9002".into(),
            name: "second-model".into(),
            token: None,
            nickname: None,
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
        subagent_depth: 0,
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

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
        || (result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}
