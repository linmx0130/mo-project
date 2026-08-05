//! Tool registry: OpenAI nested tool definitions plus argument validation
//! and dispatch.

pub mod bash;
pub mod fs;
pub mod request_mode_change;
pub mod skill;
pub mod subagent;

use std::path::PathBuf;

use mo_core::{JournalEventKind, Mode, Session};
use serde::Deserialize;
use serde_json::{Value, json};

pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_EDIT_FILE: &str = "edit_file";
pub const TOOL_CREATE_FILE: &str = "create_file";
pub const TOOL_REMOVE_FILE: &str = "remove_file";
pub const TOOL_BASH: &str = "bash";
pub const TOOL_SPAWN_SUBAGENT: &str = "spawn_subagent";
pub const TOOL_LOAD_SKILL: &str = "load_skill";
pub const TOOL_REQUEST_MODE_CHANGE: &str = "request_mode_change";

/// Everything a tool needs to run: the sandboxed workdir, the shared data
/// dir (for subagent sessions), the global agents dir (passed down so
/// subagents inject the same global instructions/skills), this worker's
/// session row, model config, the session scratch dir (`<data_dir>/
/// sessions/<id>/tmp`) where non-Build modes may create/edit/remove files,
/// and the max tool concurrency (passed down to subagents).
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workdir: PathBuf,
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub session: Session,
    pub scratch: PathBuf,
    pub subagent_depth: u32,
    /// Max number of tool calls from a single assistant message that
    /// execute concurrently (clamped to at least 1); passed to spawned
    /// subagents so they inherit the same bound.
    pub max_tool_concurrency: usize,
    pub model_base_url: String,
    pub model_name: String,
    pub auth_token: Option<String>,
}

/// OpenAI nested tool definitions advertised to the model.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": TOOL_READ_FILE,
                "description": "Read a UTF-8 text file inside the working directory or inside a global skill folder (the load_skill tool returns skill folder paths). Output is capped at ~1 MB.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the working directory (or absolute)." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_EDIT_FILE,
                "description": "Replace old_string with new_string in a file. The match must be unique unless replace_all is true. Returns the full new file content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the working directory (or absolute)." },
                        "old_string": { "type": "string", "description": "Exact text to replace. Must appear exactly once unless replace_all is set." },
                        "new_string": { "type": "string", "description": "Replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence. Default false." }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_CREATE_FILE,
                "description": "Create a new file with the given content inside the working directory. The parent directory must already exist and the file must not exist (use edit_file to modify existing files). Returns the content written.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the new file, relative to the working directory (or absolute)." },
                        "content": { "type": "string", "description": "Full content to write to the file." }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_REMOVE_FILE,
                "description": "Remove a regular file inside the working directory. Directories and symlinks are refused. Returns a confirmation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file to remove, relative to the working directory (or absolute)." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_BASH,
                "description": "Run a shell command via sh -c in the working directory. 120s timeout — the whole process group is killed on timeout. Avoid piping output through `tail`/`head`: it buffers everything, so nothing is streamed to the UI until the command ends (and a timeout looks like a hang). For long-running builds (Gradle, etc.), run them in the background (`nohup ... > build.log 2>&1 &`) and poll the log file. Returns stdout, stderr and the exit code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run." }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_SPAWN_SUBAGENT,
                "description": "Spawn a subagent (a nested agent session with the same working directory) to work on a self-contained subtask, and wait for its final answer. Subagents cannot spawn further subagents (the depth hard limit is 1). The subagent runs in the given mode (default: this session's current mode) — build has full access; plan and explore keep the codebase read-only (writes go to the subagent's own scratch dir).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "Self-contained instructions for the subagent." },
                        "mode": { "type": "string", "enum": ["build", "plan", "explore"], "description": "Mode for the subagent: build (default, full access), plan (plan-only, codebase read-only), explore (investigate, codebase read-only). Defaults to this session's current mode." }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_LOAD_SKILL,
                "description": "Load a global skill's full instructions on demand. Skills are listed in the system prompt with only their name and description; pass the skill name to get its SKILL.md content and the absolute path of its folder, from which reference files, scripts, and other resources can be read with read_file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Name of the skill to load." }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": TOOL_REQUEST_MODE_CHANGE,
                "description": "Request the user (in the UI) to switch this session's mode. Use this when the task needs a mode you do not have — e.g. you are in plan/explore mode and need to modify the codebase, or you are in build mode and only need to plan/explore. The user approves or rejects the request in the UI; on approval the session switches mode and continues the run, so you can then do the work. Root sessions only: subagents must ask their parent agent instead. Write `message` in the user's language (the language the user writes in).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": { "type": "string", "enum": ["build", "plan", "explore"], "description": "The mode to switch to." },
                        "message": { "type": "string", "description": "A short message for the user, in the user's language, explaining why the mode switch is needed and what you will do once approved." }
                    },
                    "required": ["mode", "message"],
                    "additionalProperties": false
                }
            }
        }),
    ]
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct CreateFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct RemoveFileArgs {
    path: String,
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

#[derive(Deserialize)]
struct SpawnSubagentArgs {
    prompt: String,
    /// Optional mode for the subagent (`build` | `plan` | `explore`);
    /// defaults to this session's current mode.
    mode: Option<String>,
}

#[derive(Deserialize)]
struct LoadSkillArgs {
    name: String,
}

/// The root a file-mutation tool may write under: the workdir in Build
/// mode, the session scratch dir otherwise. In non-Build modes a path that
/// resolves inside the codebase is denied up front with a mode-aware
/// message (scratch writes use absolute paths under the scratch dir).
fn write_root_for(ctx: &ToolContext, raw: &str) -> Result<PathBuf, String> {
    if ctx.session.mode == Mode::Build {
        return Ok(ctx.workdir.clone());
    }
    if fs::resolve_path(&ctx.workdir, raw).is_ok() {
        return Err(format!(
            "{} mode: the codebase is read-only; create/edit/remove is only allowed under {} (use an absolute path)",
            ctx.session.mode.as_str(),
            ctx.scratch.display()
        ));
    }
    Ok(ctx.scratch.clone())
}

/// Execute one tool call. Returns the tool output (Ok) or a tool error
/// (Err) — both are surfaced to the model; only a failed dispatch returns
/// Err and both are journaled as ToolResult events.
///
/// `tool_call_id` identifies the call (used for streamed output events) and
/// `on_event` receives non-result journal events while a tool runs: the
/// `ToolOutputDelta` chunks of a streaming tool (bash), or the
/// `ModeChangeRequest` a `request_mode_change` call emits — the caller
/// appends them to the journal so the frontend sees them live. `on_event`
/// is a shared (non-`mut`) sink: with parallel tool execution, concurrent
/// calls journal through the same closure.
pub async fn execute_tool(
    ctx: &ToolContext,
    name: &str,
    arguments: &str,
    tool_call_id: &str,
    on_event: &(dyn Fn(JournalEventKind) + Send + Sync),
) -> Result<String, String> {
    match name {
        TOOL_READ_FILE => {
            let args: ReadFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            // `read_file` may also read global skill folders (and the
            // scratch dir), so pass those roots as extra allowed roots.
            let mut roots: Vec<PathBuf> = crate::skills::discover_skills(&ctx.agents_dir)
                .into_iter()
                .map(|s| s.path)
                .collect();
            roots.push(ctx.scratch.clone());
            fs::read_file(&ctx.workdir, &args.path, &roots)
        }
        TOOL_EDIT_FILE => {
            let args: EditFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            let root = write_root_for(ctx, &args.path)?;
            fs::edit_file(
                &root,
                &args.path,
                &args.old_string,
                &args.new_string,
                args.replace_all,
            )
        }
        TOOL_CREATE_FILE => {
            let args: CreateFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            let root = write_root_for(ctx, &args.path)?;
            fs::create_file(&root, &args.path, &args.content)
        }
        TOOL_REMOVE_FILE => {
            let args: RemoveFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            let root = write_root_for(ctx, &args.path)?;
            fs::remove_file(&root, &args.path)
        }
        TOOL_BASH => {
            let args: BashArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            bash::bash(
                &ctx.workdir,
                &args.command,
                bash::DEFAULT_TIMEOUT,
                tool_call_id,
                on_event,
            )
            .await
        }
        TOOL_SPAWN_SUBAGENT => {
            let args: SpawnSubagentArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            let mode = match args.mode {
                Some(raw) => raw
                    .parse::<Mode>()
                    .map_err(|e| format!("invalid arguments for {name}: {e}"))?,
                None => ctx.session.mode,
            };
            subagent::spawn_subagent(ctx, &args.prompt, mode, tool_call_id, on_event).await
        }
        TOOL_LOAD_SKILL => {
            let args: LoadSkillArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            skill::load_skill(&ctx.agents_dir, &args.name)
        }
        TOOL_REQUEST_MODE_CHANGE => {
            let args: request_mode_change::RequestModeChangeArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            request_mode_change::request_mode_change(ctx, &args, on_event)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
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
        }
    }

    #[test]
    fn definitions_cover_all_tools() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 8);
        let names: Vec<&str> = defs
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&TOOL_READ_FILE));
        assert!(names.contains(&TOOL_EDIT_FILE));
        assert!(names.contains(&TOOL_CREATE_FILE));
        assert!(names.contains(&TOOL_REMOVE_FILE));
        assert!(names.contains(&TOOL_BASH));
        assert!(names.contains(&TOOL_SPAWN_SUBAGENT));
        assert!(names.contains(&TOOL_LOAD_SKILL));
        assert!(names.contains(&TOOL_REQUEST_MODE_CHANGE));
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
}
