//! File-access permission policy for the filesystem tools.
//!
//! The fs functions sandbox every path to the session workdir (plus, for
//! reads, extra roots such as skill folders and the session scratch dir).
//! This module decides what happens when a file tool call targets a path
//! *outside* those roots:
//!
//! - **reads** (any mode) and **writes in `build` mode**: the worker
//!   journals a `PermissionRequest` event and tells the model to stop and
//!   wait; the user sees an Allow / Deny card in the UI and the decision
//!   arrives back as a user message (`PermissionAnswered`). The decision is
//!   remembered per `(tool, path)`, so a retry of an allowed path runs
//!   without prompting again and a retry of a denied path is refused
//!   outright.
//! - **writes in `plan`/`explore` mode**: denied outright, never asked
//!   about (only the session scratch dir is writable for them).
//! - **subagents**: never ask (their journal has no UI) — a plain error
//!   telling them to report the need to their parent agent.

use std::path::PathBuf;

use mo_core::{AskUserMarker, JournalEventKind, Mode, PermissionMarker};

use crate::tools::ToolContext;
use crate::tools::fs::{self, PathClass};

/// What to do with a file-tool call once its path has been classified.
#[derive(Debug)]
pub enum PathPolicy {
    /// Run the fs operation with this exact resolved path in the
    /// `approved` list: it is inside an allowed root (the workdir, the
    /// scratch dir, or — for reads — a skill folder), or the user approved
    /// it earlier. The containment check passes for that one path only.
    Run(PathBuf),
    /// The path is outside the allowed roots and the mode permits asking:
    /// journal a permission request and tell the model to stop and wait.
    Ask { operation: &'static str },
}

/// Classify a `read_file` call. `roots` are the extra read roots (skill
/// folders + the session scratch dir) — the same set the fs call uses.
pub fn read_policy(ctx: &ToolContext, raw: &str, roots: &[PathBuf]) -> Result<PathPolicy, String> {
    match fs::classify_read(&ctx.workdir, raw, roots)? {
        PathClass::Allowed(resolved) => Ok(PathPolicy::Run(resolved)),
        PathClass::Outside(resolved) => {
            outside_policy(ctx, crate::tools::TOOL_READ_FILE, "read", raw, resolved)
        }
    }
}

/// Classify a write call (`edit_file` / `create_file` / `remove_file`).
/// `tool` is the tool name (the permission memory is keyed on it).
pub fn write_policy(ctx: &ToolContext, tool: &str, raw: &str) -> Result<PathPolicy, String> {
    match fs::classify_write(&ctx.workdir, raw)? {
        // Inside the codebase: writable in build mode; the codebase is
        // read-only in plan/explore (denied with a mode-aware message, as
        // before — never a permission request).
        PathClass::Allowed(resolved) => {
            if ctx.session.mode == Mode::Build {
                Ok(PathPolicy::Run(resolved))
            } else {
                Err(format!(
                    "{} mode: the codebase is read-only; create/edit/remove is only allowed under {} (use an absolute path)",
                    ctx.session.mode.as_str(),
                    ctx.scratch.display()
                ))
            }
        }
        // Outside the codebase: the session scratch dir is writable in
        // every mode (reads already carry it as an extra root). Everything
        // else asks the user in build mode and is denied outright in
        // plan/explore.
        PathClass::Outside(resolved) => {
            if let PathClass::Allowed(scratch_resolved) = fs::classify_write(&ctx.scratch, raw)? {
                return Ok(PathPolicy::Run(scratch_resolved));
            }
            if ctx.session.mode == Mode::Build {
                outside_policy(ctx, tool, "write", raw, resolved)
            } else {
                Err(format!(
                    "{} mode: writes are denied outside the session scratch dir ({})",
                    ctx.session.mode.as_str(),
                    ctx.scratch.display()
                ))
            }
        }
    }
}

/// The path is outside every allowed root: remember the user's earlier
/// decision for this exact `(tool, path)` (an allowed path runs without
/// prompting again; a denied one is refused), otherwise signal `Ask`.
fn outside_policy(
    ctx: &ToolContext,
    tool: &str,
    operation: &'static str,
    raw: &str,
    resolved: PathBuf,
) -> Result<PathPolicy, String> {
    if let Some(allowed) = remembered_decision(ctx, tool, raw) {
        return if allowed {
            Ok(PathPolicy::Run(resolved))
        } else {
            Err(format!(
                "the user denied this request earlier: {tool} {raw} — do not retry it; find another way or explain why you cannot proceed"
            ))
        };
    }
    Ok(PathPolicy::Ask { operation })
}

/// The user's most recent decision for this exact `(tool, raw path)`, from
/// the session journal — `Some(true)` = allowed, `Some(false)` = denied,
/// `None` = never asked about before. Only consulted for outside paths, so
/// the common (sandboxed) case never pays for the journal read.
fn remembered_decision(ctx: &ToolContext, tool: &str, raw: &str) -> Option<bool> {
    let events = mo_core::read_events(std::path::Path::new(&ctx.session.journal_path)).ok()?;
    events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::PermissionAnswered {
            tool: t,
            path: p,
            allowed,
            ..
        } if t == tool && p == raw => Some(*allowed),
        _ => None,
    })
}

/// Journal a `PermissionRequest` through `on_event` and return guidance
/// telling the model to stop working and wait for the user's decision (the
/// `ask_user` pattern). Refuses when the session is a subagent (their
/// journal has no UI) or when another user-facing request (a clarification
/// question or a permission request) is already pending.
pub fn ask_permission(
    ctx: &ToolContext,
    tool: &str,
    operation: &str,
    raw: &str,
    on_event: &dyn Fn(JournalEventKind),
) -> Result<String, String> {
    if ctx.session.parent_id.is_some() {
        return Err(
            "this path is outside the allowed roots and subagents cannot ask the user for \
             permission (the request is shown in the UI, which only root sessions have). \
             Report the need to your parent agent instead."
                .to_string(),
        );
    }
    let events = mo_core::read_events(std::path::Path::new(&ctx.session.journal_path))
        .map_err(|e| format!("failed to read session journal: {e}"))?;
    // Refuse while another request is pending: the user has not answered
    // yet, and a second card would just confuse the UI. (Stage 1: one
    // pending request of any kind at a time.)
    if matches!(
        mo_core::last_ask_user_marker(&events),
        Some(AskUserMarker::RequestPending)
    ) {
        return Err(
            "a clarification question is already pending — the user has not answered yet. Do \
             not request file access; finish your turn and wait for the user's answer."
                .to_string(),
        );
    }
    if matches!(
        mo_core::last_permission_marker(&events),
        Some(PermissionMarker::RequestPending)
    ) {
        return Err(
            "a file-access permission request is already pending — the user has not answered \
             yet. Do not request more access; finish your turn and wait for the user's decision."
                .to_string(),
        );
    }

    on_event(JournalEventKind::PermissionRequest {
        request_id: "p1".to_string(),
        tool: tool.to_string(),
        operation: operation.to_string(),
        path: raw.to_string(),
    });
    Ok(format!(
        "A permission request was sent to the user: they will see a request to {operation} \
         \"{raw}\" via the {tool} tool in the UI and can allow or deny it.\n\n\
         Stop working now: do not continue the task or retry the tool call until you receive \
         the answer. Finish your turn with a brief message to the user (in their language) \
         telling them the request was sent. The answer will arrive as a user message: if the \
         user allowed the request, retry the tool call with the same arguments; if they \
         denied it, do not retry — find another way or explain why you cannot proceed."
    ))
}

// Unit tests live in `mo_worker/src/tests/tools/permission_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/permission_tests.rs"]
mod tests;
