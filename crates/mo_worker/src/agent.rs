//! The agent loop: stream a chat completion, journal the assistant message,
//! execute any tool calls, feed results back, and repeat until the model
//! produces a final answer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, pin_mut};
use mo_core::{JournalEventKind, JournalMessage, JournalWriter, Session, ToolCallInfo};
use nah_chat::{
    ChatClient, ChatCompletionParamsBuilder, ChatMessage, ChatMessageContentValue,
    FunctionCallRequest, ToolCallRequest,
};
use serde_json::{Value, json};

use crate::prompt::build_system_prompt;
use crate::tools::{self, ToolContext};

/// Backoff schedule for LLM request failures: 5s / 15s / 30s.
const RETRY_DELAYS_SECS: [u64; 3] = [5, 15, 30];

pub struct AgentConfig {
    pub session: Session,
    pub workdir: PathBuf,
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub model_base_url: String,
    pub model_name: String,
    pub auth_token: Option<String>,
    pub subagent_depth: u32,
}

/// Run the full agent loop for one session, journaling as it goes.
/// The caller is responsible for DB status transitions.
pub async fn run_agent(config: AgentConfig, journal: &mut JournalWriter) -> Result<()> {
    let chat_client = ChatClient::init(config.model_base_url.clone(), config.auth_token.clone());
    let system_prompt =
        build_system_prompt(&config.workdir, &config.agents_dir, config.subagent_depth);
    // The conversation context is the journal history: user messages are
    // journaled by the gateway before the worker is spawned, and assistant /
    // tool messages by previous runs of this session. Rebuilding the context
    // from the journal makes every run — the first message and any followups
    // on a completed session — a natural continuation of the same thread.
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: ChatMessageContentValue::Text(system_prompt),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }];
    messages.extend(history_from_journal(Path::new(
        &config.session.journal_path,
    ))?);
    let tools = tools::tool_definitions();
    let tool_ctx = ToolContext {
        workdir: config.workdir.clone(),
        data_dir: config.data_dir.clone(),
        agents_dir: config.agents_dir.clone(),
        session: config.session.clone(),
        subagent_depth: config.subagent_depth,
        model_base_url: config.model_base_url.clone(),
        model_name: config.model_name.clone(),
        auth_token: config.auth_token.clone(),
    };

    loop {
        // `generate` streams the completion and journals a `MessageDelta`
        // event per content chunk as it arrives; the final `Message` event
        // (full assembled text) is journaled right after, so readers see
        // tokens arrive live and then settle on the canonical message.
        let assistant = generate(&chat_client, &config.model_name, &messages, &tools, journal)
            .await
            .context("LLM generation failed after retries")?;
        journal.append(JournalEventKind::Message(journal_message_from(&assistant)))?;

        let Some(tool_calls) = assistant.tool_calls.clone() else {
            messages.push(assistant);
            return Ok(());
        };

        let mut tool_messages = Vec::new();
        for tc in &tool_calls {
            journal.append(JournalEventKind::ToolCallStart {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            })?;
            // Streaming tools (bash) emit `ToolOutputDelta` events through
            // this sink while they run, so the frontend can render output
            // as it is produced rather than only when the tool finishes.
            let result = {
                let mut on_delta = |kind: JournalEventKind| {
                    let _ = journal.append(kind);
                };
                tools::execute_tool(
                    &tool_ctx,
                    &tc.function.name,
                    &tc.function.arguments,
                    &tc.id,
                    &mut on_delta,
                )
                .await
            };
            let (ok, output) = match result {
                Ok(output) => (true, output),
                Err(err) => (false, err),
            };
            journal.append(JournalEventKind::ToolResult {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                ok,
                output: output.clone(),
            })?;
            tool_messages.push(ChatMessage {
                role: "tool".to_string(),
                content: ChatMessageContentValue::Text(output),
                reasoning_content: None,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }
        messages.push(assistant);
        messages.extend(tool_messages);
    }
}

/// Rebuild the chat context from a session journal. `message` events map
/// directly to user/assistant messages (tool calls included); `tool_result`
/// events become the `tool`-role messages that must follow an assistant tool
/// call. The journal interleaves them in the correct order, so a completed
/// session can be resumed exactly where it left off.
fn history_from_journal(journal_path: &Path) -> Result<Vec<ChatMessage>> {
    let events = mo_core::read_events(journal_path).context("failed to read session journal")?;
    let mut messages = Vec::new();
    for event in events {
        match event.kind {
            JournalEventKind::Message(m) => messages.push(ChatMessage {
                role: m.role,
                content: ChatMessageContentValue::Text(m.content),
                reasoning_content: m.reasoning_content,
                tool_call_id: m.tool_call_id,
                tool_calls: m.tool_calls.map(|calls| {
                    calls
                        .into_iter()
                        .map(|tc: ToolCallInfo| ToolCallRequest {
                            id: tc.id,
                            _type: "function".to_string(),
                            function: FunctionCallRequest {
                                name: tc.name,
                                arguments: tc.arguments,
                            },
                        })
                        .collect()
                }),
            }),
            JournalEventKind::ToolResult { id, output, .. } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: ChatMessageContentValue::Text(output),
                reasoning_content: None,
                tool_call_id: Some(id),
                tool_calls: None,
            }),
            _ => {}
        }
    }
    Ok(messages)
}

/// One LLM call with retry/backoff (3 tries, 5s/15s/30s).
///
/// Content and reasoning chunks are journaled as `MessageDelta` events
/// while the stream runs — reasoning models emit `reasoning_content`
/// before the visible answer, and both stream token-by-token — so the
/// journal (and with it the gateway SSE) carries tokens live. A failed
/// attempt may leave a few orphan deltas behind; the final `Message` event
/// that follows a successful attempt replaces them on the reader side, so
/// history stays correct.
async fn generate(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    tools: &[Value],
    journal: &mut JournalWriter,
) -> Result<ChatMessage> {
    let mut last_error: Option<anyhow::Error> = None;
    for (attempt, delay) in RETRY_DELAYS_SECS.iter().enumerate() {
        match generate_once(chat_client, model, messages, tools, journal).await {
            Ok(msg) => return Ok(msg),
            Err(e) => {
                tracing::warn!("LLM call failed (attempt {}): {e:#}", attempt + 1);
                last_error = Some(e);
                if attempt + 1 < RETRY_DELAYS_SECS.len() {
                    tokio::time::sleep(Duration::from_secs(*delay)).await;
                }
            }
        }
    }
    Err(last_error.unwrap())
}

async fn generate_once(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    tools: &[Value],
    journal: &mut JournalWriter,
) -> Result<ChatMessage> {
    let mut params = ChatCompletionParamsBuilder::new();
    // No hard `max_tokens` cap: reasoning models may spend arbitrary tokens
    // on `reasoning_content` before the actual answer, and a cap truncates
    // the stream before any content arrives. Let the model run until it
    // finishes on its own.
    params.temperature(0.7).insert("tools", json!(tools));
    let stream = chat_client
        .chat_completion_stream(model, messages, &params)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start chat completion stream: {e}"))?;
    pin_mut!(stream);
    let mut message = ChatMessage::new();
    while let Some(delta) = stream.next().await {
        let delta = delta.map_err(|e| anyhow::anyhow!("error in stream delta: {e}"))?;
        // Journal every chunk that carries visible text or reasoning, so
        // both stream token-by-token (reasoning models emit
        // `reasoning_content` deltas first, then `content` deltas).
        let content = delta.content.clone().unwrap_or_default();
        let reasoning_content = delta.reasoning_content.clone().filter(|s| !s.is_empty());
        if !content.is_empty() || reasoning_content.is_some() {
            journal.append(JournalEventKind::MessageDelta {
                content,
                reasoning_content,
            })?;
        }
        message.apply_model_response_chunk(delta);
    }
    let empty = message.role.is_empty()
        && message.content.to_string().is_empty()
        && message.tool_calls.is_none();
    if empty {
        bail!("model returned an empty response");
    }
    Ok(message)
}

fn journal_message_from(msg: &ChatMessage) -> JournalMessage {
    JournalMessage {
        role: msg.role.clone(),
        content: msg.content.to_string(),
        reasoning_content: msg.reasoning_content.clone(),
        tool_call_id: msg.tool_call_id.clone(),
        tool_calls: msg.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|tc: &ToolCallRequest| ToolCallInfo {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                })
                .collect()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, routing::post};
    use mo_core::{SessionStatus, db, open_db};
    use serde_json::{Value, json};

    fn delta_role(role: &str) -> Value {
        json!({ "role": role })
    }

    fn delta_content(content: &str) -> Value {
        json!({ "content": content })
    }

    fn delta_reasoning(content: &str) -> Value {
        json!({ "reasoning_content": content })
    }

    fn delta_tool_call(index: usize, id: &str, name: &str, arguments: &str) -> Value {
        json!({
            "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            }]
        })
    }

    /// Build a full SSE payload from a list of delta chunks.
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

    fn sample_session(workdir: &std::path::Path, journal_path: &std::path::Path) -> Session {
        let now = chrono::Utc::now().to_rfc3339();
        Session {
            id: "e2e".to_string(),
            parent_id: None,
            workdir: workdir.display().to_string(),
            prompt: "Read notes.txt and tell me what it says.".to_string(),
            model: "mock-model".to_string(),
            status: SessionStatus::Pending,
            pid: None,
            journal_path: journal_path.display().to_string(),
            created_at: now.clone(),
            updated_at: now,
            heartbeat_at: None,
            error: None,
        }
    }

    /// End-to-end agent loop test against a tiny mock LLM server that
    /// replays canned chat-completion SSE responses:
    /// request 1 -> assistant tool_call(read_file notes.txt),
    /// request 2 -> assistant final text answer.
    #[tokio::test]
    async fn e2e_agent_loop_with_mock_llm() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("work");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::write(workdir.join("notes.txt"), "hello world from notes\n").unwrap();
        let data_dir = dir.path().join("data");
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("AGENTS.md"),
            "Global rule: answer in lowercase.\n",
        )
        .unwrap();

        let conn = open_db(&data_dir.join("mo.db")).unwrap();
        let session = sample_session(
            &workdir,
            &data_dir.join("sessions").join("e2e").join("journal.jsonl"),
        );
        db::create_session(&conn, &session).unwrap();
        drop(conn);

        // Mock LLM server: stateful across the two requests. Each request is
        // checked to carry the global AGENTS.md inside the system prompt.
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/chat/completions",
                post(
                    |calls: axum::extract::State<Arc<AtomicUsize>>,
                     body: axum::extract::Json<Value>| async move {
                        let system = body["messages"]
                            .as_array()
                            .and_then(|msgs| msgs.iter().find(|m| m["role"] == "system"))
                            .and_then(|m| m["content"].as_str())
                            .unwrap_or("");
                        assert!(
                            system.contains("Global rule: answer in lowercase."),
                            "system prompt missing global AGENTS.md: {system}"
                        );
                        assert!(
                            body.get("max_tokens").is_none(),
                            "worker request must not hard-cap output tokens: {body:?}"
                        );
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        let body = if n == 0 {
                            sse_payload(&[
                                delta_role("assistant"),
                                delta_tool_call(
                                    0,
                                    "call_1",
                                    "read_file",
                                    r#"{"path":"notes.txt"}"#,
                                ),
                            ])
                        } else {
                            // The final answer streams reasoning first, then
                            // several content chunks, exercising
                            // token-by-token journaling of both fields.
                            sse_payload(&[
                                delta_role("assistant"),
                                delta_reasoning("Let me recall: "),
                                delta_reasoning("notes say hello."),
                                delta_content("The file says: "),
                                delta_content("hello world "),
                                delta_content("from notes.\n"),
                            ])
                        };
                        (
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            body,
                        )
                    },
                ),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent_cfg = AgentConfig {
            session: session.clone(),
            workdir: workdir.clone(),
            data_dir: data_dir.clone(),
            agents_dir,
            model_base_url: format!("http://{addr}"),
            model_name: "mock-model".to_string(),
            auth_token: None,
            subagent_depth: 0,
        };
        let mut journal = JournalWriter::open(std::path::Path::new(&session.journal_path)).unwrap();
        // The gateway journals the user message before spawning the worker;
        // `run_agent` rebuilds its context from the journal.
        journal
            .append(JournalEventKind::Message(JournalMessage {
                role: "user".to_string(),
                content: "Read notes.txt and tell me what it says.".to_string(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }))
            .unwrap();
        run_agent(agent_cfg, &mut journal).await.unwrap();

        // Journal sequence: user message, assistant(tool call), tool_call_start,
        // tool_result, then the final answer streamed as reasoning deltas
        // followed by content deltas, then the assembled assistant message.
        let events = mo_core::read_events(std::path::Path::new(&session.journal_path)).unwrap();
        assert_eq!(events.len(), 10, "events: {events:#?}");
        let kinds: Vec<&JournalEventKind> = events.iter().map(|e| &e.kind).collect();
        assert!(
            matches!(kinds[0], JournalEventKind::Message(m) if m.role == "user" && m.content.contains("Read notes.txt"))
        );
        assert!(
            matches!(kinds[1], JournalEventKind::Message(m) if m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|t| t.len() == 1))
        );
        assert!(
            matches!(kinds[2], JournalEventKind::ToolCallStart { name, .. } if name == "read_file")
        );
        assert!(
            matches!(kinds[3], JournalEventKind::ToolResult { ok: true, output, .. } if output.contains("hello world from notes"))
        );
        // Token-by-token preview: reasoning deltas first (empty content),
        // then the content deltas assembling the final answer.
        assert_eq!(
            kinds[4],
            &JournalEventKind::MessageDelta {
                content: String::new(),
                reasoning_content: Some("Let me recall: ".to_string()),
            }
        );
        assert_eq!(
            kinds[5],
            &JournalEventKind::MessageDelta {
                content: String::new(),
                reasoning_content: Some("notes say hello.".to_string()),
            }
        );
        assert_eq!(
            kinds[6],
            &JournalEventKind::MessageDelta {
                content: "The file says: ".to_string(),
                reasoning_content: None,
            }
        );
        assert_eq!(
            kinds[7],
            &JournalEventKind::MessageDelta {
                content: "hello world ".to_string(),
                reasoning_content: None,
            }
        );
        assert_eq!(
            kinds[8],
            &JournalEventKind::MessageDelta {
                content: "from notes.\n".to_string(),
                reasoning_content: None,
            }
        );
        assert!(
            matches!(kinds[9], JournalEventKind::Message(m) if m.role == "assistant" && m.content.contains("hello world from notes") && m.reasoning_content.as_deref() == Some("Let me recall: notes say hello."))
        );
    }
}
