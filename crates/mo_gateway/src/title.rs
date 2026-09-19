//! Session title generation: a short, separate gateway-side LLM call.
//!
//! New sessions are created with a timestamped placeholder title
//! (`New session - <time>`); a call to the model then names the session from
//! its first user message and the gateway updates the DB (the `prompt`
//! column doubles as the title) when the result lands. This keeps the
//! sidebar/header readable from the first second without letting a raw user
//! message become the permanent title.
//!
//! Generation runs on a dedicated OS thread with its own current-thread
//! runtime (nah_chat's stream future is `!Send`). The initial creation flow
//! is fire-and-forget; the regenerate endpoint waits on the outcome through
//! a channel so it can answer with the finished title.

use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Result, bail};
use futures_util::{StreamExt, pin_mut};
use mo_core::db;
use nah_chat::{
    ChatClient, ChatCompletionParamsBuilder, ChatCompletionStreamEvent, ChatMessage,
    ChatMessageContentValue,
};

use crate::state::AppState;

/// Maximum length of a stored session title, in characters (not bytes).
/// Both generated and user-entered titles are capped here and by the
/// rename endpoint.
pub(crate) const MAX_TITLE_CHARS: usize = 256;

/// Truncate a title to [`MAX_TITLE_CHARS`] characters.
pub(crate) fn cap_title(title: &str) -> String {
    title.chars().take(MAX_TITLE_CHARS).collect()
}

/// The model is asked for a bare title; the mock LLM keys on "short title"
/// to recognize this request. The length restriction lives here, in the
/// prompt, rather than in a hard `max_tokens` cap: reasoning models may
/// spend arbitrary tokens on `reasoning_content` before the title text.
const TITLE_SYSTEM_PROMPT: &str = "Generate a short title for this chat session. Given the user's first \
     message, reply with only the title: at most 6 words, no quotes, no period.";

/// The placeholder title used from the moment the message is sent until the
/// generated title lands (or generation is unavailable).
pub fn placeholder_title() -> String {
    format!(
        "New session - {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    )
}

/// Kick off title generation on a background thread and write the result
/// back to the DB when it lands. Failures are logged and leave the current
/// title in place. The default (first) model from the config file is used.
/// Used by the initial creation flow, where the result is picked up by the
/// UI's session-list polling — see [`spawn_title_generation_wait`] for the
/// regenerate endpoint, which needs the outcome itself.
pub fn spawn_title_generation(state: Arc<AppState>, session_id: String, first_message: String) {
    generate_title_on_thread(state, session_id, first_message, None);
}

/// Like [`spawn_title_generation`], but the background thread reports its
/// outcome on the returned receiver once generation finishes (or fails):
/// `Ok(Some(title))` — a title was generated and stored (already capped);
/// `Ok(None)` — nothing usable (no model configured, empty content, tool
/// call); `Err` — the LLM call itself failed.
///
/// The receiver is bounded only by the generation itself: if the caller
/// never receives, the send simply fails silently on drop.
pub fn spawn_title_generation_wait(
    state: Arc<AppState>,
    session_id: String,
    first_message: String,
) -> mpsc::Receiver<Result<Option<String>>> {
    let (tx, rx) = mpsc::channel();
    generate_title_on_thread(state, session_id, first_message, Some(tx));
    rx
}

/// Run title generation on a dedicated OS thread, store the result on the
/// session row, and report the outcome to `done` when one was requested.
fn generate_title_on_thread(
    state: Arc<AppState>,
    session_id: String,
    first_message: String,
    done: Option<mpsc::Sender<Result<Option<String>>>>,
) {
    let Some(model) = state.default_model().cloned() else {
        tracing::debug!(
            session = %session_id,
            "no model configured; keeping placeholder title"
        );
        if let Some(done) = done {
            let _ = done.send(Ok(None));
        }
        return;
    };
    std::thread::spawn(move || {
        let result = run_generation(&model, &first_message);
        match &result {
            Ok(Some(generated)) => {
                let generated = cap_title(generated);
                let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = db::set_prompt(&conn, &session_id, &generated) {
                    tracing::warn!(
                        session = %session_id,
                        "failed to save generated session title: {e}"
                    );
                } else {
                    tracing::info!(
                        session = %session_id,
                        title = %generated,
                        "generated session title"
                    );
                }
            }
            Ok(None) => {
                // No model configured, or the model returned nothing
                // usable; the current title stays.
                tracing::debug!(
                    session = %session_id,
                    "no generated session title; keeping current title"
                );
            }
            Err(e) => {
                tracing::warn!(session = %session_id, "session title generation failed: {e:#}");
            }
        }
        if let Some(done) = done {
            let _ = done.send(result);
        }
    });
}

/// One title-generation run: build the thread-local runtime and drive the
/// streaming LLM call to completion.
fn run_generation(model: &mo_core::ModelConfig, first_message: &str) -> Result<Option<String>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build title-generation runtime: {e}"))?;
    rt.block_on(generate_title(
        first_message,
        &model.base_url,
        &model.name,
        model.token.clone(),
    ))
}

/// Best-effort title generation from the session's first user message.
///
/// * `Ok(Some(title))` — a usable title was generated.
/// * `Ok(None)` — no model is configured, or the model returned nothing
///   usable (empty content or a tool call); the placeholder stays.
/// * `Err` — the LLM call itself failed; callers log and keep the
///   placeholder.
async fn generate_title(
    first_message: &str,
    base_url: &str,
    model: &str,
    auth_token: Option<String>,
) -> Result<Option<String>> {
    if base_url.is_empty() || model.is_empty() {
        // No usable model configuration; the placeholder is the intended
        // title.
        return Ok(None);
    }
    let client = ChatClient::init(base_url.to_string(), auth_token);
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: ChatMessageContentValue::Text(TITLE_SYSTEM_PROMPT.to_string()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        },
        ChatMessage {
            role: "user".to_string(),
            content: ChatMessageContentValue::Text(first_message.to_string()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    let message = generate_once(&client, model, &messages).await?;
    if message.tool_calls.is_some() {
        return Ok(None);
    }
    let title = message.content.to_string().trim().to_string();
    if title.is_empty() {
        // No hard `max_tokens` cap is set, so an empty content with
        // reasoning means the model simply never produced title text.
        // Surface that so odd model behavior is diagnosable instead of
        // silently leaving every session with the placeholder title.
        if message
            .reasoning_content
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty())
        {
            tracing::warn!("title generation returned reasoning but no title content");
        }
        return Ok(None);
    }
    Ok(Some(title))
}

/// One streaming chat completion call (same request shape the worker uses,
/// so the mock LLM keeps working). No retries: titles are cheap to
/// regenerate and a failure just leaves the placeholder in place. `Usage`
/// stream events are ignored — titles don't need token counts.
async fn generate_once(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
) -> Result<ChatMessage> {
    let mut params = ChatCompletionParamsBuilder::new();
    params.temperature(0.0);
    let stream = chat_client
        .chat_completion_stream(model, messages, &params)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start chat completion stream: {e}"))?;
    pin_mut!(stream);
    let mut message = ChatMessage::new();
    while let Some(event) = stream.next().await {
        match event.map_err(|e| anyhow::anyhow!("error in stream delta: {e}"))? {
            ChatCompletionStreamEvent::Delta(delta) => {
                message.apply_model_response_chunk(delta);
            }
            ChatCompletionStreamEvent::Usage(_) => {}
        }
    }
    if message.role.is_empty()
        && message.content.to_string().is_empty()
        && message.tool_calls.is_none()
    {
        bail!("model returned an empty response");
    }
    Ok(message)
}

// Unit tests live in `mo_gateway/src/tests/title_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/title_tests.rs"]
mod tests;
