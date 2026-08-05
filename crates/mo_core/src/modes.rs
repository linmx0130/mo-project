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
    "spawn_subagent",
    "load_skill",
    "request_mode_change",
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
             bash is available but treat it as read-only (a soft restriction): use it to \
             gather facts (builds, tests, greps), not to change anything.\n\
             Finish with the plan as your final answer.\n",
            scratch.display()
        ),
        Mode::Explore => format!(
            "[Session mode changed to explore]\n\n\
             You are now in Explore mode. Investigate the codebase to answer the user's \
             question or gather facts for a parent agent.\n\
             The codebase is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             Prefer read_file; run read-only bash commands when helpful.\n\
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

    #[test]
    fn mode_change_message_states_mode_restriction_and_goal() {
        let scratch = Path::new("/data/sessions/s1/tmp");
        let build = mode_change_message(Mode::Build, scratch);
        assert!(build.starts_with("[Session mode changed to build]"));
        assert!(build.contains("Build mode"));
        assert!(build.contains("modify the codebase"));
        // Build has no scratch dir to write to; the message must not send
        // the model looking for one.
        assert!(!build.contains(scratch.to_str().unwrap()));

        let plan = mode_change_message(Mode::Plan, scratch);
        assert!(plan.starts_with("[Session mode changed to plan]"));
        assert!(plan.contains("Plan mode"));
        assert!(plan.contains("implementation plan"));
        assert!(plan.contains("READ-ONLY"));
        assert!(plan.contains(scratch.to_str().unwrap()));
        assert!(plan.contains("absolute paths"));

        let explore = mode_change_message(Mode::Explore, scratch);
        assert!(explore.starts_with("[Session mode changed to explore]"));
        assert!(explore.contains("Explore mode"));
        assert!(explore.contains("READ-ONLY"));
        assert!(explore.contains("Prefer read_file"));
        assert!(explore.contains(scratch.to_str().unwrap()));
    }

    #[test]
    fn approved_message_adds_approval_sentence() {
        let scratch = Path::new("/data/sessions/s1/tmp");
        let approved = mode_change_approved_message(Mode::Build, scratch);
        assert!(approved.starts_with("[Session mode changed to build]"));
        assert!(approved.contains("You are now in Build mode"));
        assert!(approved.contains("approved your request"));
        assert!(approved.contains("Continue with the task"));
    }

    #[test]
    fn tool_names_include_request_mode_change() {
        assert!(TOOL_NAMES.contains(&"request_mode_change"));
        for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
            assert!(mode.info().tools.contains(&"request_mode_change"));
        }
    }

    #[test]
    fn last_mode_marker_resolves_pending_requests() {
        use crate::types::{JournalEvent, JournalEventKind, JournalMessage};
        let mk = |kind: JournalEventKind| JournalEvent {
            seq: 0,
            ts: chrono::Utc::now(),
            kind,
        };
        let user = || {
            mk(JournalEventKind::Message(JournalMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
        };
        let request = || {
            mk(JournalEventKind::ModeChangeRequest {
                mode: Mode::Build,
                message: "may I switch?".to_string(),
            })
        };
        let approved = || {
            mk(JournalEventKind::ModeChange {
                mode: Mode::Build,
                content: "[Session mode changed to build]".to_string(),
            })
        };
        let declined = || mk(JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build });

        // No markers at all.
        assert_eq!(last_mode_marker(&[user()]), None);
        // A lone request is pending.
        assert_eq!(
            last_mode_marker(&[user(), request()]),
            Some(ModeMarker::RequestPending { mode: Mode::Build })
        );
        // A ModeChange after the request resolves it (approved).
        assert_eq!(
            last_mode_marker(&[user(), request(), approved()]),
            Some(ModeMarker::Approved { mode: Mode::Build })
        );
        // A declined marker resolves it too.
        assert_eq!(
            last_mode_marker(&[user(), request(), declined()]),
            Some(ModeMarker::Declined { mode: Mode::Build })
        );
        // An approved request followed by a *new* request is pending again.
        assert_eq!(
            last_mode_marker(&[user(), request(), approved(), request()]),
            Some(ModeMarker::RequestPending { mode: Mode::Build })
        );
    }
}
