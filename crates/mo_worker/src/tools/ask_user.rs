//! `ask_user` tool: ask the user (in the UI) a clarification question.
//!
//! When the model needs more input from the user — a choice between
//! approaches, a preference, a detail only the user can decide — it calls
//! this tool with a single question (`question_title`, `question_text`,
//! `options`). Stage 1 supports exactly one question per call: the model
//! calls the tool again for further questions, and a second call is refused
//! while one is pending (the user has not answered yet).
//!
//! The tool journals an `AskUserRequest` event; the frontend renders it as
//! a question card (every option plus a free-text input box) and freezes
//! the composer while the request is pending. The user answers by picking an
//! option or typing free text; the gateway journals an `AskUserAnswered`
//! event and respawns the worker, and the worker's history rebuild maps it
//! to a user-role message carrying the answers as a JSON object keyed by
//! `question_id` — the tool's "return value" to the model.

use mo_core::{AskUserMarker, AskUserOption, AskUserQuestion, JournalEventKind};

use crate::tools::ToolContext;

/// Execute an `ask_user` tool call.
///
/// Validates the question (non-empty title/text, non-empty option titles),
/// refuses when a request is already pending or the session is a subagent,
/// then journals the request through `on_event` and returns guidance telling
/// the model to stop working and wait for the user's answer.
pub fn ask_user(
    ctx: &ToolContext,
    args: &AskUserArgs,
    on_event: &dyn Fn(JournalEventKind),
) -> Result<String, String> {
    let question = validate(args)?;
    if ctx.session.parent_id.is_some() {
        return Err(
            "subagents cannot ask the user: the question is shown in the UI, which only \
             root sessions have. Report the need to your parent agent instead."
                .to_string(),
        );
    }
    // Refuse when a request is already pending: the user has not answered
    // yet, and a second question would just confuse the UI. The journal's
    // last ask-user marker decides: an `AskUserRequest` with no resolving
    // `AskUserAnswered` after it is still pending. A pending file-access
    // `PermissionRequest` blocks too (one user-facing card at a time).
    // (Stage 1: one question per call — ask again only after the previous
    // one was answered.)
    let events = mo_core::read_events(std::path::Path::new(&ctx.session.journal_path))
        .map_err(|e| format!("failed to read session journal: {e}"))?;
    if matches!(
        mo_core::last_ask_user_marker(&events),
        Some(AskUserMarker::RequestPending)
    ) {
        return Err(
            "a clarification question is already pending — the user has not answered yet. Do \
             not call this tool again; finish your turn and wait for the user's answer."
                .to_string(),
        );
    }
    if matches!(
        mo_core::last_permission_marker(&events),
        Some(mo_core::PermissionMarker::RequestPending)
    ) {
        return Err(
            "a file-access permission request is already pending — the user has not answered \
             yet. Do not call this tool; finish your turn and wait for the user's decision."
                .to_string(),
        );
    }

    let title = question.question_title.clone();
    on_event(JournalEventKind::AskUserRequest { question });
    Ok(format!(
        "Clarification question sent to the user: they will see \"{}\" in the UI with the \
         listed options and a free-text input box, and can answer by picking an option or \
         typing their own text.\n\n\
         Stop working now: do not continue the task or spawn further subagents until you \
         receive the answer. Finish your turn with a brief message to the user (in their \
         language) telling them the question was sent. The answer will arrive as a user \
         message carrying a JSON object keyed by question_id, e.g. {{\"q1\": \"the chosen \
         option title or the user's typed text\"}} — then continue from where you left off.",
        title
    ))
}

/// Arguments of the `ask_user` tool call. Stage 1: exactly one question per
/// call — the model calls the tool again for further questions.
#[derive(serde::Deserialize)]
pub struct AskUserArgs {
    /// The precise, concise headline of the question (shown to the user).
    pub question_title: String,
    /// A longer explanation of the question for the user.
    pub question_text: String,
    /// Preset choices: each has a precise, concise `option_title` (what
    /// comes back as the answer when chosen) and an `option_text` that
    /// further explains the option. May be empty for a free-text-only
    /// question.
    #[serde(default)]
    pub options: Vec<AskUserOptionArgs>,
}

/// One option of an `ask_user` call.
#[derive(serde::Deserialize)]
pub struct AskUserOptionArgs {
    /// The precise, concise label the user picks (also the answer value).
    pub option_title: String,
    /// A longer explanation of this option for the user.
    pub option_text: String,
}

/// Validate the arguments and build the journaled question with the
/// worker-assigned `question_id` (`q1` — stage 1 has one question per call,
/// so the id is deterministic and keys the answer in the answers object).
fn validate(args: &AskUserArgs) -> Result<AskUserQuestion, String> {
    let title = args.question_title.trim();
    let text = args.question_text.trim();
    if title.is_empty() {
        return Err("invalid arguments for ask_user: question_title must not be empty".to_string());
    }
    if text.is_empty() {
        return Err("invalid arguments for ask_user: question_text must not be empty".to_string());
    }
    let mut options = Vec::with_capacity(args.options.len());
    for (i, opt) in args.options.iter().enumerate() {
        if opt.option_title.trim().is_empty() {
            return Err(format!(
                "invalid arguments for ask_user: options[{i}].option_title must not be empty"
            ));
        }
        options.push(AskUserOption {
            option_title: opt.option_title.trim().to_string(),
            option_text: opt.option_text.trim().to_string(),
        });
    }
    Ok(AskUserQuestion {
        question_id: "q1".to_string(),
        question_title: title.to_string(),
        question_text: text.to_string(),
        options,
    })
}

// Unit tests live in `mo_worker/src/tests/tools/ask_user_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/ask_user_tests.rs"]
mod tests;
