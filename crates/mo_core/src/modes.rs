//! The session-mode registry: the set of modes the harness ships with, and
//! the per-mode metadata both the gateway (`GET /api/modes`) and the worker
//! (system-prompt framing, write sandbox) share.
//!
//! All modes expose the same tool set; what differs is the system prompt
//! (journaled at the first run) and the *write sandbox*: `Build` may modify
//! the codebase, while `Plan` and `Explore` treat it as read-only and may
//! only create/edit/remove files inside the session scratch dir.

use std::path::Path;

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
    "bash_in_background",
    "spawn_subagent",
    "load_skill",
    "request_mode_change",
    "ask_user",
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

/// The text of the mode-change message the gateway injects into the journal
/// (and hence the model context) immediately before a followup user message,
/// when the session's mode differs from the mode of the last run.
///
/// The system prompt is journaled once at the first run and reused verbatim,
/// so after a mode switch the model would otherwise keep the old mode's
/// framing. This message mirrors the system-prompt framing for the new mode:
/// it states the mode, its restrictions (the write sandbox) and its goal,
/// bracketed with a `[Session mode changed to …]` prefix so the notice is
/// unmistakable in the context. `scratch` is the session scratch dir
/// (`<data_dir>/sessions/<id>/tmp`) that non-Build modes may write to —
/// mentioned in the text, exactly as in the system prompt, so the model is
/// told where it *may* write.
pub fn mode_change_message(mode: Mode, scratch: &Path) -> String {
    match mode {
        Mode::Build => "[Session mode changed to build]\n\n\
             You are now in Build mode: you can modify the codebase (create/edit/remove \
             files), run commands, and use subagents and skills to get the job done.\n"
            .to_string(),
        Mode::Plan => format!(
            "[Session mode changed to plan]\n\n\
             You are now in Plan mode. Your job is to produce a clear, actionable \
             implementation plan — do not implement anything yet.\n\
             The codebase is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             bash and bash_in_background are available but treat them as read-only (a soft \
             restriction): use them to gather facts (builds, tests, greps), not to change \
             anything.\n\
             Finish with the plan as your final answer. Once the plan is ready, if it has \
             no open questions the user must answer before implementation, call the \
             `request_mode_change` tool with mode \"build\" to switch to build mode; if it \
             has must-answer questions, list them and wait for the user's answers instead.\n",
            scratch.display()
        ),
        Mode::Explore => format!(
            "[Session mode changed to explore]\n\n\
             You are now in Explore mode. Investigate the codebase to answer the user's \
             question or gather facts for a parent agent.\n\
             The codebase is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             Prefer read_file; run read-only bash or bash_in_background commands when \
             helpful.\n\
             Report concise findings as your final answer.\n",
            scratch.display()
        ),
    }
}

/// The content of the single `ModeChange` notice the gateway journals when
/// the user approves a pending `mode_change_request`: the standard
/// mode-change text plus an approval sentence, so the model knows the user
/// approved its request and that it should continue the task in the new
/// mode (this is the "single mode change message" sent to the LLM on
/// approval — the worker maps it to a user-role message and the run
/// continues).
pub fn mode_change_approved_message(mode: Mode, scratch: &Path) -> String {
    format!(
        "{}\nThe user approved your request to switch modes. Continue with the task — pick \
         up where you left off.\n",
        mode_change_message(mode, scratch)
    )
}

/// The journal's *mode marker* — the last event that pins down the session's
/// mode-change state, as scanned by the worker's `request_mode_change` tool
/// and the gateway's approve/reject endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeMarker {
    /// A `mode_change_request` with no resolving marker after it: the
    /// request is still pending (the user has not approved or rejected it).
    RequestPending { mode: Mode },
    /// A `ModeChange` notice: the pending request (if any) was approved and
    /// the session switched to `mode`; no request is pending.
    Approved { mode: Mode },
    /// A `ModeChangeRequestDeclined` event: the pending request was rejected;
    /// no request is pending and the mode did not change.
    Declined { mode: Mode },
}

/// Scan journal events (oldest first) for the last mode marker, if any.
///
/// The marker set is exactly the three events above; anything else
/// (`SystemPrompt`, messages, tool events, ...) does not resolve a request
/// and is skipped. Used verbatim by:
///
/// - the worker's `request_mode_change` tool (refuse when a request is
///   already pending),
/// - the gateway's approve endpoint (409 unless the last marker is a pending
///   request; the pending request's mode is what the session switches to),
/// - the gateway's reject endpoint (journal `ModeChangeRequestDeclined`
///   unless there is nothing to reject).
pub fn last_mode_marker(events: &[crate::types::JournalEvent]) -> Option<ModeMarker> {
    events.iter().rev().find_map(|e| match &e.kind {
        crate::types::JournalEventKind::ModeChangeRequest { mode, .. } => {
            Some(ModeMarker::RequestPending { mode: *mode })
        }
        crate::types::JournalEventKind::ModeChange { mode, .. } => {
            Some(ModeMarker::Approved { mode: *mode })
        }
        crate::types::JournalEventKind::ModeChangeRequestDeclined { mode } => {
            Some(ModeMarker::Declined { mode: *mode })
        }
        _ => None,
    })
}

// Unit tests live in `mo_core/src/tests/modes_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/modes_tests.rs"]
mod tests;
