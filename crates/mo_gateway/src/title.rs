//! Session title generation: a short, separate gateway-side LLM call.
//!
//! New sessions are created with a timestamped placeholder title
//! (`New session - <time>`); a fire-and-forget call to the model then names
//! the session from its first user message and the gateway updates the DB
//! (the `prompt` column doubles as the title) when the result lands. This
//! keeps the sidebar/header readable from the first second without letting
//! a raw user message become the permanent title.

use std::sync::Arc;

use anyhow::{Result, bail};
use futures_util::{StreamExt, pin_mut};
use mo_core::db;
use nah_chat::{ChatClient, ChatCompletionParamsBuilder, ChatMessage, ChatMessageContentValue};

use crate::state::AppState;

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

/// Kick off title generation for a freshly created session on a background
/// thread and write the result back to the DB when it lands. Failures are
/// logged and leave the placeholder title in place. The default (first)
/// model from the config file is used.
///
/// A dedicated OS thread with its own current-thread runtime is used because
/// nah_chat's stream future is `!Send` and cannot be driven by a
/// `tokio::spawn` task on the shared gateway runtime.
pub fn spawn_title_generation(state: Arc<AppState>, session_id: String, first_message: String) {
    let Some(model) = state.default_model().cloned() else {
        tracing::debug!(
            session = %session_id,
            "no model configured; keeping placeholder title"
        );
        return;
    };
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!(session = %session_id, "failed to build title-generation runtime: {e}");
                return;
            }
        };
        match rt.block_on(generate_title(
            &first_message,
            &model.base_url,
            &model.name,
            model.token.clone(),
        )) {
            Ok(Some(generated)) => {
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
                // usable; the placeholder stays as the title.
                tracing::debug!(
                    session = %session_id,
                    "no generated session title; keeping placeholder"
                );
            }
            Err(e) => {
                tracing::warn!(session = %session_id, "session title generation failed: {e:#}");
            }
        }
    });
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
/// regenerate and a failure just leaves the placeholder in place.
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
    while let Some(delta) = stream.next().await {
        let delta = delta.map_err(|e| anyhow::anyhow!("error in stream delta: {e}"))?;
        message.apply_model_response_chunk(delta);
    }
    if message.role.is_empty()
        && message.content.to_string().is_empty()
        && message.tool_calls.is_none()
    {
        bail!("model returned an empty response");
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use serde_json::{Value, json};

    fn sse_payload(deltas: &[Value]) -> String {
        let mut out = String::new();
        for delta in deltas {
            out.push_str(&format!(
                "data: {}\n\n",
                json!({ "choices": [{ "delta": delta }] })
            ));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// A tiny mock LLM: responses are keyed on the first user message so
    /// parallel tests stay deterministic. Each request also asserts it
    /// carried the title system prompt and no hard `max_tokens` cap (the
    /// length restriction lives in the prompt; a cap is what broke title
    /// generation for reasoning models).
    ///
    /// * message contains "tool"      -> assistant tool call (unusable title)
    /// * message contains "empty"     -> empty assistant content
    /// * message contains "reasoning" -> reasoning_content, then content
    /// * otherwise                    -> plain text title
    async fn mock_llm() -> String {
        let router = Router::new().route(
            "/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let system = body["messages"]
                    .as_array()
                    .and_then(|msgs| msgs.iter().find(|m| m["role"] == "system"))
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("");
                assert!(
                    system.contains("short title"),
                    "title request must carry the title system prompt: {system}"
                );
                assert!(
                    body.get("max_tokens").is_none(),
                    "title request must not hard-cap output tokens: {body:?}"
                );
                let user = body["messages"]
                    .as_array()
                    .and_then(|msgs| msgs.iter().find(|m| m["role"] == "user"))
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("");
                let body = if user.contains("tool") {
                    sse_payload(&[
                        json!({ "role": "assistant" }),
                        json!({
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_t",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": r#"{"path":"x"}"#,
                                }
                            }]
                        }),
                    ])
                } else if user.contains("empty") {
                    sse_payload(&[json!({ "role": "assistant" })])
                } else if user.contains("reasoning") {
                    // A reasoning model streams `reasoning_content` first;
                    // the title only appears in `content` afterwards.
                    sse_payload(&[
                        json!({ "role": "assistant" }),
                        json!({ "reasoning_content": "The user wants " }),
                        json!({ "reasoning_content": "a title about notes." }),
                        json!({ "content": "Summarize notes.txt" }),
                    ])
                } else {
                    sse_payload(&[
                        json!({ "role": "assistant" }),
                        json!({ "content": "Explore notes.txt" }),
                    ])
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn placeholder_title_has_new_session_prefix() {
        let title = placeholder_title();
        assert!(title.starts_with("New session - "), "got: {title}");
        assert!(title.len() > "New session - ".len());
    }

    #[tokio::test]
    async fn no_model_config_skips_generation() {
        let result = generate_title("hello", "", "", None).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn generates_title_from_first_message() {
        let base_url = mock_llm().await;
        let title = generate_title(
            "Read notes.txt and summarize it",
            &base_url,
            "mock-model",
            None,
        )
        .await
        .unwrap();
        assert_eq!(title.as_deref(), Some("Explore notes.txt"));
    }

    #[tokio::test]
    async fn reasoning_model_title_comes_from_content_not_reasoning() {
        let base_url = mock_llm().await;
        // A reasoning model streams `reasoning_content` first; the title
        // must be read from the `content` that follows, never from the
        // reasoning text.
        let title = generate_title("reasoning model please", &base_url, "mock-model", None)
            .await
            .unwrap();
        assert_eq!(title.as_deref(), Some("Summarize notes.txt"));
    }

    #[tokio::test]
    async fn tool_call_or_empty_response_keeps_placeholder() {
        let base_url = mock_llm().await;
        // The model answers with a tool call -> no usable title.
        let title = generate_title("do the tool thing", &base_url, "mock-model", None)
            .await
            .unwrap();
        assert_eq!(title, None);
        // Empty content -> no usable title.
        let title = generate_title("empty answer please", &base_url, "mock-model", None)
            .await
            .unwrap();
        assert_eq!(title, None);
    }
}
