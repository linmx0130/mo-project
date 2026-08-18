//! Unit tests for the `tools` module — production code lives in
//! `mo_worker/src/tools/mod.rs`. Wired from there with `#[cfg(test)] #[path = "../tests/tools/mod_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use mo_core::Mode;

fn test_ctx(agents_dir: PathBuf) -> ToolContext {
    let scratch = PathBuf::from("/tmp/data/sessions/s/tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    ToolContext {
        workdir: PathBuf::from("/tmp"),
        data_dir: PathBuf::from("/tmp/data"),
        agents_dir,
        session: Session {
            id: "s".into(),
            parent_id: None,
            workdir: "/tmp".into(),
            prompt: "p".into(),
            model: "m".into(),
            status: mo_core::SessionStatus::Running,
            mode: Mode::Build,
            tools: vec![],
            skills: vec![],
            pid: None,
            journal_path: "/tmp/j.jsonl".into(),
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

#[test]
fn definitions_cover_all_tools() {
    let defs = tool_definitions(&[]);
    assert_eq!(defs.len(), 10);
    let names: Vec<&str> = defs
        .iter()
        .map(|d| d["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&TOOL_READ_FILE));
    assert!(names.contains(&TOOL_EDIT_FILE));
    assert!(names.contains(&TOOL_CREATE_FILE));
    assert!(names.contains(&TOOL_REMOVE_FILE));
    assert!(names.contains(&TOOL_BASH));
    assert!(names.contains(&TOOL_BASH_IN_BACKGROUND));
    assert!(names.contains(&TOOL_SPAWN_SUBAGENT));
    assert!(names.contains(&TOOL_LOAD_SKILL));
    assert!(names.contains(&TOOL_REQUEST_MODE_CHANGE));
    assert!(names.contains(&TOOL_ASK_USER));
    // The request_mode_change definition carries the mode enum and the
    // user-language guidance.
    let def = defs
        .iter()
        .find(|d| d["function"]["name"] == TOOL_REQUEST_MODE_CHANGE)
        .unwrap();
    assert_eq!(
        def["function"]["parameters"]["properties"]["mode"]["enum"],
        json!(["build", "plan", "explore"])
    );
    assert!(
        def["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
            .len()
            == 2
    );
    assert!(
        def["function"]["description"]
            .as_str()
            .unwrap()
            .contains("user's language")
    );
    // The description carries the plan-mode finishing cue: call once the
    // plan is ready with no must-answer open questions.
    let description = def["function"]["description"].as_str().unwrap();
    assert!(description.contains("plan is ready"), "got: {description}");
    assert!(
        description.contains("must-answer questions"),
        "got: {description}"
    );

    // The ask_user definition carries the single-question shape: title,
    // text and an options array of {option_title, option_text}; the
    // description tells the model to stop and wait for the answer.
    let def = defs
        .iter()
        .find(|d| d["function"]["name"] == TOOL_ASK_USER)
        .unwrap();
    let params = &def["function"]["parameters"];
    assert_eq!(
        params["properties"]["question_title"]["type"], "string",
        "got: {params}"
    );
    assert_eq!(
        params["properties"]["question_text"]["type"], "string",
        "got: {params}"
    );
    assert_eq!(
        params["properties"]["options"]["items"]["properties"]["option_title"]["type"], "string",
        "got: {params}"
    );
    assert_eq!(
        params["properties"]["options"]["items"]["properties"]["option_text"]["type"], "string",
        "got: {params}"
    );
    assert_eq!(
        params["required"],
        json!(["question_title", "question_text", "options"]),
        "got: {params}"
    );
    let description = def["function"]["description"].as_str().unwrap();
    assert!(
        description.contains("one question per call"),
        "got: {description}"
    );
    assert!(description.contains("question_id"), "got: {description}");

    // The bash_in_background definition carries the run/kill/status action
    // enum and tells the model to redirect output (background output is
    // discarded).
    let def = defs
        .iter()
        .find(|d| d["function"]["name"] == TOOL_BASH_IN_BACKGROUND)
        .unwrap();
    assert_eq!(
        def["function"]["parameters"]["properties"]["action"]["enum"],
        json!(["run", "kill", "status"])
    );
    assert_eq!(
        def["function"]["parameters"]["required"],
        json!(["action"]),
        "got: {}",
        def["function"]["parameters"]["required"]
    );
    let description = def["function"]["description"].as_str().unwrap();
    assert!(description.contains("process id"), "got: {description}");
    assert!(description.contains("discarded"), "got: {description}");
}

#[tokio::test]
async fn unknown_tool_errors() {
    let ctx = test_ctx(PathBuf::from("/tmp/agents"));
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(&ctx, "nope", "{}", "call_x", &no_event)
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));
}

#[tokio::test]
async fn bash_in_background_dispatches_and_validates() {
    let ctx = test_ctx(PathBuf::from("/tmp/agents"));
    let no_event = |_: JournalEventKind| {};

    // `run` requires a command, `status`/`kill` require a process id, and
    // an unknown action is rejected before any process is spawned.
    let err = execute_tool(
        &ctx,
        TOOL_BASH_IN_BACKGROUND,
        r#"{"action":"run"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("requires `command`"), "got: {err}");

    let err = execute_tool(
        &ctx,
        TOOL_BASH_IN_BACKGROUND,
        r#"{"action":"status"}"#,
        "c2",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("requires `process_id`"), "got: {err}");

    let err = execute_tool(
        &ctx,
        TOOL_BASH_IN_BACKGROUND,
        r#"{"action":"kill"}"#,
        "c3",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("requires `process_id`"), "got: {err}");

    let err = execute_tool(
        &ctx,
        TOOL_BASH_IN_BACKGROUND,
        r#"{"action":"explode"}"#,
        "c4",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("unknown action"), "got: {err}");
}

#[tokio::test]
async fn load_skill_returns_path_and_skill_md() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    let skill_dir = agents.join("greeter");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: greeter\ndescription: Greets people.\n---\n# Greeter\nSay hello.\n",
    )
    .unwrap();
    let ctx = test_ctx(agents);
    let no_event = |_: JournalEventKind| {};
    let out = execute_tool(
        &ctx,
        TOOL_LOAD_SKILL,
        r#"{"name":"greeter"}"#,
        "call_s",
        &no_event,
    )
    .await
    .unwrap();
    let path_line = format!("Path: {}", skill_dir.canonicalize().unwrap().display());
    assert!(out.starts_with(&path_line), "got: {out}");
    assert!(out.contains("# Greeter"));
    assert!(out.contains("Say hello."));

    // Unknown skill name errors.
    let err = execute_tool(
        &ctx,
        TOOL_LOAD_SKILL,
        r#"{"name":"nope"}"#,
        "call_s",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("skill not found"), "got: {err}");
}

/// A plan/explore-mode context whose workdir is a real temp dir.
fn plan_ctx(dir: &tempfile::TempDir, mode: Mode) -> ToolContext {
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("notes.txt"), "codebase\n").unwrap();
    // Canonicalized, like `run_agent` resolves it in production (the
    // read roots compare canonical paths).
    let scratch = dir.path().join("data/sessions/s/tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    ToolContext {
        workdir,
        data_dir: dir.path().join("data"),
        agents_dir: dir.path().join("agents"),
        session: Session {
            id: "s".into(),
            parent_id: None,
            workdir: dir.path().join("work").display().to_string(),
            prompt: "p".into(),
            model: "m".into(),
            status: mo_core::SessionStatus::Running,
            mode,
            tools: vec![],
            skills: vec![],
            pid: None,
            journal_path: "/tmp/j.jsonl".into(),
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

/// Non-Build modes deny codebase mutations but allow them in scratch.
#[tokio::test]
async fn plan_mode_denies_codebase_writes_and_allows_scratch() {
    for mode in [Mode::Plan, Mode::Explore] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = plan_ctx(&dir, mode);
        let no_event = |_: JournalEventKind| {};

        // Writing into the codebase (relative or absolute) is denied
        // with a mode-aware message.
        for args in [
            r#"{"path":"new.txt","content":"x"}"#.to_string(),
            format!(
                r#"{{"path":"{}","content":"x"}}"#,
                ctx.workdir.join("new.txt").display()
            ),
        ] {
            let err = execute_tool(&ctx, TOOL_CREATE_FILE, &args, "c1", &no_event)
                .await
                .unwrap_err();
            assert!(err.contains("read-only"), "got: {err}");
            assert!(
                err.contains(&ctx.scratch.display().to_string()),
                "got: {err}"
            );
        }
        // Editing an existing codebase file is denied too.
        let err = execute_tool(
            &ctx,
            TOOL_EDIT_FILE,
            r#"{"path":"notes.txt","old_string":"codebase","new_string":"hacked"}"#,
            "c2",
            &no_event,
        )
        .await
        .unwrap_err();
        assert!(err.contains("read-only"), "got: {err}");
        // Removing a codebase file is denied.
        let err = execute_tool(
            &ctx,
            TOOL_REMOVE_FILE,
            r#"{"path":"notes.txt"}"#,
            "c3",
            &no_event,
        )
        .await
        .unwrap_err();
        assert!(err.contains("read-only"), "got: {err}");

        // Absolute paths under the scratch dir work: create, read,
        // edit, remove.
        let scratch_file = ctx.scratch.join("draft.md");
        let create_args = format!(
            r#"{{"path":"{}","content":"draft"}}"#,
            scratch_file.display()
        );
        execute_tool(&ctx, TOOL_CREATE_FILE, &create_args, "c4", &no_event)
            .await
            .unwrap();
        let read_args = format!(r#"{{"path":"{}"}}"#, scratch_file.display());
        let out = execute_tool(&ctx, TOOL_READ_FILE, &read_args, "c5", &no_event)
            .await
            .unwrap();
        assert!(out.contains("draft"), "got: {out}");
        let edit_args = format!(
            r#"{{"path":"{}","old_string":"draft","new_string":"plan"}}"#,
            scratch_file.display()
        );
        execute_tool(&ctx, TOOL_EDIT_FILE, &edit_args, "c6", &no_event)
            .await
            .unwrap();
        let remove_args = format!(r#"{{"path":"{}"}}"#, scratch_file.display());
        execute_tool(&ctx, TOOL_REMOVE_FILE, &remove_args, "c7", &no_event)
            .await
            .unwrap();
        assert!(!scratch_file.exists());
        // The codebase file is untouched.
        assert_eq!(
            std::fs::read_to_string(ctx.workdir.join("notes.txt")).unwrap(),
            "codebase\n"
        );
    }
}

/// Build mode keeps writing to the codebase (and stays the default).
#[tokio::test]
async fn build_mode_writes_to_workdir() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = plan_ctx(&dir, Mode::Build);
    let no_event = |_: JournalEventKind| {};
    execute_tool(
        &ctx,
        TOOL_CREATE_FILE,
        r#"{"path":"new.txt","content":"built"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(ctx.workdir.join("new.txt")).unwrap(),
        "built"
    );
}

/// The subagent tool validates its `mode` argument.
#[tokio::test]
async fn spawn_subagent_rejects_unknown_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = plan_ctx(&dir, Mode::Build);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_SPAWN_SUBAGENT,
        r#"{"prompt":"do it","mode":"nope"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("invalid arguments"), "got: {err}");
}

/// A non-Build-mode ctx with a real journal file, for the
/// `request_mode_change` tests (the tool reads the journal to decide
/// whether a request is already pending).
fn request_ctx(dir: &tempfile::TempDir, mode: Mode, parent_id: Option<String>) -> ToolContext {
    let mut ctx = plan_ctx(dir, mode);
    ctx.session.parent_id = parent_id;
    let journal = dir.path().join("data/sessions/s/journal.jsonl");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    ctx.session.journal_path = journal.display().to_string();
    ctx
}

/// A plan-mode agent requests build: the tool journals a
/// `ModeChangeRequest` through the event sink and returns guidance
/// telling the model to stop and wait for the user.
#[tokio::test]
async fn request_mode_change_journals_event_and_returns_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"build","message":"may I switch to build mode to implement the plan?"}"#,
        "call_rmc",
        &on_event,
    )
    .await
    .unwrap();
    assert!(out.contains("Mode change request sent"), "got: {out}");
    assert!(out.contains("build"), "got: {out}");
    assert!(out.contains("Stop working now"), "got: {out}");
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1, "events: {events:#?}");
    match &events[0] {
        JournalEventKind::ModeChangeRequest { mode, message } => {
            assert_eq!(*mode, Mode::Build);
            assert_eq!(message, "may I switch to build mode to implement the plan?");
        }
        other => panic!("expected mode_change_request, got: {other:?}"),
    }
}

/// Requesting the mode the session is already in is a no-op error.
#[tokio::test]
async fn request_mode_change_rejects_same_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"plan","message":"let me stay in plan mode"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("already in plan mode"), "got: {err}");
}

/// Subagents cannot request a mode change: the request is shown to the
/// user in the UI, which only root sessions have.
#[tokio::test]
async fn request_mode_change_rejects_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, Some("parent-1".to_string()));
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"build","message":"may I switch?"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("subagents cannot request"), "got: {err}");
}

/// A second request while one is already pending is refused — the user
/// has not answered yet.
#[tokio::test]
async fn request_mode_change_rejects_when_pending() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::ModeChangeRequest {
            mode: Mode::Build,
            message: "first request".to_string(),
        })
        .unwrap();
    drop(journal);

    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"build","message":"second request"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("already pending"), "got: {err}");
}

/// After the pending request was resolved (approved or rejected), a new
/// request is allowed again.
#[tokio::test]
async fn request_mode_change_allowed_after_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::ModeChangeRequest {
            mode: Mode::Build,
            message: "first request".to_string(),
        })
        .unwrap();
    // Resolved by the user's rejection.
    journal
        .append(JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build })
        .unwrap();
    drop(journal);

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"build","message":"second request"}"#,
        "call_2",
        &on_event,
    )
    .await
    .unwrap();
    assert!(out.contains("Mode change request sent"), "got: {out}");
    assert_eq!(events.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
}

/// An unparseable mode is rejected as invalid arguments.
#[tokio::test]
async fn request_mode_change_rejects_invalid_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"nope","message":"hi"}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("invalid arguments"), "got: {err}");
}

/// An empty request message is rejected.
#[tokio::test]
async fn request_mode_change_rejects_empty_message() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_REQUEST_MODE_CHANGE,
        r#"{"mode":"build","message":"  "}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("message must not be empty"), "got: {err}");
}

/// `tool_definitions` drops the schemas of disabled tools: the model only
/// ever sees the tools the session enabled. An empty enabled list (legacy
/// sessions) advertises everything.
#[test]
fn definitions_filter_out_disabled_tools() {
    // A session with ask_user + spawn_subagent turned off.
    let enabled: Vec<String> =
        mo_core::resolve_enabled_tools(&["ask_user".to_string(), "spawn_subagent".to_string()])
            .unwrap();
    let defs = tool_definitions(&enabled);
    let names: Vec<&str> = defs
        .iter()
        .map(|d| d["function"]["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&TOOL_ASK_USER), "got: {names:?}");
    assert!(!names.contains(&TOOL_SPAWN_SUBAGENT), "got: {names:?}");
    // The fixed tools and the remaining toggleable ones stay.
    for name in [
        TOOL_READ_FILE,
        TOOL_EDIT_FILE,
        TOOL_CREATE_FILE,
        TOOL_REMOVE_FILE,
        TOOL_BASH,
        TOOL_BASH_IN_BACKGROUND,
        TOOL_LOAD_SKILL,
        TOOL_REQUEST_MODE_CHANGE,
    ] {
        assert!(names.contains(&name), "{name} must stay: {names:?}");
    }
    assert_eq!(defs.len(), 8, "got: {names:?}");
}

/// The execution gate refuses a disabled tool even if the model
/// hallucinates its name (defense in depth: the schema was not injected,
/// but the call must never be dispatched).
#[tokio::test]
async fn execute_tool_refuses_disabled_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = plan_ctx(&dir, Mode::Build);
    ctx.session.tools = mo_core::resolve_enabled_tools(&["ask_user".to_string()]).unwrap();
    // The ask_user tool reads the session journal; give it a real file.
    let journal = dir.path().join("data/sessions/s/journal.jsonl");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, "").unwrap();
    ctx.session.journal_path = journal.display().to_string();
    let no_event = |_: JournalEventKind| {};

    // The muted tool is refused with a clear message.
    let err = execute_tool(
        &ctx,
        TOOL_ASK_USER,
        r#"{"question_title":"t","question_text":"q","options":[]}"#,
        "c1",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("disabled in this session"), "got: {err}");

    // An enabled tool still works (and fixed tools are always enabled).
    let out = execute_tool(&ctx, TOOL_BASH, r#"{"command":"echo hi"}"#, "c2", &no_event)
        .await
        .unwrap();
    assert!(out.contains("hi"), "got: {out}");

    // A legacy session (empty enabled list = no restriction) can still
    // call the tool.
    ctx.session.tools = vec![];
    let out = execute_tool(
        &ctx,
        TOOL_ASK_USER,
        r#"{"question_title":"t","question_text":"q","options":[]}"#,
        "c3",
        &no_event,
    )
    .await;
    assert!(out.is_ok(), "legacy sessions keep every tool: {out:?}");
}

/// A `read_file` call targeting an existing file outside the workdir (and
/// outside the scratch dir) journals a `PermissionRequest` through the
/// event sink and returns stop-and-wait guidance — in any mode.
#[tokio::test]
async fn read_outside_journals_permission_request_and_returns_guidance() {
    for mode in [Mode::Build, Mode::Plan] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = request_ctx(&dir, mode, None);
        std::fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
        let outside = dir.path().join("secret.txt").display().to_string();
        let args = format!(r#"{{"path":"{outside}"}}"#);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
        let on_event = {
            let events = std::sync::Arc::clone(&events);
            move |kind: JournalEventKind| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
            }
        };
        let out = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &on_event)
            .await
            .unwrap();
        assert!(out.contains("permission request was sent"), "got: {out}");
        assert!(out.contains("Stop working now"), "got: {out}");
        let events = events.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(events.len(), 1, "events: {events:#?}");
        match &events[0] {
            JournalEventKind::PermissionRequest {
                request_id,
                tool: Some(tool),
                operation: Some(operation),
                path: Some(path),
                items,
            } => {
                assert_eq!(request_id, "p1");
                assert_eq!(tool, TOOL_READ_FILE);
                assert_eq!(operation, "read");
                assert_eq!(path, &outside);
                // The fallback journals the legacy single-item shape: no
                // batched items (the batched flow holds calls before
                // execution instead of reaching this per-call path).
                assert!(items.is_empty(), "items: {items:#?}");
            }
            other => panic!("expected permission_request, got: {other:?}"),
        }
    }
}

/// A build-mode write outside the workdir and scratch dir asks the user;
/// a plan-mode write to the same path is denied outright — never asked.
#[tokio::test]
async fn write_outside_asks_in_build_but_is_denied_in_plan() {
    // Build: the write asks the user.
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(
        &ctx,
        TOOL_CREATE_FILE,
        r#"{"path":"/tmp/outside.txt","content":"x"}"#,
        "c1",
        &on_event,
    )
    .await
    .unwrap();
    assert!(out.contains("permission request was sent"), "got: {out}");
    {
        let events = events.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(events.len(), 1, "events: {events:#?}");
        assert!(matches!(
            &events[0],
            JournalEventKind::PermissionRequest { operation: Some(op), .. } if op == "write"
        ));
    }
    // The guard is dropped: the Plan branch below awaits execute_tool again.

    // Plan: the same write is denied without any request.
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Plan, None);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(
        &ctx,
        TOOL_CREATE_FILE,
        r#"{"path":"/tmp/outside.txt","content":"x"}"#,
        "c2",
        &no_event,
    )
    .await
    .unwrap_err();
    assert!(err.contains("denied"), "got: {err}");
    assert!(
        err.contains(&ctx.scratch.display().to_string()),
        "got: {err}"
    );
    assert!(
        mo_core::read_events(std::path::Path::new(&ctx.session.journal_path))
            .unwrap()
            .is_empty(),
        "plan-mode outside write must not journal anything"
    );
}

/// A subagent cannot ask the user for permission: the request is shown in
/// the UI, which only root sessions have.
#[tokio::test]
async fn subagent_read_outside_is_rejected_not_asked() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, Some("parent-1".to_string()));
    std::fs::write(dir.path().join("secret.txt"), "x\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let args = format!(r#"{{"path":"{outside}"}}"#);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &no_event)
        .await
        .unwrap_err();
    assert!(err.contains("subagents cannot ask the user"), "got: {err}");
}

/// After the user allowed a request, a retry of the same `(tool, path)`
/// runs without prompting again — the decision is remembered.
#[tokio::test]
async fn allowed_retry_reads_file_without_asking_again() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "secret content\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: Some(TOOL_READ_FILE.into()),
            operation: Some("read".into()),
            path: Some(outside.clone()),
            allowed: Some(true),
            decisions: Vec::new(),
        })
        .unwrap();
    drop(journal);

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let args = format!(r#"{{"path":"{outside}"}}"#);
    let out = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &on_event)
        .await
        .unwrap();
    assert!(out.contains("secret content"), "got: {out}");
    assert!(
        events.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "an allowed retry must not journal a new permission request"
    );
}

/// After the user denied a request, a retry of the same `(tool, path)` is
/// refused outright — no second prompt.
#[tokio::test]
async fn denied_retry_is_refused_without_asking_again() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "x\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: Some(TOOL_READ_FILE.into()),
            operation: Some("read".into()),
            path: Some(outside.clone()),
            allowed: Some(false),
            decisions: Vec::new(),
        })
        .unwrap();
    drop(journal);

    let no_event = |_: JournalEventKind| {};
    let args = format!(r#"{{"path":"{outside}"}}"#);
    let err = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &no_event)
        .await
        .unwrap_err();
    assert!(err.contains("denied permission"), "got: {err}");
}

/// One pending user-facing request at a time: a pending clarification
/// question blocks a permission request, and a pending permission request
/// blocks another one.
#[tokio::test]
async fn pending_requests_block_new_permission_requests() {
    // A pending ask_user question blocks the permission request.
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("secret.txt"), "x\n").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    let args = format!(r#"{{"path":"{outside}"}}"#);
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::AskUserRequest {
            question: mo_core::AskUserQuestion {
                question_id: "q1".into(),
                question_title: "t".into(),
                question_text: "q".into(),
                options: vec![],
            },
        })
        .unwrap();
    drop(journal);
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &no_event)
        .await
        .unwrap_err();
    assert!(
        err.contains("clarification question is already pending"),
        "got: {err}"
    );

    // A pending permission request blocks another one.
    let dir = tempfile::tempdir().unwrap();
    let ctx = request_ctx(&dir, Mode::Build, None);
    std::fs::write(dir.path().join("other.txt"), "x\n").unwrap();
    let other = dir.path().join("other.txt").display().to_string();
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: None,
            operation: None,
            path: None,
            items: vec![mo_core::PermissionRequestItem {
                call_id: "call_0".into(),
                tool: TOOL_READ_FILE.into(),
                operation: "read".into(),
                path: "/etc/a".into(),
                arguments: r#"{"path":"/etc/a"}"#.into(),
            }],
        })
        .unwrap();
    drop(journal);
    let args = format!(r#"{{"path":"{other}"}}"#);
    let err = execute_tool(&ctx, TOOL_READ_FILE, &args, "c1", &no_event)
        .await
        .unwrap_err();
    assert!(
        err.contains("file-access permission request is already pending"),
        "got: {err}"
    );
}
