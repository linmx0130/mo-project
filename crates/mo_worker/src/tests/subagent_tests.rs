//! Unit tests for the `tools::subagent` module — production code lives in
//! `mo_worker/src/tools/subagent.rs`. Wired from there with `#[cfg(test)] #[path = "../tests/subagent_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use std::sync::{Arc, Mutex};

/// A ctx whose data dir is a tempdir (so `create_child_session` can
/// write the DB + journal) and whose session is a *root* session
/// (`parent_id` None) unless overridden.
fn test_ctx(dir: &tempfile::TempDir, parent_id: Option<String>) -> ToolContext {
    let session = Session {
        id: "parent".into(),
        parent_id,
        workdir: "/tmp".into(),
        prompt: "parent title".into(),
        model: "m".into(),
        status: SessionStatus::Running,
        mode: Mode::Build,
        pid: None,
        journal_path: "/tmp/j.jsonl".into(),
        created_at: "now".into(),
        updated_at: "now".into(),
        heartbeat_at: None,
        error: None,
    };
    ToolContext {
        workdir: std::path::PathBuf::from("/tmp"),
        data_dir: dir.path().join("data"),
        agents_dir: dir.path().join("agents"),
        session,
        scratch: dir.path().join("data/sessions/parent/tmp"),
        subagent_depth: 0,
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        model_base_url: "http://localhost:1".into(),
        model_name: "m".into(),
        auth_token: None,
        context_window: Some(4096),
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
    }
}

/// A session that is itself a subagent (`parent_id` set) is refused:
/// the depth hard limit is 1, so subagents are leaves.
#[test]
fn subagent_cannot_spawn_further() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(&dir, Some("root".to_string()));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let no_event = |_: JournalEventKind| {};
    let err = rt
        .block_on(spawn_subagent(
            &ctx,
            "do it",
            Mode::Explore,
            "call_x",
            &no_event,
        ))
        .unwrap_err();
    assert!(
        err.contains("subagents cannot spawn further subagents"),
        "got: {err}"
    );
    assert!(err.contains("hard limit 1"), "got: {err}");
}

/// Creating a child session: the DB row carries the `Subagent for …`
/// title (never the task text), the child journal is seeded with the
/// task as its first user message, and the parent journal receives a
/// `SubagentStarted` event linking the tool block to the child.
#[test]
fn create_child_session_titles_and_seeds_journal() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(&dir, None);
    let events = Arc::new(Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };

    let id = create_child_session(
        &ctx,
        "subagent task text",
        Mode::Explore,
        "call_x",
        &on_event,
    )
    .unwrap();

    // The row: parent set, title "Subagent for <parent title>", mode
    // as chosen by the parent.
    let conn = open_db(&ctx.data_dir.join("mo.db")).unwrap();
    let row = db::get_session(&conn, &id).unwrap().unwrap();
    assert_eq!(row.parent_id.as_deref(), Some("parent"));
    assert_eq!(row.prompt, "Subagent for parent title");
    assert_eq!(row.mode, Mode::Explore);
    assert_eq!(row.status, SessionStatus::Pending);
    drop(conn);

    // The child journal's first (and only) event is the task as a user
    // message — the worker rebuilds its context from the journal.
    let journal_path = ctx
        .data_dir
        .join("sessions")
        .join(&id)
        .join("journal.jsonl");
    let child_events = mo_core::read_events(&journal_path).unwrap();
    assert_eq!(child_events.len(), 1, "events: {child_events:#?}");
    match &child_events[0].kind {
        JournalEventKind::Message(m) => {
            assert_eq!(m.role, "user");
            assert_eq!(m.content, "subagent task text");
        }
        other => panic!("expected user message, got: {other:?}"),
    }

    // The SubagentStarted event links the tool block to the child.
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1, "events: {events:#?}");
    match &events[0] {
        JournalEventKind::SubagentStarted {
            child_id,
            tool_call_id,
            mode,
        } => {
            assert_eq!(child_id, &id);
            assert_eq!(tool_call_id, "call_x");
            assert_eq!(*mode, Mode::Explore);
        }
        other => panic!("expected subagent_started, got: {other:?}"),
    }
}

/// A parent without a title yields the bare "Subagent" fallback.
#[test]
fn create_child_session_falls_back_to_bare_subagent_title() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = test_ctx(&dir, None);
    ctx.session.prompt = "   ".to_string();
    let no_event = |_: JournalEventKind| {};
    let id = create_child_session(&ctx, "task", Mode::Build, "c1", &no_event).unwrap();
    let conn = open_db(&ctx.data_dir.join("mo.db")).unwrap();
    let row = db::get_session(&conn, &id).unwrap().unwrap();
    assert_eq!(row.prompt, "Subagent");
}
