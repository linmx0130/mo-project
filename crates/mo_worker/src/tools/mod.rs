//! Tool registry: OpenAI nested tool definitions plus argument validation
//! and dispatch.

pub mod ask_user;
pub mod bash;
pub mod bash_in_background;
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
pub const TOOL_BASH_IN_BACKGROUND: &str = "bash_in_background";
pub const TOOL_SPAWN_SUBAGENT: &str = "spawn_subagent";
pub const TOOL_LOAD_SKILL: &str = "load_skill";
pub const TOOL_REQUEST_MODE_CHANGE: &str = "request_mode_change";
pub const TOOL_ASK_USER: &str = "ask_user";

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
    /// The session's model context window in tokens (`None` = unlimited),
    /// passed to spawned subagents so they compress against the parent's
    /// resolved model settings.
    pub context_window: Option<u64>,
    /// The context-compression threshold (fraction of the context window),
    /// passed to spawned subagents so they inherit the same value.
    pub context_compression_threshold: f64,
}

/// OpenAI nested tool definitions advertised to the model.
///
/// `enabled` is the session's enabled-tool list (`Session::tools`): the
/// schemas of disabled tools are *not* injected into the prompt, so the
/// model cannot call them. An empty list means "no restriction" (legacy
/// sessions) — every tool is advertised. The fixed tools (bash + file
/// operations) are always in the session list, so they are always
/// advertised.
pub fn tool_definitions(enabled: &[String]) -> Vec<Value> {
    let all = vec![
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
                "description": "Run a shell command via sh -c in the working directory. 120s timeout — the whole process group is killed on timeout. Avoid piping output through `tail`/`head`: it buffers everything, so nothing is streamed to the UI until the command ends (and a timeout looks like a hang). For long-running commands (>2 minutes), use `bash_in_background` instead and redirect its output to a file. Returns stdout, stderr and the exit code.",
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
                "name": TOOL_BASH_IN_BACKGROUND,
                "description": "Run a long-running shell command in the background via sh -c and return immediately with a process id; then use the same tool with action=status or action=kill to check on or stop it. Prefer this over `bash` for commands that run longer than ~2 minutes. stdout/stderr are discarded, so redirect them to files in the command (e.g. `make build > build.log 2>&1`); do NOT append `&` — the tool already backgrounds the command. `run` returns the process id (or an error if the process fails to launch), `status` returns whether the process is still running, and `kill` returns an acknowledgement that the kill signal was sent.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["run", "kill", "status"], "description": "Which operation to perform: run a command, kill a process, or query a process's status." },
                        "command": { "type": "string", "description": "The shell command to run in the background (required for action=run)." },
                        "process_id": { "type": "string", "description": "The process id returned by a previous action=run (required for action=status and action=kill)." }
                    },
                    "required": ["action"],
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
                "description": "Request the user (in the UI) to switch this session's mode. Use this when the task needs a mode you do not have — e.g. you are in plan/explore mode and need to modify the codebase, or you are in build mode and only need to plan/explore. In plan mode, call this once the plan is ready and has no open questions the user must answer; if the plan has must-answer questions, list them and wait for the user's answers instead. The user approves or rejects the request in the UI; on approval the session switches mode and continues the run, so you can then do the work. Root sessions only: subagents must ask their parent agent instead. Write `message` in the user's language (the language the user writes in).",
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
        json!({
            "type": "function",
            "function": {
                "name": TOOL_ASK_USER,
                "description": "Ask the user (in the UI) a clarification question when you need more input from them — a choice between approaches, a preference, or a detail only the user can decide. The question is shown with every option plus a free-text input box; the user answers by picking an option (the answer is that option's title) or typing their own text. Call this tool once per question (one question per call; call it again for further questions after the user answered). After calling it, stop working and finish your turn: the answer arrives as a user message carrying a JSON object keyed by question_id (e.g. {\"q1\": \"Rust\"}), then continue. Root sessions only: subagents must ask their parent agent instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question_title": { "type": "string", "description": "The precise, concise headline of the question, shown to the user." },
                        "question_text": { "type": "string", "description": "A longer explanation of the question for the user (what you need and why)." },
                        "options": {
                            "type": "array",
                            "description": "Preset choices for the user. Each option has a precise, concise `option_title` (what comes back as the answer when chosen) and an `option_text` that further explains the option. Pass an empty array for a free-text-only question.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "option_title": { "type": "string", "description": "The precise, concise label the user picks (also the answer value)." },
                                    "option_text": { "type": "string", "description": "A longer explanation of this option for the user." }
                                },
                                "required": ["option_title", "option_text"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["question_title", "question_text", "options"],
                    "additionalProperties": false
                }
            }
        }),
    ];
    // Drop the schemas of disabled tools: the model only ever sees the
    // tools the session enabled (an empty enabled list = no restriction).
    all.into_iter()
        .filter(|def| {
            let name = def["function"]["name"].as_str().unwrap_or_default();
            mo_core::tools::is_enabled(name, enabled)
        })
        .collect()
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
struct BashInBackgroundArgs {
    action: String,
    command: Option<String>,
    process_id: Option<String>,
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
    // Defense in depth: the session's disabled tools are not advertised to
    // the model (their schemas are filtered out of the prompt), but a model
    // may still hallucinate a muted tool name. Refuse it explicitly so the
    // call is never dispatched and the model gets a clear explanation. An
    // empty enabled list (legacy sessions) means "no restriction".
    if !mo_core::tools::is_enabled(name, &ctx.session.tools) {
        return Err(format!(
            "tool {name} is disabled in this session (the session was created with it turned off)"
        ));
    }
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
        TOOL_BASH_IN_BACKGROUND => {
            let args: BashInBackgroundArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            bash_in_background::execute(
                &ctx.workdir,
                &args.action,
                args.command.as_deref(),
                args.process_id.as_deref(),
            )
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
        TOOL_ASK_USER => {
            let args: ask_user::AskUserArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            ask_user::ask_user(ctx, &args, on_event)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// Unit tests live in `mo_worker/src/tests/tools/mod_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/mod_tests.rs"]
mod tests;
