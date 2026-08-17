//! Unit tests for the `tools::permission` module — production code lives in
//! `mo_worker/src/tools/permission.rs`. Wired from there with
//! `#[cfg(test)] #[path = "../tests/tools/permission_tests.rs"] mod tests;` so the tests
//! keep `use super::*` access to the module's items (private ones included).

use super::*;
use crate::tools::{TOOL_CREATE_FILE, TOOL_READ_FILE};
use mo_core::{AskUserQuestion, Mode, Session};

fn ctx(dir: &tempfile::TempDir, mode: Mode, parent_id: Option<String>) -> ToolContext {
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("notes.txt"), "codebase\n").unwrap();
    let scratch = dir.path().join("data/sessions/s/tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    let journal = dir.path().join("data/sessions/s/journal.jsonl");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    ToolContext {
        workdir,
        data_dir: dir.path().join("data"),
        agents_dir: dir.path().join("agents"),
        session: Session {
            id: "s".into(),
            parent_id,
            workdir: dir.path().join("work").display().to_string(),
            prompt: "p".into(),
            model: "m".into(),
            status: mo_core::SessionStatus::Running,
            mode,
            tools: vec![],
            pid: None,
            journal_path: journal.display().to_string(),
            created_at: "now".into(),
            updated_at: "now".into(),
            heartbeat_at: None,
            error: None,
        },
        scratch,
        subagent_depth: 0,
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        model_base_url: "http://localhost:1".into(),
        model_name: "m".into(),
        auth_token: None,
        context_window: None,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
    }
}

/// The extra read roots the dispatch passes: the session scratch dir (plus
/// skill folders, which are not exercised here).
fn roots(ctx: &ToolContext) -> Vec<PathBuf> {
    vec![ctx.scratch.clone()]
}

/// `read_policy`: files inside the workdir or the scratch dir are Allowed;
/// an existing file outside both asks the user — in every mode.
#[test]
fn read_policy_allowed_roots_and_ask_outside() {
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir, mode, None);
        std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
        let outside = dir.path().join("secret.txt").display().to_string();
        let scratch_file = ctx.scratch.join("note.md");
        std::fs::write(&scratch_file, "scratch\n").unwrap();

        assert!(matches!(
            read_policy(&ctx, "notes.txt", &roots(&ctx)).unwrap(),
            PathPolicy::Run(_)
        ));
        assert!(matches!(
            read_policy(&ctx, &scratch_file.display().to_string(), &roots(&ctx)).unwrap(),
            PathPolicy::Run(_)
        ));
        // Outside every root → Ask (reads ask in all modes).
        assert!(matches!(
            read_policy(&ctx, &outside, &roots(&ctx)).unwrap(),
            PathPolicy::Ask { operation: "read" }
        ));
        // A missing file is a plain error, never a request.
        assert!(read_policy(&ctx, "no-such-file.txt", &roots(&ctx)).is_err());
    }
}

/// `write_policy` in build mode: workdir and scratch are Allowed; an
/// outside path asks the user.
#[test]
fn write_policy_build_allows_roots_and_asks_outside() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, None);
    let scratch_file = ctx.scratch.join("draft.md");
    assert!(matches!(
        write_policy(&ctx, TOOL_CREATE_FILE, "new.txt").unwrap(),
        PathPolicy::Run(_)
    ));
    assert!(matches!(
        write_policy(&ctx, TOOL_CREATE_FILE, &scratch_file.display().to_string()).unwrap(),
        PathPolicy::Run(_)
    ));
    assert!(matches!(
        write_policy(&ctx, TOOL_CREATE_FILE, "/tmp/outside.txt").unwrap(),
        PathPolicy::Ask { operation: "write" }
    ));
}

/// `write_policy` in plan/explore mode: the codebase is read-only (denied
/// with a mode-aware message) and any path outside the scratch dir is
/// denied outright — never a permission request.
#[test]
fn write_policy_plan_denies_without_asking() {
    for mode in [Mode::Plan, Mode::Explore] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir, mode, None);
        let scratch_file = ctx.scratch.join("draft.md");

        // Codebase write → mode-aware denial.
        let err = write_policy(&ctx, TOOL_CREATE_FILE, "new.txt").unwrap_err();
        assert!(err.contains("read-only"), "got: {err}");
        assert!(
            err.contains(&ctx.scratch.display().to_string()),
            "got: {err}"
        );
        // Scratch write → Allowed.
        assert!(matches!(
            write_policy(&ctx, TOOL_CREATE_FILE, &scratch_file.display().to_string()).unwrap(),
            PathPolicy::Run(_)
        ));
        // Outside both → denied, never asked.
        let err = write_policy(&ctx, TOOL_CREATE_FILE, "/tmp/outside.txt").unwrap_err();
        assert!(err.contains("denied"), "got: {err}");
        assert!(
            err.contains(&ctx.scratch.display().to_string()),
            "got: {err}"
        );
    }
}

/// `ask_permission` journals a `PermissionRequest` through the event sink
/// and returns stop-and-wait guidance (the ask_user pattern).
#[test]
fn ask_permission_journals_request_and_returns_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = ask_permission(&ctx, TOOL_READ_FILE, "read", "/etc/hostname", &on_event).unwrap();
    assert!(out.contains("permission request was sent"), "got: {out}");
    assert!(out.contains("/etc/hostname"), "got: {out}");
    assert!(out.contains("Stop working now"), "got: {out}");
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1, "events: {events:#?}");
    match &events[0] {
        JournalEventKind::PermissionRequest {
            request_id,
            tool,
            operation,
            path,
        } => {
            assert_eq!(request_id, "p1");
            assert_eq!(tool, TOOL_READ_FILE);
            assert_eq!(operation, "read");
            assert_eq!(path, "/etc/hostname");
        }
        other => panic!("expected permission_request, got: {other:?}"),
    }
}

/// Subagents cannot ask: the request is shown in the UI, which only root
/// sessions have.
#[test]
fn ask_permission_rejects_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, Some("parent-1".to_string()));
    let no_event = |_: JournalEventKind| {};
    let err = ask_permission(&ctx, TOOL_READ_FILE, "read", "/etc/hostname", &no_event).unwrap_err();
    assert!(err.contains("subagents cannot ask the user"), "got: {err}");
}

/// A second request while one is already pending (of either kind) is
/// refused — the user has not answered yet.
#[test]
fn ask_permission_rejects_when_pending() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, None);
    let no_event = |_: JournalEventKind| {};

    // A pending permission request blocks a new one.
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: TOOL_READ_FILE.into(),
            operation: "read".into(),
            path: "/etc/a".into(),
        })
        .unwrap();
    drop(journal);
    let err = ask_permission(&ctx, TOOL_READ_FILE, "read", "/etc/b", &no_event).unwrap_err();
    assert!(err.contains("already pending"), "got: {err}");

    // A pending clarification question blocks a permission request too.
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::AskUserRequest {
            question: AskUserQuestion {
                question_id: "q1".into(),
                question_title: "t".into(),
                question_text: "q".into(),
                options: vec![],
            },
        })
        .unwrap();
    drop(journal);
    let err = ask_permission(&ctx, TOOL_READ_FILE, "read", "/etc/b", &no_event).unwrap_err();
    assert!(err.contains("already pending"), "got: {err}");
}

/// A previous *allowed* decision for the same `(tool, path)` is
/// remembered: the policy returns `Approved` (the dispatch then runs the
/// fs call with that exact path) instead of asking again.
#[test]
fn allowed_decision_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: TOOL_READ_FILE.into(),
            operation: "read".into(),
            path: outside.clone(),
            allowed: true,
        })
        .unwrap();
    drop(journal);

    match read_policy(&ctx, &outside, &roots(&ctx)).unwrap() {
        PathPolicy::Run(resolved) => {
            assert_eq!(
                resolved,
                dir.path().join("secret.txt").canonicalize().unwrap()
            )
        }
        other => panic!("expected Approved, got: {other:?}"),
    }
    // An unasked sibling path still asks.
    std::fs::write(dir.path().join("other.txt"), "x\n").unwrap();
    let sibling = dir.path().join("other.txt").display().to_string();
    assert!(matches!(
        read_policy(&ctx, &sibling, &roots(&ctx)).unwrap(),
        PathPolicy::Ask { .. }
    ));
}

/// A previous *denied* decision for the same `(tool, path)` is remembered:
/// the policy refuses outright instead of asking again.
#[test]
fn denied_decision_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: TOOL_READ_FILE.into(),
            operation: "read".into(),
            path: outside.clone(),
            allowed: false,
        })
        .unwrap();
    drop(journal);

    let err = read_policy(&ctx, &outside, &roots(&ctx)).unwrap_err();
    assert!(err.contains("denied this request earlier"), "got: {err}");
    assert!(err.contains(&outside), "got: {err}");
}
