//! The session-mode registry: the set of modes the harness ships with, and
//! the per-mode metadata both the gateway (`GET /api/modes`) and the worker
//! (system-prompt framing, write sandbox) share.
//!
//! All modes expose the same tool set; what differs is the system prompt
//! (journaled at the first run) and the *write sandbox*: `Build` may modify
//! the codebase, while `Plan` and `Explore` treat it as read-only and may
//! only create/edit/remove files inside the session scratch dir.

use serde::Serialize;

use crate::types::Mode;

/// Static metadata for one mode, served to the UI (`GET /api/modes`) so the
/// pickers (new-session form, status-bar switcher) can render without
/// duplicating the registry.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ModeInfo {
    /// Mode id, as used in `POST /api/sessions` and `POST .../mode`
    /// (`"build" | "plan" | "explore"`).
    pub name: &'static str,
    /// Human-readable label shown in the UI.
    pub label: &'static str,
    /// Short help text for the pickers.
    pub description: &'static str,
    /// The tool names available in this mode (all modes share the set
    /// today; kept per-mode so future modes can restrict it).
    pub tools: &'static [&'static str],
    /// Where file mutations are allowed: `"codebase"` or `"scratch only"`.
    pub writable: &'static str,
}

pub const TOOL_NAMES: &[&str] = &[
    "read_file",
    "edit_file",
    "create_file",
    "remove_file",
    "bash",
    "spawn_subagent",
    "load_skill",
];

/// The built-in modes, in display order. `build` is the default for new
/// sessions (and the fallback for rows migrated from before modes existed).
pub const MODES: [ModeInfo; 3] = [
    ModeInfo {
        name: "build",
        label: "Build",
        description: "Execute tasks: modify the codebase, run commands, use subagents and skills.",
        tools: TOOL_NAMES,
        writable: "codebase",
    },
    ModeInfo {
        name: "plan",
        label: "Plan",
        description: "Make a plan before executing: the codebase is read-only; write drafts to the session scratch dir.",
        tools: TOOL_NAMES,
        writable: "scratch only",
    },
    ModeInfo {
        name: "explore",
        label: "Explore",
        description: "Understand the codebase and answer questions: the codebase is read-only; write notes to the session scratch dir.",
        tools: TOOL_NAMES,
        writable: "scratch only",
    },
];

impl Mode {
    /// Static metadata for this mode (from the `MODES` registry).
    pub fn info(self) -> &'static ModeInfo {
        MODES
            .iter()
            .find(|m| m.name == self.as_str())
            .expect("every Mode has a registry entry")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_a_registry_entry() {
        for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
            let info = mode.info();
            assert_eq!(info.name, mode.as_str());
            assert!(!info.description.is_empty());
            assert!(!info.tools.is_empty());
            assert_eq!(info.tools.len(), TOOL_NAMES.len());
        }
    }

    #[test]
    fn build_is_writable_and_plan_explore_are_scratch_only() {
        assert_eq!(Mode::Build.info().writable, "codebase");
        assert_eq!(Mode::Plan.info().writable, "scratch only");
        assert_eq!(Mode::Explore.info().writable, "scratch only");
    }

    #[test]
    fn mode_round_trips() {
        for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
            assert_eq!(mode.as_str().parse::<Mode>().unwrap(), mode);
            assert_eq!(mode.to_string(), mode.as_str());
        }
        assert!("nope".parse::<Mode>().is_err());
        assert!(serde_json::from_str::<Mode>("\"build\"").is_ok());
        assert_eq!(serde_json::to_string(&Mode::Plan).unwrap(), "\"plan\"");
    }
}
