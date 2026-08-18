//! Unit tests for the `tools::permission` module — production code lives in
//! `mo_worker/src/tools/permission.rs`. Wired from there with
//! `#[cfg(test)] #[path = "../tests/tools/permission_tests.rs"] mod tests;` so the tests
//! keep `use super::*` access to the module's items (private ones included).

use super::*;
use crate::tools::{TOOL_CREATE_FILE, TOOL_READ_FILE};
use mo_core::{AskUserQuestion, Mode, PermissionDecision, Session};

fn make_ctx(dir: &tempfile::TempDir, mode: Mode, parent_id: Option<String>) -> ToolContext {
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
            skills: vec![],
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
        let ctx = make_ctx(&dir, mode, None);
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
    let ctx = make_ctx(&dir, Mode::Build, None);
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
        let ctx = make_ctx(&dir, mode, None);
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

/// `ask_permission_batch` journals a single batched `PermissionRequest`
/// through the event sink (one item per held call) and returns `Ok(())` —
/// no model guidance, since nothing is sent to the LLM until the user
/// decides.
#[test]
fn ask_permission_batch_journals_one_request_with_all_items() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let items = vec![
        PermissionRequestItem {
            call_id: "call_1".into(),
            tool: TOOL_READ_FILE.into(),
            operation: "read".into(),
            path: "/etc/a".into(),
            arguments: r#"{"path":"/etc/a"}"#.into(),
        },
        PermissionRequestItem {
            call_id: "call_2".into(),
            tool: TOOL_CREATE_FILE.into(),
            operation: "write".into(),
            path: "/tmp/b".into(),
            arguments: r#"{"path":"/tmp/b","content":"x"}"#.into(),
        },
    ];
    ask_permission_batch(&ctx, &items, &on_event).unwrap();
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1, "events: {events:#?}");
    match &events[0] {
        JournalEventKind::PermissionRequest {
            request_id, items, ..
        } => {
            assert_eq!(request_id, "p1");
            assert_eq!(items.len(), 2, "items: {items:#?}");
            assert_eq!(items[0].call_id, "call_1");
            assert_eq!(items[0].tool, TOOL_READ_FILE);
            assert_eq!(items[0].operation, "read");
            assert_eq!(items[0].path, "/etc/a");
            assert_eq!(items[1].call_id, "call_2");
            assert_eq!(items[1].tool, TOOL_CREATE_FILE);
        }
        other => panic!("expected permission_request, got: {other:?}"),
    }
}

/// `preflight` holds exactly the calls whose paths are outside the allowed
/// roots (and the mode permits asking); everything else runs.
#[test]
fn preflight_holds_outside_paths_only() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();

    // Inside the workdir → Run.
    assert!(matches!(
        preflight(
            &ctx,
            TOOL_READ_FILE,
            "read",
            "notes.txt",
            r#"{"path":"notes.txt"}"#,
            "c1",
            &roots(&ctx)
        ),
        Preflight::Run
    ));
    // Outside every root → held with the call's id / arguments.
    match preflight(
        &ctx,
        TOOL_READ_FILE,
        "read",
        &outside,
        &format!(r#"{{"path":"{outside}"}}"#),
        "call_9",
        &roots(&ctx),
    ) {
        Preflight::Permission(item) => {
            assert_eq!(item.call_id, "call_9");
            assert_eq!(item.tool, TOOL_READ_FILE);
            assert_eq!(item.operation, "read");
            assert_eq!(item.path, outside);
            assert!(item.arguments.contains(&outside), "got: {}", item.arguments);
        }
        other => panic!("expected Permission, got: {other:?}"),
    }
    // A missing file is a policy error → Run (the error surfaces at
    // execution time as an ordinary tool result).
    assert!(matches!(
        preflight(
            &ctx,
            TOOL_READ_FILE,
            "read",
            "no-such.txt",
            r#"{"path":"no-such.txt"}"#,
            "c3",
            &roots(&ctx)
        ),
        Preflight::Run
    ));
    // Non-file tools are never held.
    assert!(matches!(
        preflight(
            &ctx,
            crate::tools::TOOL_BASH,
            "read",
            "",
            r#"{"command":"pwd"}"#,
            "c4",
            &roots(&ctx)
        ),
        Preflight::Run
    ));
}

/// A plan-mode outside write is a policy error → `preflight` says Run (the
/// mode-aware denial surfaces at execution time); a build-mode write is
/// held.
#[test]
fn preflight_plan_write_runs_and_build_write_holds() {
    let dir = tempfile::tempdir().unwrap();
    let ctx_plan = make_ctx(&dir, Mode::Plan, None);
    assert!(matches!(
        preflight(
            &ctx_plan,
            TOOL_CREATE_FILE,
            "write",
            "/tmp/outside.txt",
            r#"{"path":"/tmp/outside.txt","content":"x"}"#,
            "c1",
            &roots(&ctx_plan)
        ),
        Preflight::Run
    ));

    let dir = tempfile::tempdir().unwrap();
    let ctx_build = make_ctx(&dir, Mode::Build, None);
    assert!(matches!(
        preflight(
            &ctx_build,
            TOOL_CREATE_FILE,
            "write",
            "/tmp/outside.txt",
            r#"{"path":"/tmp/outside.txt","content":"x"}"#,
            "c1",
            &roots(&ctx_build)
        ),
        Preflight::Permission(_)
    ));
}

/// Subagents cannot ask: the batch request is refused (their journal has no
/// UI).
#[test]
fn ask_permission_batch_rejects_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, Some("parent-1".to_string()));
    let no_event = |_: JournalEventKind| {};
    let items = vec![PermissionRequestItem {
        call_id: "call_1".into(),
        tool: TOOL_READ_FILE.into(),
        operation: "read".into(),
        path: "/etc/hostname".into(),
        arguments: r#"{"path":"/etc/hostname"}"#.into(),
    }];
    let err = ask_permission_batch(&ctx, &items, &no_event).unwrap_err();
    assert!(err.contains("subagents cannot ask the user"), "got: {err}");
}

/// A second request while one is already pending (of either kind) is
/// refused — the user has not answered yet.
#[test]
fn ask_permission_batch_rejects_when_pending() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, None);
    let no_event = |_: JournalEventKind| {};
    let items = vec![PermissionRequestItem {
        call_id: "call_1".into(),
        tool: TOOL_READ_FILE.into(),
        operation: "read".into(),
        path: "/etc/b".into(),
        arguments: r#"{"path":"/etc/b"}"#.into(),
    }];

    // A pending permission request blocks a new one.
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: None,
            operation: None,
            path: None,
            items: vec![PermissionRequestItem {
                call_id: "call_0".into(),
                tool: TOOL_READ_FILE.into(),
                operation: "read".into(),
                path: "/etc/a".into(),
                arguments: r#"{"path":"/etc/a"}"#.into(),
            }],
        })
        .unwrap();
    drop(journal);
    let err = ask_permission_batch(&ctx, &items, &no_event).unwrap_err();
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
    let err = ask_permission_batch(&ctx, &items, &no_event).unwrap_err();
    assert!(err.contains("already pending"), "got: {err}");
}

/// A previous *allowed* decision for the same `(tool, path)` is
/// remembered: the policy returns `Approved` (the dispatch then runs the
/// fs call with that exact path) instead of asking again — from both the
/// batched (`decisions`) and the legacy single-item answer shapes.
#[test]
fn allowed_decision_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: None,
            operation: None,
            path: None,
            allowed: None,
            decisions: vec![PermissionDecision {
                call_id: "call_1".into(),
                tool: TOOL_READ_FILE.into(),
                operation: "read".into(),
                path: outside.clone(),
                allowed: true,
            }],
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

    // Legacy single-item answers are remembered too. (A fresh block scope
    // so the `ctx` binding above does not shadow the helper fn.)
    {
        let dir = tempfile::tempdir().unwrap();
        let legacy_ctx = make_ctx(&dir, Mode::Build, None);
        std::fs::write(dir.path().join("legacy.txt"), "x\n").unwrap();
        let legacy = dir.path().join("legacy.txt").display().to_string();
        let mut journal =
            mo_core::JournalWriter::open(std::path::Path::new(&legacy_ctx.session.journal_path))
                .unwrap();
        journal
            .append(JournalEventKind::PermissionAnswered {
                request_id: "p1".into(),
                tool: Some(TOOL_READ_FILE.into()),
                operation: Some("read".into()),
                path: Some(legacy.clone()),
                allowed: Some(true),
                decisions: Vec::new(),
            })
            .unwrap();
        drop(journal);
        assert!(matches!(
            read_policy(&legacy_ctx, &legacy, &roots(&legacy_ctx)).unwrap(),
            PathPolicy::Run(_)
        ));
    }
}

/// A previous *denied* decision for the same `(tool, path)` is remembered:
/// the policy refuses outright instead of asking again.
#[test]
fn denied_decision_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: None,
            operation: None,
            path: None,
            allowed: None,
            decisions: vec![PermissionDecision {
                call_id: "call_1".into(),
                tool: TOOL_READ_FILE.into(),
                operation: "read".into(),
                path: outside.clone(),
                allowed: false,
            }],
        })
        .unwrap();
    drop(journal);

    let err = read_policy(&ctx, &outside, &roots(&ctx)).unwrap_err();
    assert!(err.contains("denied permission"), "got: {err}");
    assert!(err.contains(&outside), "got: {err}");
}
