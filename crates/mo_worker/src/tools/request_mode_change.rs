//! `request_mode_change` tool: ask the user (in the UI) to switch this
//! session's mode.
//!
//! The write sandbox is tied to the session's mode (Build = codebase
//! writable; Plan/Explore = codebase read-only), and the journaled system
//! prompt keeps the framing of the mode the session ran under. When the
//! model needs a mode it does not have (e.g. it is in Plan mode and the user
//! asks it to build), it calls this tool instead of working around the
//! sandbox. The tool journals a `ModeChangeRequest` event; the frontend
//! renders it as an approval prompt (Agree / Reject) and freezes the
//! composer. Approving (`POST /api/sessions/:id/mode/approve`) switches the
//! session's mode and continues the run with a single `ModeChange` notice;
//! rejecting journals a `ModeChangeRequestDeclined` marker and switches
//! nothing.

use mo_core::{JournalEventKind, Mode, last_mode_marker};

use crate::tools::ToolContext;

/// Execute a `request_mode_change` tool call.
///
/// Validates the requested mode (must parse, differ from the session's
/// current mode, and come from a root session), refuses when a request is
/// already pending, then journals the request through `on_event` and returns
/// guidance telling the model to stop working and wait for the user.
pub fn request_mode_change(
    ctx: &ToolContext,
    args: &RequestModeChangeArgs,
    on_event: &dyn Fn(JournalEventKind),
) -> Result<String, String> {
    let mode = args
        .mode
        .parse::<Mode>()
        .map_err(|e| format!("invalid arguments for request_mode_change: {e}"))?;
    if mode == ctx.session.mode {
        return Err(format!(
            "the session is already in {mode} mode — no mode change needed"
        ));
    }
    if ctx.session.parent_id.is_some() {
        return Err(
            "subagents cannot request a mode change: the request is shown to the user in the \
             UI, which only root sessions have. Report the need to your parent agent instead."
                .to_string(),
        );
    }
    if args.message.trim().is_empty() {
        return Err(
            "invalid arguments for request_mode_change: message must not be empty".to_string(),
        );
    }
    // Refuse when a request is already pending: the user has not answered
    // yet, and a second request would just confuse the UI. The journal's
    // last mode marker decides: a `ModeChangeRequest` with no resolving
    // `ModeChange` / `ModeChangeRequestDeclined` after it is still pending.
    let events = mo_core::read_events(std::path::Path::new(&ctx.session.journal_path))
        .map_err(|e| format!("failed to read session journal: {e}"))?;
    if matches!(
        last_mode_marker(&events),
        Some(mo_core::ModeMarker::RequestPending { .. })
    ) {
        return Err(
            "a mode change request is already pending — the user has not answered yet. Do not \
             call this tool again; finish your turn and wait for the user's decision."
                .to_string(),
        );
    }

    on_event(JournalEventKind::ModeChangeRequest {
        mode,
        message: args.message.clone(),
    });
    Ok(format!(
        "Mode change request sent to the user: the session will switch to {mode} mode once \
         they approve it in the UI.\n\n\
         Stop working now: do not modify the codebase or spawn further subagents. Finish your \
         turn with a brief message to the user (in their language) telling them the request \
         was sent and what you will do once approved."
    ))
}

/// Arguments of the `request_mode_change` tool call.
#[derive(serde::Deserialize)]
pub struct RequestModeChangeArgs {
    /// The mode to switch to (`build` | `plan` | `explore`).
    pub mode: String,
    /// A short message for the user explaining why the switch is needed,
    /// written in the user's language.
    pub message: String,
}
