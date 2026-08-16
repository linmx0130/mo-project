//! Shared domain types: sessions and journal events.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle state of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Pending => "pending",
            SessionStatus::Running => "running",
            SessionStatus::Completed => "completed",
            SessionStatus::Failed => "failed",
            SessionStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
        )
    }

    /// Ordering used by the gateway SSE tail to decide whether a DB-only
    /// status change should be synthesized as a journal `StatusChange` event.
    /// Terminal states rank above running which ranks above pending.
    pub fn rank(&self) -> u8 {
        match self {
            SessionStatus::Pending => 0,
            SessionStatus::Running => 1,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled => 2,
        }
    }
}

impl FromStr for SessionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(SessionStatus::Pending),
            "running" => Ok(SessionStatus::Running),
            "completed" => Ok(SessionStatus::Completed),
            "failed" => Ok(SessionStatus::Failed),
            "cancelled" => Ok(SessionStatus::Cancelled),
            other => Err(format!("unknown session status: {other}")),
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The session mode: a different system prompt (journaled once at the first
/// run) and a different write sandbox. `Build` may modify the codebase;
/// `Plan` and `Explore` treat the codebase as read-only and may only
/// create/edit/remove files inside the session scratch dir. All modes share
/// the same tool set — the restriction is *where* writes land, not whether
/// the tools exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Build,
    Plan,
    Explore,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::Plan => "plan",
            Mode::Explore => "explore",
        }
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "build" => Ok(Mode::Build),
            "plan" => Ok(Mode::Plan),
            "explore" => Ok(Mode::Explore),
            other => Err(format!(
                "unknown mode: {other} (expected build, plan or explore)"
            )),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `build` is the default mode: for new sessions, and the fallback when
/// deserializing rows/journal lines written before modes were recorded.
impl Default for Mode {
    fn default() -> Self {
        Mode::Build
    }
}

/// A session row. Timestamps are RFC3339 strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub parent_id: Option<String>,
    pub workdir: String,
    pub prompt: String,
    pub model: String,
    pub status: SessionStatus,
    /// The session mode (see `Mode`): drives the system prompt journaled on
    /// the first run and the write-sandbox policy of every run. Mutable via
    /// `POST /api/sessions/:id/mode` — switching it changes only the write
    /// sandbox; the journaled system prompt never changes.
    pub mode: Mode,
    /// The tools enabled for this session (tool names whose schemas are
    /// injected into the model prompt). Chosen once at creation from the
    /// "New session" form's checkbox list; `bash` and the file-operation
    /// tools are always included (see `mo_core::tools`). An empty list
    /// means "all tools" — the legacy default for sessions created before
    /// tool selection existed.
    pub tools: Vec<String>,
    pub pid: Option<u32>,
    pub journal_path: String,
    pub created_at: String,
    pub updated_at: String,
    pub heartbeat_at: Option<String>,
    pub error: Option<String>,
}

/// A tool call as requested by the model (arguments is a JSON string).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A chat message mirrored into the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
}

/// One selectable option of a clarification question (`ask_user` tool).
/// `option_title` is the precise, concise label the user picks — and the
/// value that comes back as the answer when the option is chosen;
/// `option_text` further explains the option in the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserOption {
    pub option_title: String,
    pub option_text: String,
}

/// One clarification question (`ask_user` tool). Stage 1 supports exactly
/// one question per tool call — the model calls the tool again for further
/// questions, and a second call is refused while one is pending.
///
/// `question_id` is assigned by the worker (`q1` by default) and keys the
/// answer in the `AskUserAnswered` answers object. `question_title` is the
/// precise, concise headline; `question_text` further explains the question
/// to the user. `options` lists the preset choices (each with a concise
/// `option_title` and an explanatory `option_text`); it may be empty, in
/// which case the user answers with free text only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserQuestion {
    pub question_id: String,
    pub question_title: String,
    pub question_text: String,
    pub options: Vec<AskUserOption>,
}

/// The journal's *ask-user marker* — the last event that pins down the
/// clarification-request state, as scanned by the worker's `ask_user` tool
/// (refuse while a request is pending) and the gateway's answer endpoint
/// (409 unless a request is pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskUserMarker {
    /// An `ask_user_request` with no `ask_user_answered` after it: the
    /// question is still waiting for the user's answer.
    RequestPending,
    /// An `ask_user_answered` event: the pending request was answered; no
    /// request is pending.
    Answered,
}

/// Scan journal events (oldest first) for the last ask-user marker, if any.
/// Anything else (`ModeChangeRequest`, messages, tool events, ...) does not
/// resolve a request and is skipped.
pub fn last_ask_user_marker(events: &[JournalEvent]) -> Option<AskUserMarker> {
    events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::AskUserRequest { .. } => Some(AskUserMarker::RequestPending),
        JournalEventKind::AskUserAnswered { .. } => Some(AskUserMarker::Answered),
        _ => None,
    })
}

/// The journal's *model marker* — the last event that pins down which model
/// the conversation last ran under, as scanned by the gateway when a
/// followup arrives to decide whether to inject a `ModelChange` notice.
///
/// The marker set is `SystemPrompt` (its `model` field: the model of the
/// first run — or of the run that followed a context compression, which
/// journals a fresh prompt) and `ModelChange` (its `to` field: every run
/// after an injected notice happened under the new model). Anything else is
/// skipped, exactly like the mode-marker scan. A `SystemPrompt` whose
/// `model` is empty (journals written before the field existed) carries no
/// marker — an old session's very first switch is therefore not journaled
/// (the DB still records the new model and the next run uses it; only the
/// notice is skipped), and a session that never ran (no `SystemPrompt`, no
/// `ModelChange`) has no marker either.
pub fn last_model_marker(events: &[JournalEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::SystemPrompt { model, .. } if !model.is_empty() => Some(model.clone()),
        JournalEventKind::ModelChange { to, .. } => Some(to.clone()),
        _ => None,
    })
}

/// The payload of a journal line, serde-tagged on `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEventKind {
    Message(JournalMessage),
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        output: String,
    },
    StatusChange {
        status: SessionStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The session's system prompt, journaled by the worker on the first
    /// run (before the first LLM call) and reused verbatim on every later
    /// run — it is never rebuilt, so mid-session changes to `AGENTS.md`,
    /// skills or the mode never invalidate it (and the same system text is
    /// sent to the LLM on every run, which is prompt-cache friendly).
    /// Readers (the worker's history rebuild and the frontend timeline)
    /// treat it as session metadata, not as a chat message.
    ///
    /// `mode` is the mode the session ran under when this prompt was
    /// journaled (the mode of the first run). Together with `ModeChange`
    /// events it is the journal's *mode marker*: the gateway scans for the
    /// last one when a followup arrives to decide whether the session's
    /// mode changed since the last run. `#[serde(default)]` (→ `build`)
    /// keeps journals written before the field existed parseable.
    ///
    /// `model` is the model the session ran under when this prompt was
    /// journaled (the model of the first run, or of the run that followed a
    /// context compression, which journals a fresh prompt). Together with
    /// `ModelChange` events it is the journal's *model marker*: the gateway
    /// scans for the last one when a followup arrives to decide whether the
    /// session's model changed since the last run. `#[serde(default)]` (→
    /// empty) keeps journals written before the field existed parseable —
    /// an empty model is treated as "no recorded marker" by the scan.
    SystemPrompt {
        content: String,
        #[serde(default)]
        mode: Mode,
        #[serde(default)]
        model: String,
    },
    /// A notice that the session's mode changed since the last run. The
    /// gateway injects it into the journal *immediately before* a followup
    /// user message when the session's mode differs from the mode of the
    /// last run — and only then (at most one per followup; switching modes
    /// multiple times before a single followup collapses into one message
    /// describing the final mode, and switching back to the last-run mode
    /// injects nothing).
    ///
    /// `content` is the full message text (from
    /// `mo_core::modes::mode_change_message`), embedded at injection time so
    /// the worker passes it through verbatim and the frontend renders it
    /// without regenerating. `mode` is the *new* mode — because a
    /// `ModeChange` is only ever injected right before a user message, it
    /// doubles as a mode marker: every run after it happened under its mode.
    /// The worker maps the event to a user-role chat message, so the model
    /// sees it directly before the real user message.
    ModeChange {
        mode: Mode,
        content: String,
    },
    /// The worker's request, via the `request_mode_change` tool, to switch
    /// the session's mode. Journaled by the worker when the model calls the
    /// tool; the frontend renders it as an approval prompt (Agree / Reject)
    /// and freezes the composer while it is pending. Approving
    /// (`POST /api/sessions/:id/mode/approve`) switches the session's mode
    /// and journals a `ModeChange` notice to continue the run; rejecting
    /// (`POST /api/sessions/:id/mode/reject`) journals a
    /// `ModeChangeRequestDeclined` marker instead and switches nothing.
    ///
    /// `message` is the model's request text for the user, in the user's
    /// language — generated by the model, surfaced verbatim in the UI.
    /// Readers (the worker's history rebuild) skip this event: it is flow
    /// metadata for the UI, never a chat message.
    ModeChangeRequest {
        mode: Mode,
        message: String,
    },
    /// The user rejected a pending `ModeChangeRequest`. Journaled by the
    /// gateway (`POST /api/sessions/:id/mode/reject`) when the user clicks
    /// Reject in the UI; it resolves the request (no mode switch happens) so
    /// the request stops being pending. Like `ModeChangeRequest`, it never
    /// reaches the model context — it is flow metadata, not a chat message.
    ModeChangeRequestDeclined {
        mode: Mode,
    },
    /// A notice that the session's model changed since the last run. The
    /// gateway injects it into the journal *immediately before* a followup
    /// user message when the session's model differs from the model of the
    /// last run — and only then (at most one per followup; switching models
    /// multiple times before a single followup collapses into one event
    /// describing the final model, and switching back to the last-run model
    /// injects nothing).
    ///
    /// `from` is the model of the last run (the previous model marker: the
    /// journaled `SystemPrompt`'s model or a previously injected
    /// `ModelChange`); `to` is the session's current model. Because a
    /// `ModelChange` is only ever injected right before a run (a followup
    /// user message or the `mode/approve` continuation), it doubles as a
    /// model marker: every run after it happened under `to`.
    ///
    /// Unlike `ModeChange`, this event never reaches the model context —
    /// the journaled system prompt is model-agnostic and the next run is
    /// spawned with `to` regardless — so the worker's history rebuild skips
    /// it: it is flow metadata for the UI (a timeline notice row) and the
    /// audit trail, not a chat message.
    ModelChange {
        from: String,
        to: String,
    },
    /// The worker's clarification question, via the `ask_user` tool.
    /// Journaled by the worker when the model calls the tool; the frontend
    /// renders it as a question card (every option plus a free-text input
    /// box) and freezes the composer while it is pending. Stage 1 supports
    /// exactly one question per call — the model calls the tool again for
    /// further questions, and a second call is refused while one is pending.
    ///
    /// `question.question_id` is assigned by the worker (`q1`) and keys the
    /// answer in the `AskUserAnswered` answers object. Readers (the worker's
    /// history rebuild) skip this event: it is flow metadata for the UI,
    /// never a chat message.
    AskUserRequest {
        question: AskUserQuestion,
    },
    /// The user answered a pending `AskUserRequest`. Journaled by the
    /// gateway (`POST /api/sessions/:id/ask/answer`) when the user picks an
    /// option or types a free-form answer in the UI; it resolves the request
    /// (the worker respawns and the run continues) and carries the answers
    /// as a JSON object keyed by `question_id` — each value is the chosen
    /// option's `option_title` or the user's typed text.
    ///
    /// Like `ModeChange`, the worker's history rebuild maps this event to a
    /// user-role message, so the model receives the answers (the tool's
    /// "return value") as a user message on the resumed run.
    AskUserAnswered {
        answers: BTreeMap<String, String>,
    },
    /// A streamed chunk of an assistant message (token-by-token preview).
    ///
    /// The worker journals these as the LLM stream arrives, *before* the
    /// final `Message` event with the assembled text. A chunk may carry
    /// either the visible `content`, `reasoning_content` (reasoning models
    /// stream their thinking first, then the answer), or both. Readers (the
    /// frontend) append them to the in-flight assistant message and let the
    /// final `Message` event replace the assembled content — which also
    /// repairs the transient state when a retried LLM call leaves partial
    /// deltas behind. Delta events are skipped when rebuilding chat
    /// history, so they never reach the model context.
    MessageDelta {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// A streamed chunk of a running tool's output (bash stdout/stderr).
    ///
    /// `id` matches the tool call id of the preceding `ToolCallStart`
    /// event; readers append the chunks to that tool block until the final
    /// `ToolResult` event (with the complete, capped output) arrives.
    ToolOutputDelta {
        id: String,
        name: String,
        output: String,
    },
    /// The session's context length after an LLM call, reported by the API
    /// (`usage.prompt_tokens` — the tokens the model consumed for this call,
    /// i.e. system prompt + history + tool outputs). The worker journals one
    /// of these after every successful LLM call; readers show the latest as
    /// the session's current context usage in the status bar.
    ///
    /// `context_window` is the model's configured window in tokens at
    /// session time, or `None` for unlimited — embedded by the worker so the
    /// frontend can render the length against the window without extra
    /// lookups.
    ContextUsage {
        tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
    },
    /// The parent worker spawned a subagent session. Journaled by the
    /// `spawn_subagent` tool into the *parent's* journal, right after the
    /// child session row is created, so readers can link the parent's
    /// `spawn_subagent` tool block (identified by `tool_call_id`) to the
    /// child session (`child_id`) and render its messages in a modal.
    /// `mode` is the mode the child runs under.
    ///
    /// Like `ModeChangeRequest`, this is flow metadata for the UI, never a
    /// chat message: the worker's history rebuild and the mode-marker scan
    /// both skip it.
    SubagentStarted {
        child_id: String,
        tool_call_id: String,
        mode: Mode,
    },
    /// Context compression: the model's *handoff prompt*, generated when
    /// the session's context length (the API-reported `usage.prompt_tokens`)
    /// crossed the configured fraction of the model's context window
    /// (`mo.toml`'s `context_compression_threshold`, default 0.75). The
    /// worker asks the model to summarize the conversation so far into a
    /// handoff prompt (original user input, environment facts, key
    /// decisions + reasons, progress + todo, next step) and journals it as
    /// this event — immediately followed by a fresh `SystemPrompt` event
    /// rebuilt from `mode`, because the compressed context starts a new
    /// "session" that needs the current mode's system prompt.
    ///
    /// Readers (the worker's history rebuild) treat the handoff event's own
    /// `seq` as the *compression boundary*: every event with `seq` below it
    /// is dropped from the model context on future runs, and the handoff
    /// text itself becomes the first user message of the compressed
    /// context. The earlier events stay in the journal — and in the UI —
    /// so the full history remains inspectable.
    Handoff {
        content: String,
        /// The mode the session ran under when the handoff was generated;
        /// the fresh `SystemPrompt` event that follows is built from it.
        mode: Mode,
    },
}

/// One line of a session journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEvent {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: JournalEventKind,
}

// Unit tests live in `mo_core/src/tests/types_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/types_tests.rs"]
mod tests;
