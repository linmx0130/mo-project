//! The agent loop: stream a chat completion, journal the assistant message,
//! execute any tool calls, feed results back, and repeat until the model
//! produces a final answer.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, pin_mut};
use mo_core::{
    JournalEventKind, JournalMessage, JournalWriter, Session, ToolCallInfo,
};
use nah_chat::{
    ChatClient, ChatCompletionParamsBuilder, ChatMessage, ChatMessageContentValue,
    ToolCallRequest,
};
use serde_json::{Value, json};

use crate::prompt::build_system_prompt;
use crate::tools::{self, ToolContext};

/// Safety cap on tool-call turns so a misbehaving model cannot loop forever.
const MAX_TURNS: usize = 50;
/// Backoff schedule for LLM request failures: 5s / 15s / 30s.
const RETRY_DELAYS_SECS: [u64; 3] = [5, 15, 30];

pub struct AgentConfig {
    pub session: Session,
    pub workdir: PathBuf,
    pub data_dir: PathBuf,
    pub model_base_url: String,
    pub model_name: String,
    pub auth_token: Option<String>,
    pub subagent_depth: u32,
}

/// Run the full agent loop for one session, journaling as it goes.
/// The caller is responsible for DB status transitions.
pub async fn run_agent(config: AgentConfig, journal: &mut JournalWriter) -> Result<()> {
    let chat_client = ChatClient::init(config.model_base_url.clone(), config.auth_token.clone());
    let system_prompt = build_system_prompt(&config.workdir, config.subagent_depth);
    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage {
            role: "system".to_string(),
            content: ChatMessageContentValue::Text(system_prompt),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        },
        ChatMessage::user_text_message(&config.session.prompt),
    ];
    let tools = tools::tool_definitions();
    let tool_ctx = ToolContext {
        workdir: config.workdir.clone(),
        data_dir: config.data_dir.clone(),
        session: config.session.clone(),
        subagent_depth: config.subagent_depth,
        model_base_url: config.model_base_url.clone(),
        model_name: config.model_name.clone(),
        auth_token: config.auth_token.clone(),
    };

    for _ in 0..MAX_TURNS {
        let assistant = generate(&chat_client, &config.model_name, &messages, &tools)
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
            let result =
                tools::execute_tool(&tool_ctx, &tc.function.name, &tc.function.arguments).await;
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
    bail!("agent exceeded {MAX_TURNS} tool-call turns without producing a final answer");
}

/// One LLM call with retry/backoff (3 tries, 5s/15s/30s).
async fn generate(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    tools: &[Value],
) -> Result<ChatMessage> {
    let mut last_error: Option<anyhow::Error> = None;
    for (attempt, delay) in RETRY_DELAYS_SECS.iter().enumerate() {
        match generate_once(chat_client, model, messages, tools).await {
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
) -> Result<ChatMessage> {
    let mut params = ChatCompletionParamsBuilder::new();
    params
        .max_tokens(8192)
        .temperature(0.7)
        .insert("tools", json!(tools));
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
    use mo_core::{SessionStatus, open_db, db};
    use serde_json::{Value, json};

    fn delta_role(role: &str) -> Value {
        json!({ "role": role })
    }

    fn delta_content(content: &str) -> Value {
        json!({ "content": content })
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

        let conn = open_db(&data_dir.join("mo.db")).unwrap();
        let session = sample_session(
            &workdir,
            &data_dir.join("sessions").join("e2e").join("journal.jsonl"),
        );
        db::create_session(&conn, &session).unwrap();
        drop(conn);

        // Mock LLM server: stateful across the two requests.
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/chat/completions",
                post(
                    |calls: axum::extract::State<Arc<AtomicUsize>>| async move {
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
                            sse_payload(&[
                                delta_role("assistant"),
                                delta_content("The file says: hello world from notes.\n"),
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
            model_base_url: format!("http://{addr}"),
            model_name: "mock-model".to_string(),
            auth_token: None,
            subagent_depth: 0,
        };
        let mut journal =
            JournalWriter::open(std::path::Path::new(&session.journal_path)).unwrap();
        run_agent(agent_cfg, &mut journal).await.unwrap();

        // Journal: message(tool-call assistant), tool_call_start, tool_result, message(final).
        let events = mo_core::read_events(std::path::Path::new(&session.journal_path)).unwrap();
        assert_eq!(events.len(), 4, "events: {events:#?}");
        assert!(
            matches!(&events[0].kind, JournalEventKind::Message(m) if m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|t| t.len() == 1))
        );
        assert!(
            matches!(&events[1].kind, JournalEventKind::ToolCallStart { name, .. } if name == "read_file")
        );
        assert!(
            matches!(&events[2].kind, JournalEventKind::ToolResult { ok: true, output, .. } if output.contains("hello world from notes"))
        );
        assert!(
            matches!(&events[3].kind, JournalEventKind::Message(m) if m.role == "assistant" && m.content.contains("hello world from notes"))
        );
    }
}
