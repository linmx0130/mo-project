//! Tool registry: OpenAI nested tool definitions plus argument validation
//! and dispatch.

pub mod bash;
pub mod fs;
pub mod skill;
pub mod subagent;

use std::path::PathBuf;

use mo_core::{JournalEventKind, Session};
use serde::Deserialize;
use serde_json::{Value, json};

pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_EDIT_FILE: &str = "edit_file";
pub const TOOL_CREATE_FILE: &str = "create_file";
pub const TOOL_REMOVE_FILE: &str = "remove_file";
pub const TOOL_BASH: &str = "bash";
pub const TOOL_SPAWN_SUBAGENT: &str = "spawn_subagent";
pub const TOOL_LOAD_SKILL: &str = "load_skill";

/// Everything a tool needs to run: the sandboxed workdir, the shared data
/// dir (for subagent sessions), the global agents dir (passed down so
/// subagents inject the same global instructions/skills), this worker's
/// session row, and model config.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workdir: PathBuf,
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub session: Session,
    pub subagent_depth: u32,
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
                "description": "Spawn a subagent (a nested agent session with the same working directory) to work on a self-contained subtask, and wait for its final answer. Depth is capped at 3.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "Self-contained instructions for the subagent." }
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
}

#[derive(Deserialize)]
struct LoadSkillArgs {
    name: String,
}

/// Execute one tool call. Returns the tool output (Ok) or a tool error
/// (Err) — both are surfaced to the model; only a failed dispatch returns
/// Err and both are journaled as ToolResult events.
///
/// `tool_call_id` identifies the call (used for streamed output events) and
/// `on_delta` receives `ToolOutputDelta` events while a streaming tool
/// (bash) runs, so the frontend can render output as it is produced.
pub async fn execute_tool(
    ctx: &ToolContext,
    name: &str,
    arguments: &str,
    tool_call_id: &str,
    on_delta: &mut (dyn FnMut(JournalEventKind) + Send),
) -> Result<String, String> {
    match name {
        TOOL_READ_FILE => {
            let args: ReadFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            // `read_file` may also read global skill folders, so pass the
            // discovered skill folder paths as extra allowed roots.
            let skill_roots: Vec<PathBuf> = crate::skills::discover_skills(&ctx.agents_dir)
                .into_iter()
                .map(|s| s.path)
                .collect();
            fs::read_file(&ctx.workdir, &args.path, &skill_roots)
        }
        TOOL_EDIT_FILE => {
            let args: EditFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            fs::edit_file(
                &ctx.workdir,
                &args.path,
                &args.old_string,
                &args.new_string,
                args.replace_all,
            )
        }
        TOOL_CREATE_FILE => {
            let args: CreateFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            fs::create_file(&ctx.workdir, &args.path, &args.content)
        }
        TOOL_REMOVE_FILE => {
            let args: RemoveFileArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            fs::remove_file(&ctx.workdir, &args.path)
        }
        TOOL_BASH => {
            let args: BashArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            bash::bash(
                &ctx.workdir,
                &args.command,
                bash::DEFAULT_TIMEOUT,
                tool_call_id,
                on_delta,
            )
            .await
        }
        TOOL_SPAWN_SUBAGENT => {
            let args: SpawnSubagentArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            subagent::spawn_subagent(ctx, &args.prompt).await
        }
        TOOL_LOAD_SKILL => {
            let args: LoadSkillArgs = serde_json::from_str(arguments)
                .map_err(|e| format!("invalid arguments for {name}: {e}"))?;
            skill::load_skill(&ctx.agents_dir, &args.name)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(agents_dir: PathBuf) -> ToolContext {
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
                pid: None,
                journal_path: "/tmp/j.jsonl".into(),
                created_at: "now".into(),
                updated_at: "now".into(),
                heartbeat_at: None,
                error: None,
            },
            subagent_depth: 0,
            model_base_url: "http://localhost:1".into(),
            model_name: "m".into(),
            auth_token: None,
        }
    }

    #[test]
    fn definitions_cover_all_tools() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 7);
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
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let ctx = test_ctx(PathBuf::from("/tmp/agents"));
        let mut no_delta = |_: JournalEventKind| {};
        let err = execute_tool(&ctx, "nope", "{}", "call_x", &mut no_delta)
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
        let mut no_delta = |_: JournalEventKind| {};
        let out = execute_tool(
            &ctx,
            TOOL_LOAD_SKILL,
            r#"{"name":"greeter"}"#,
            "call_s",
            &mut no_delta,
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
            &mut no_delta,
        )
        .await
        .unwrap_err();
        assert!(err.contains("skill not found"), "got: {err}");
    }
}
