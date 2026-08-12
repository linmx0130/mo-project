//! Unit tests for the `agent` module — production code lives in
//! `mo_worker/src/agent.rs`. Wired from there with `#[cfg(test)] #[path = "tests/agent_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Router, routing::post};
use mo_core::{Mode, SessionStatus, db, open_db};
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

/// Build an SSE payload ending with a usage chunk (empty `choices` +
/// top-level `usage`) before `[DONE]`, as OpenAI-compatible servers send
/// when the request sets `stream_options.include_usage`.
fn sse_payload_with_usage(deltas: &[Value], prompt_tokens: u64, completion_tokens: u64) -> String {
    let mut out = String::new();
    for delta in deltas {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({ "choices": [{ "delta": delta }] })
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }
        })
    ));
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
        mode: Mode::Build,
        pid: None,
        journal_path: journal_path.display().to_string(),
        created_at: now.clone(),
        updated_at: now,
        heartbeat_at: None,
        error: None,
    }
}

/// A journal whose last assistant message carries tool_calls that were
/// never answered (the worker died or was stopped mid-tool) must rebuild
/// without that dangling message — the model API rejects tool_calls
/// without matching tool messages, which is exactly what broke followups
/// on sessions that were killed while a bash command was running.
#[test]
fn history_drops_dangling_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "first message".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    // The worker journaled the assistant tool call, then died before
    // the tool_result (only the tool_call_start made it out).
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCallInfo {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"./gradlew assembleDebug"}"#.to_string(),
            }]),
        }))
        .unwrap();
    journal
        .append(JournalEventKind::ToolCallStart {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"./gradlew assembleDebug"}"#.to_string(),
        })
        .unwrap();
    // The user's followup ("Continue") is journaled by the gateway when
    // the session is resumed.
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "Continue".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert!(system.is_none(), "no system prompt journaled here");
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content.to_string(), "first message");
    assert_eq!(messages[1].role, "user");
    assert_eq!(messages[1].content.to_string(), "Continue");
    // No assistant message with tool_calls may survive.
    for m in &messages {
        assert!(
            !(m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())),
            "dangling tool call survived: {messages:#?}"
        );
    }
}

/// A completed tool round-trip (assistant tool_calls followed by the
/// matching tool_result) must be preserved verbatim.
#[test]
fn history_keeps_answered_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "do it".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCallInfo {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"notes.txt"}"#.to_string(),
            }]),
        }))
        .unwrap();
    journal
        .append(JournalEventKind::ToolResult {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            ok: true,
            output: "file contents".to_string(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: "done".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert!(system.is_none(), "no system prompt journaled here");
    assert_eq!(messages.len(), 4, "messages: {messages:#?}");
    assert_eq!(messages[1].role, "assistant");
    assert!(
        messages[1]
            .tool_calls
            .as_ref()
            .is_some_and(|c| c.len() == 1),
        "answered tool call must be kept: {messages:#?}"
    );
    assert_eq!(messages[2].role, "tool");
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[3].role, "assistant");
    assert_eq!(messages[3].content.to_string(), "done");
}

/// Tool results are journaled in *completion* order (parallel
/// execution), but the rebuilt context must present them in the
/// assistant's `tool_calls` array order — the model-facing history is
/// deterministic regardless of which call finished first.
#[test]
fn history_reorders_tool_messages_to_match_call_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "read both files".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    // The assistant called read_file(a) first, then read_file(b).
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Some(vec![
                ToolCallInfo {
                    id: "call_a".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                },
                ToolCallInfo {
                    id: "call_b".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"b.txt"}"#.to_string(),
                },
            ]),
        }))
        .unwrap();
    // ...but call_b finished first, so its result was journaled first.
    journal
        .append(JournalEventKind::ToolResult {
            id: "call_b".to_string(),
            name: "read_file".to_string(),
            ok: true,
            output: "content of b".to_string(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::ToolResult {
            id: "call_a".to_string(),
            name: "read_file".to_string(),
            ok: true,
            output: "content of a".to_string(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: "done".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert!(system.is_none(), "no system prompt journaled here");
    assert_eq!(messages.len(), 5, "messages: {messages:#?}");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[2].role, "tool");
    // Deterministic order: call_a's result before call_b's, even though
    // the journal has them in the opposite (completion) order.
    assert_eq!(
        messages[2].tool_call_id.as_deref(),
        Some("call_a"),
        "messages: {messages:#?}"
    );
    assert_eq!(messages[2].content.to_string(), "content of a");
    assert_eq!(
        messages[3].tool_call_id.as_deref(),
        Some("call_b"),
        "messages: {messages:#?}"
    );
    assert_eq!(messages[3].content.to_string(), "content of b");
    assert_eq!(messages[4].role, "assistant");
    assert_eq!(messages[4].content.to_string(), "done");
}

/// Two tool calls in a single assistant message run concurrently and
/// are answered in one round-trip: all `ToolCallStart` events land
/// before any `ToolResult`, both results are journaled, and the
/// follow-up LLM request carries the tool messages in the original call
/// order (deterministic regardless of completion order).
#[tokio::test]
async fn e2e_parallel_tool_calls_are_batched_in_call_order() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("a.txt"), "AAA\n").unwrap();
    std::fs::write(workdir.join("b.txt"), "BBB\n").unwrap();
    let data_dir = dir.path().join("data");
    let agents_dir = dir.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    let session = sample_session(
        &workdir,
        &data_dir.join("sessions").join("e2e").join("journal.jsonl"),
    );
    db::create_session(&conn, &session).unwrap();
    drop(conn);

    // Mock LLM server. Request 1: two tool calls in ONE assistant
    // message. Request 2: the final answer; its body must show the two
    // tool messages in call order (call_1 before call_2) even though
    // the tools ran concurrently.
    let calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route(
            "/chat/completions",
            post(
                |calls: axum::extract::State<Arc<AtomicUsize>>,
                 body: axum::extract::Json<Value>| async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 1 {
                        // Second request: the tool results are fed back.
                        let msgs = body["messages"].as_array().unwrap();
                        let tail: Vec<&Value> = msgs.iter().rev().take(3).collect();
                        // [assistant(tool_calls), tool(call_1), tool(call_2)]
                        assert_eq!(tail[2]["role"], "assistant");
                        assert_eq!(tail[1]["role"], "tool");
                        assert_eq!(tail[1]["tool_call_id"], "call_1");
                        assert!(
                            tail[1]["content"].as_str().unwrap().contains("AAA"),
                            "got: {}",
                            tail[1]
                        );
                        assert_eq!(tail[0]["role"], "tool");
                        assert_eq!(tail[0]["tool_call_id"], "call_2");
                        assert!(
                            tail[0]["content"].as_str().unwrap().contains("BBB"),
                            "got: {}",
                            tail[0]
                        );
                    }
                    let body = if n == 0 {
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_tool_call(0, "call_1", "read_file", r#"{"path":"a.txt"}"#),
                                delta_tool_call(1, "call_2", "read_file", r#"{"path":"b.txt"}"#),
                            ],
                            40,
                            6,
                        )
                    } else {
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_content("saw "),
                                delta_content("A and B"),
                            ],
                            60,
                            3,
                        )
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
        context_window: Some(4096),
        subagent_depth: 0,
        max_tool_concurrency: 8,
        context_compression_threshold: 0.75,
    };
    let mut journal = JournalWriter::open(std::path::Path::new(&session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "read a.txt and b.txt".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    run_agent(agent_cfg, &mut journal).await.unwrap();

    let events = mo_core::read_events(std::path::Path::new(&session.journal_path)).unwrap();
    let kinds: Vec<&JournalEventKind> = events.iter().map(|e| &e.kind).collect();
    let start_positions: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter_map(|(idx, k)| {
            matches!(k, JournalEventKind::ToolCallStart { name, .. } if name == "read_file")
                .then_some(idx)
        })
        .collect();
    let result_positions: Vec<usize> = kinds
            .iter()
            .enumerate()
            .filter_map(|(idx, k)| {
                matches!(k, JournalEventKind::ToolResult { ok: true, output, .. } if output.contains("AAA") || output.contains("BBB"))
                    .then_some(idx)
            })
            .collect();
    assert_eq!(start_positions.len(), 2, "starts: {start_positions:?}");
    assert_eq!(result_positions.len(), 2, "results: {result_positions:?}");
    // Every ToolCallStart lands before any ToolResult (the UI shows all
    // tool blocks at once, then each completes independently).
    assert!(
        start_positions.iter().max().unwrap() < result_positions.iter().min().unwrap(),
        "a tool result was journaled before all starts: {kinds:#?}"
    );
    // The final answer arrived.
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            JournalEventKind::Message(m)
                if m.role == "assistant" && m.content.contains("A and B")
        )),
        "kinds: {kinds:#?}"
    );
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
                    assert_eq!(
                        body["stream_options"]["include_usage"], true,
                        "worker request must ask for usage: {body:?}"
                    );
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 {
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_tool_call(
                                    0,
                                    "call_1",
                                    "read_file",
                                    r#"{"path":"notes.txt"}"#,
                                ),
                            ],
                            30,
                            5,
                        )
                    } else {
                        // The final answer streams reasoning first, then
                        // several content chunks, exercising
                        // token-by-token journaling of both fields.
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_reasoning("Let me recall: "),
                                delta_reasoning("notes say hello."),
                                delta_content("The file says: "),
                                delta_content("hello world "),
                                delta_content("from notes.\n"),
                            ],
                            48,
                            12,
                        )
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
        context_window: Some(4096),
        subagent_depth: 0,
        max_tool_concurrency: 8,
        context_compression_threshold: 0.75,
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

    // Journal sequence: user message, the journaled system prompt, then
    // context_usage (request 1), assistant(tool call), tool_call_start,
    // tool_result, the final answer streamed as reasoning deltas
    // followed by content deltas, context_usage (request 2), and the
    // assembled assistant message.
    let events = mo_core::read_events(std::path::Path::new(&session.journal_path)).unwrap();
    assert_eq!(events.len(), 13, "events: {events:#?}");
    let kinds: Vec<&JournalEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        matches!(kinds[0], JournalEventKind::Message(m) if m.role == "user" && m.content.contains("Read notes.txt"))
    );
    // The system prompt is journaled once, on the first run, with the
    // session's mode framing.
    assert!(
        matches!(kinds[1], JournalEventKind::SystemPrompt { content, mode: Mode::Build } if content.contains("Build mode") && content.contains("Global rule: answer in lowercase.")),
        "expected journaled Build-mode system prompt, got: {:?}",
        kinds[1]
    );
    // Each LLM call journals the API-reported context length against the
    // configured window; the first call's count is smaller than the
    // second's (the tool round-trip grew the context).
    assert_eq!(
        kinds[2],
        &JournalEventKind::ContextUsage {
            tokens: 30,
            context_window: Some(4096),
        }
    );
    assert!(
        matches!(kinds[3], JournalEventKind::Message(m) if m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|t| t.len() == 1))
    );
    assert!(
        matches!(kinds[4], JournalEventKind::ToolCallStart { name, .. } if name == "read_file")
    );
    assert!(
        matches!(kinds[5], JournalEventKind::ToolResult { ok: true, output, .. } if output.contains("hello world from notes"))
    );
    // Token-by-token preview: reasoning deltas first (empty content),
    // then the content deltas assembling the final answer.
    assert_eq!(
        kinds[6],
        &JournalEventKind::MessageDelta {
            content: String::new(),
            reasoning_content: Some("Let me recall: ".to_string()),
        }
    );
    assert_eq!(
        kinds[7],
        &JournalEventKind::MessageDelta {
            content: String::new(),
            reasoning_content: Some("notes say hello.".to_string()),
        }
    );
    assert_eq!(
        kinds[8],
        &JournalEventKind::MessageDelta {
            content: "The file says: ".to_string(),
            reasoning_content: None,
        }
    );
    assert_eq!(
        kinds[9],
        &JournalEventKind::MessageDelta {
            content: "hello world ".to_string(),
            reasoning_content: None,
        }
    );
    assert_eq!(
        kinds[10],
        &JournalEventKind::MessageDelta {
            content: "from notes.\n".to_string(),
            reasoning_content: None,
        }
    );
    assert_eq!(
        kinds[11],
        &JournalEventKind::ContextUsage {
            tokens: 48,
            context_window: Some(4096),
        }
    );
    assert!(
        matches!(kinds[12], JournalEventKind::Message(m) if m.role == "assistant" && m.content.contains("hello world from notes") && m.reasoning_content.as_deref() == Some("Let me recall: notes say hello."))
    );
}

/// A second run on the same journal must reuse the journaled system
/// prompt verbatim (never rebuild or re-journal it) and must send it as
/// the first message, so the LLM sees the exact same system text on
/// every run (prompt-cache friendly).
#[test]
fn history_reuses_journaled_system_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    let prompt = "You are in Build mode. [stale copy that must survive]";
    journal
        .append(JournalEventKind::SystemPrompt {
            content: prompt.to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: "done".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some(prompt));
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    // The system prompt is session metadata: it never appears inline.
    assert!(
        !messages.iter().any(|m| m.role == "system"),
        "journaled system prompt must not appear as a chat message"
    );
}

/// A `ModeChange` event injected by the gateway before a followup user
/// message must appear in the rebuilt context as a user-role message
/// directly before that user message — the model reads the mode notice
/// before the real user message.
#[test]
fn history_places_mode_change_before_followup_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "first message".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "You are in Build mode.".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: "done".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    // The gateway appended the mode-change notice, then the followup.
    journal
        .append(JournalEventKind::ModeChange {
            mode: Mode::Plan,
            content: "[Session mode changed to plan]\n\nYou are now in Plan mode.".to_string(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "followup".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("You are in Build mode."));
    assert_eq!(messages.len(), 4, "messages: {messages:#?}");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content.to_string(), "first message");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content.to_string(), "done");
    // The mode change is a user-role message sitting directly before the
    // followup user message.
    assert_eq!(messages[2].role, "user");
    assert_eq!(
        messages[2].content.to_string(),
        "[Session mode changed to plan]\n\nYou are now in Plan mode."
    );
    assert_eq!(messages[3].role, "user");
    assert_eq!(messages[3].content.to_string(), "followup");
    // The journaled system prompt is still session metadata, not a chat
    // message.
    assert!(
        !messages.iter().any(|m| m.role == "system"),
        "journaled system prompt must not appear as a chat message"
    );
}

fn user_msg(content: &str) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: "user".to_string(),
        content: content.to_string(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

fn assistant_msg(content: &str) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: "assistant".to_string(),
        content: content.to_string(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

/// A journal containing a handoff (context compression): every event
/// before the handoff is dropped from the model context, the handoff
/// text becomes the first user message (wrapped in the marker), and the
/// fresh `SystemPrompt` journaled right after the handoff becomes the
/// system prompt.
#[test]
fn history_drops_pre_handoff_events_and_injects_handoff_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("original task")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(assistant_msg("earlier work")).unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TEXT".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "fresh system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(user_msg("followup")).unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("fresh system"));
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert_eq!(messages[0].role, "user");
    let handoff_msg = messages[0].content.to_string();
    assert!(
        handoff_msg.contains("[Context compressed"),
        "handoff message must carry the marker: {handoff_msg}"
    );
    assert!(handoff_msg.contains("HANDOFF TEXT"), "got: {handoff_msg}");
    assert_eq!(messages[1].role, "user");
    assert_eq!(messages[1].content.to_string(), "followup");
}

/// Multiple compressions: the *last* handoff is the boundary. Earlier
/// handoffs (and their post-handoff events) are below it and dropped.
#[test]
fn history_uses_last_handoff_when_multiple() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("original task")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF ONE".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "sys after one".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(user_msg("mid work")).unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TWO".to_string(),
            mode: Mode::Plan,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "sys after two".to_string(),
            mode: Mode::Plan,
        })
        .unwrap();
    journal.append(user_msg("followup")).unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("sys after two"));
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert!(messages[0].content.to_string().contains("HANDOFF TWO"));
    assert!(!messages[0].content.to_string().contains("HANDOFF ONE"));
    assert_eq!(messages[1].content.to_string(), "followup");
}

/// A journal that ends right after the handoff + fresh SystemPrompt
/// (the run stopped right after compressing) rebuilds to the handoff as
/// the only message — a followup then continues from the handoff.
#[test]
fn history_with_handoff_and_no_followup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("original task")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(assistant_msg("earlier work")).unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TEXT".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "fresh system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("fresh system"));
    assert_eq!(messages.len(), 1, "messages: {messages:#?}");
    assert!(messages[0].content.to_string().contains("HANDOFF TEXT"));
}

/// The post-handoff tool round-trip still gets the deterministic
/// call-order reordering (tool results journaled in completion order are
/// re-presented in the assistant's `tool_calls` array order).
#[test]
fn history_keeps_post_handoff_tool_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("original task")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TEXT".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "fresh system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(user_msg("read files")).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Some(vec![
                ToolCallInfo {
                    id: "call_a".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                },
                ToolCallInfo {
                    id: "call_b".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"b.txt"}"#.to_string(),
                },
            ]),
        }))
        .unwrap();
    // call_b finished first.
    journal
        .append(JournalEventKind::ToolResult {
            id: "call_b".to_string(),
            name: "read_file".to_string(),
            ok: true,
            output: "content of b".to_string(),
        })
        .unwrap();
    journal
        .append(JournalEventKind::ToolResult {
            id: "call_a".to_string(),
            name: "read_file".to_string(),
            ok: true,
            output: "content of a".to_string(),
        })
        .unwrap();
    journal.append(assistant_msg("done")).unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("fresh system"));
    assert_eq!(messages.len(), 6, "messages: {messages:#?}");
    assert!(messages[0].content.to_string().contains("HANDOFF TEXT"));
    assert_eq!(messages[1].content.to_string(), "read files");
    assert_eq!(messages[2].role, "assistant");
    assert_eq!(
        messages[3].tool_call_id.as_deref(),
        Some("call_a"),
        "messages: {messages:#?}"
    );
    assert_eq!(
        messages[4].tool_call_id.as_deref(),
        Some("call_b"),
        "messages: {messages:#?}"
    );
    assert_eq!(messages[5].content.to_string(), "done");
}

/// A worker that died mid-tool *after* a handoff leaves the compressed
/// context's tail dangling; the dangling assistant tool-call message is
/// dropped just like in an uncompressed journal.
#[test]
fn history_drops_dangling_tool_calls_after_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("original task")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TEXT".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "fresh system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal.append(user_msg("continue")).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCallInfo {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"make"}"#.to_string(),
            }]),
        }))
        .unwrap();
    journal
        .append(JournalEventKind::ToolCallStart {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"make"}"#.to_string(),
        })
        .unwrap();
    drop(journal);

    let (system, messages, _) = history_from_journal(&path).unwrap();
    assert_eq!(system.as_deref(), Some("fresh system"));
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert!(messages[0].content.to_string().contains("HANDOFF TEXT"));
    assert_eq!(messages[1].content.to_string(), "continue");
    for m in &messages {
        assert!(
            !(m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())),
            "dangling tool call survived: {messages:#?}"
        );
    }
}

/// The last `context_usage` event is returned so the compression gate
/// can seed a resumed run — and after a handoff it is the *compressed*
/// count (pre-handoff usages are below the boundary).
#[test]
fn history_tracks_last_context_usage() {
    // No handoff: the last usage overall.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = JournalWriter::open(&path).unwrap();
    journal.append(user_msg("hi")).unwrap();
    journal
        .append(JournalEventKind::ContextUsage {
            tokens: 100,
            context_window: Some(200),
        })
        .unwrap();
    journal.append(assistant_msg("ok")).unwrap();
    journal
        .append(JournalEventKind::ContextUsage {
            tokens: 120,
            context_window: Some(200),
        })
        .unwrap();
    drop(journal);
    let (_, _, last) = history_from_journal(&path).unwrap();
    assert_eq!(last, Some(120));

    // With a handoff: pre-handoff usages are dropped; the post-handoff
    // (compressed) one is the seed.
    let path2 = dir.path().join("journal2.jsonl");
    let mut journal = JournalWriter::open(&path2).unwrap();
    journal.append(user_msg("hi")).unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "old system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::ContextUsage {
            tokens: 900,
            context_window: Some(1000),
        })
        .unwrap();
    journal
        .append(JournalEventKind::Handoff {
            content: "HANDOFF TEXT".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::SystemPrompt {
            content: "fresh system".to_string(),
            mode: Mode::Build,
        })
        .unwrap();
    journal
        .append(JournalEventKind::ContextUsage {
            tokens: 30,
            context_window: Some(1000),
        })
        .unwrap();
    drop(journal);
    let (_, _, last) = history_from_journal(&path2).unwrap();
    assert_eq!(last, Some(30));
}

/// End-to-end context compression against a mock LLM server: request 1
/// reports usage at 80% of a tiny window (threshold 0.75) and requests
/// a tool call; after the tool round-trip the worker asks the model for
/// a handoff prompt (request 2, no tools); it journals the `Handoff` +
/// fresh `SystemPrompt`, then continues with the *compressed* context
/// (request 3 = exactly system + the handoff user message — no trace of
/// the original prompt or the tool round-trip).
#[tokio::test]
async fn e2e_context_compression_generates_handoff_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("notes.txt"), "hello world from notes\n").unwrap();
    let data_dir = dir.path().join("data");
    let agents_dir = dir.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let conn = open_db(&data_dir.join("mo.db")).unwrap();
    let session = sample_session(
        &workdir,
        &data_dir.join("sessions").join("e2e").join("journal.jsonl"),
    );
    db::create_session(&conn, &session).unwrap();
    drop(conn);

    let calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route(
            "/chat/completions",
            post(
                |calls: axum::extract::State<Arc<AtomicUsize>>,
                 body: axum::extract::Json<Value>| async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 {
                        // Request 1: a tool call; usage crosses the 75%
                        // threshold of the 40-token window (32/40 = 0.8).
                        assert!(
                            body["tools"].is_array(),
                            "regular calls must advertise tools: {body:?}"
                        );
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_tool_call(
                                    0,
                                    "call_1",
                                    "read_file",
                                    r#"{"path":"notes.txt"}"#,
                                ),
                            ],
                            32,
                            4,
                        )
                    } else if n == 1 {
                        // Request 2: the handoff call — no tools, and the
                        // handoff instruction as the last user message.
                        assert!(
                            body.get("tools").is_none(),
                            "handoff call must not advertise tools: {body:?}"
                        );
                        let msgs = body["messages"].as_array().unwrap();
                        let last = msgs.last().unwrap();
                        assert_eq!(last["role"], "user");
                        assert!(
                            last["content"].as_str().unwrap().contains("handoff prompt"),
                            "handoff instruction missing: {last}"
                        );
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_content("HANDOFF: original input ... next step"),
                            ],
                            999, // ignored: no context_usage is journaled for the handoff call
                            20,
                        )
                    } else {
                        // Request 3: the compressed context — exactly the
                        // fresh system prompt + the handoff as a user
                        // message. No trace of the original prompt or the
                        // tool round-trip.
                        let msgs = body["messages"].as_array().unwrap();
                        assert_eq!(
                            msgs.len(),
                            2,
                            "compressed context must be system + handoff: {msgs:#?}"
                        );
                        assert_eq!(msgs[0]["role"], "system");
                        assert_eq!(msgs[1]["role"], "user");
                        let content = msgs[1]["content"].as_str().unwrap();
                        assert!(
                            content.contains("[Context compressed"),
                            "handoff message must carry the marker: {content}"
                        );
                        assert!(
                            content.contains("HANDOFF: original input"),
                            "got: {content}"
                        );
                        assert!(
                            !content.contains("do the long task"),
                            "pre-handoff events must be dropped: {content}"
                        );
                        sse_payload_with_usage(
                            &[
                                delta_role("assistant"),
                                delta_content("continued after compression"),
                            ],
                            5,
                            3,
                        )
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
        context_window: Some(40),
        subagent_depth: 0,
        max_tool_concurrency: 8,
        context_compression_threshold: 0.75,
    };
    let mut journal = JournalWriter::open(std::path::Path::new(&session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "do the long task".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .unwrap();
    run_agent(agent_cfg, &mut journal).await.unwrap();

    let events = mo_core::read_events(std::path::Path::new(&session.journal_path)).unwrap();
    let kinds: Vec<&JournalEventKind> = events.iter().map(|e| &e.kind).collect();
    // user, SystemPrompt, ContextUsage{32}, assistant(tool call),
    // ToolCallStart, ToolResult, Handoff, fresh SystemPrompt,
    // MessageDelta (final answer), ContextUsage{5}, final Message. The
    // handoff call itself journals nothing (no deltas, no usage).
    assert_eq!(kinds.len(), 11, "events: {events:#?}");
    assert!(
        matches!(kinds[0], JournalEventKind::Message(m) if m.role == "user" && m.content == "do the long task")
    );
    assert!(matches!(
        kinds[1],
        JournalEventKind::SystemPrompt {
            mode: Mode::Build,
            ..
        }
    ));
    assert_eq!(
        kinds[2],
        &JournalEventKind::ContextUsage {
            tokens: 32,
            context_window: Some(40),
        }
    );
    assert!(
        matches!(kinds[3], JournalEventKind::Message(m) if m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|t| t.len() == 1))
    );
    assert!(
        matches!(kinds[4], JournalEventKind::ToolCallStart { name, .. } if name == "read_file")
    );
    assert!(
        matches!(kinds[5], JournalEventKind::ToolResult { ok: true, output, .. } if output.contains("hello world from notes"))
    );
    // The handoff event carries the model's handoff text + the mode it
    // was generated under.
    assert_eq!(
        kinds[6],
        &JournalEventKind::Handoff {
            content: "HANDOFF: original input ... next step".to_string(),
            mode: Mode::Build,
        }
    );
    // The fresh system prompt anchors the compressed context (per spec:
    // the mode system prompt for the current mode).
    assert!(
        matches!(kinds[7], JournalEventKind::SystemPrompt { content, mode: Mode::Build } if content.contains("Build mode")),
        "expected fresh Build-mode system prompt, got: {:?}",
        kinds[7]
    );
    // The post-compression answer streams normally (deltas journaled
    // for regular calls), then reports the *compressed* context length.
    assert_eq!(
        kinds[8],
        &JournalEventKind::MessageDelta {
            content: "continued after compression".to_string(),
            reasoning_content: None,
        }
    );
    assert_eq!(
        kinds[9],
        &JournalEventKind::ContextUsage {
            tokens: 5,
            context_window: Some(40),
        }
    );
    assert!(
        matches!(kinds[10], JournalEventKind::Message(m) if m.role == "assistant" && m.content.contains("continued after compression"))
    );
}
