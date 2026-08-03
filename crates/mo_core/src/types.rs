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

/// A session row. Timestamps are RFC3339 strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub parent_id: Option<String>,
    pub workdir: String,
    pub prompt: String,
    pub model: String,
    pub status: SessionStatus,
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
