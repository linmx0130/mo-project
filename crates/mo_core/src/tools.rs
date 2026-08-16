//! The session tool registry: static per-tool metadata for the UI (the
//! "New session" checkbox list, served via `GET /api/tools`) plus the
//! validation/resolution logic shared by the gateway (session creation)
//! and the worker (schema filtering + execution gate).
//!
//! Two groups of tools exist:
//!
//! * **Fixed** — `bash` (and its background variant) plus the file
//!   operation tools. They are *always* available: users cannot disable
//!   them, and their schemas are always injected into the model prompt.
//! * **Toggleable** — everything else (`spawn_subagent`, `load_skill`,
//!   `request_mode_change`, `ask_user`). A user may turn any of them off
//!   per session; a disabled tool's schema is not injected into the
//!   prompt, and the worker refuses to execute it.

use serde::Serialize;

/// Static metadata for one tool, served to the UI (`GET /api/tools`) so
/// the "New session" form can render the checkbox list without
/// duplicating the registry.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ToolInfo {
    /// Tool id, as used in the model-facing tool schemas.
    pub name: &'static str,
    /// Human-readable label shown in the UI.
    pub label: &'static str,
    /// Short help text for the checkbox list.
    pub description: &'static str,
    /// Always available: the tool cannot be disabled for a session (bash
    /// + file operations). The UI shows it pre-checked and disabled.
    pub fixed: bool,
}

/// Always-available tools: shell (`bash`, `bash_in_background`) and file
/// operations (`read_file`, `edit_file`, `create_file`, `remove_file`).
/// Their schemas are always injected into the prompt; users can never
/// disable them per session.
pub const FIXED_TOOLS: &[&str] = &[
    "read_file",
    "edit_file",
    "create_file",
    "remove_file",
    "bash",
    "bash_in_background",
];

/// Optional tools — everything else. Users may disable any of these per
/// session (the "New session" checkbox list); a disabled tool's schema
/// is not injected into the prompt.
pub const TOGGLEABLE_TOOLS: &[&str] = &[
    "spawn_subagent",
    "load_skill",
    "request_mode_change",
    "ask_user",
];

/// Every tool name, in display order (fixed first, then toggleable).
pub const TOOL_NAMES: &[&str] = &[
    "read_file",
    "edit_file",
    "create_file",
    "remove_file",
    "bash",
    "bash_in_background",
    "spawn_subagent",
    "load_skill",
    "request_mode_change",
    "ask_user",
];

/// The full registry, in display order. The first six tools are fixed
/// (always available); the last four are toggleable.
pub const TOOLS: &[ToolInfo] = &[
    ToolInfo {
        name: "read_file",
        label: "Read files",
        description: "Read a text file inside the working directory.",
        fixed: true,
    },
    ToolInfo {
        name: "edit_file",
        label: "Edit files",
        description: "Replace text inside an existing file.",
        fixed: true,
    },
    ToolInfo {
        name: "create_file",
        label: "Create files",
        description: "Create a new file with the given content.",
        fixed: true,
    },
    ToolInfo {
        name: "remove_file",
        label: "Remove files",
        description: "Permanently delete a regular file.",
        fixed: true,
    },
    ToolInfo {
        name: "bash",
        label: "Bash",
        description: "Run a shell command (builds, tests, git, ...).",
        fixed: true,
    },
    ToolInfo {
        name: "bash_in_background",
        label: "Background bash",
        description: "Run a long shell command in the background (status/kill).",
        fixed: true,
    },
    ToolInfo {
        name: "spawn_subagent",
        label: "Subagents",
        description: "Spawn a nested agent for a self-contained subtask.",
        fixed: false,
    },
    ToolInfo {
        name: "load_skill",
        label: "Skills",
        description: "Load a global skill's full instructions on demand.",
        fixed: false,
    },
    ToolInfo {
        name: "request_mode_change",
        label: "Mode-change requests",
        description: "Ask the user (in the UI) to switch this session's mode.",
        fixed: false,
    },
    ToolInfo {
        name: "ask_user",
        label: "Ask the user",
        description: "Ask a clarification question shown in the UI.",
        fixed: false,
    },
];

/// True for the always-available tools (`bash` + file operations).
pub fn is_fixed(name: &str) -> bool {
    FIXED_TOOLS.contains(&name)
}

/// True for the optional tools a user may disable per session.
pub fn is_toggleable(name: &str) -> bool {
    TOGGLEABLE_TOOLS.contains(&name)
}

/// Whether the tool `name` is enabled for a session given its
/// enabled-tool list (the `Session::tools` column). An *empty* list means
/// "no restriction" — the legacy default for sessions created before tool
/// selection existed — so every tool is enabled.
pub fn is_enabled(name: &str, enabled: &[String]) -> bool {
    enabled.is_empty() || enabled.iter().any(|t| t == name)
}

/// Resolve a session's canonical enabled-tool list from the client's
/// `banned_tools` — the *toggleable* tools the user turned off in the
/// "New session" form. Fixed tools are always included; every banned name
/// must be a known toggleable tool (banning a fixed tool or an unknown
/// name is a client error). The result is stored on the session row and
/// drives both the schema injection and the execution gate.
pub fn resolve_enabled_tools(banned: &[String]) -> Result<Vec<String>, String> {
    for name in banned {
        if is_fixed(name) {
            return Err(format!(
                "tool {name} is always available and cannot be disabled"
            ));
        }
        if !is_toggleable(name) {
            return Err(format!("unknown tool: {name}"));
        }
    }
    let mut enabled: Vec<String> = FIXED_TOOLS.iter().map(|s| (*s).to_string()).collect();
    for name in TOGGLEABLE_TOOLS {
        if !banned.iter().any(|b| b == name) {
            enabled.push((*name).to_string());
        }
    }
    Ok(enabled)
}

// Unit tests live in `mo_core/src/tests/tools_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/tools_tests.rs"]
mod tests;
