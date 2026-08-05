//! Shared domain types: sessions and journal events.

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
    SystemPrompt {
        content: String,
        #[serde(default)]
        mode: Mode,
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
}

/// One line of a session journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEvent {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: JournalEventKind,
}
