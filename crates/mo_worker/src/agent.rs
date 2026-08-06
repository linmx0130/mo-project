//! The agent loop: stream a chat completion, journal the assistant message,
//! execute any tool calls, feed results back, and repeat until the model
//! produces a final answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, pin_mut};
use mo_core::{JournalEventKind, JournalMessage, JournalWriter, Session, ToolCallInfo};
use nah_chat::{
    ChatClient, ChatCompletionParamsBuilder, ChatCompletionStreamEvent, ChatMessage,
    ChatMessageContentValue, FunctionCallRequest, ToolCallRequest,
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
    /// The model's context window in tokens (from `mo.toml`), embedded in
    /// each `ContextUsage` journal event; `None` = unlimited.
    pub context_window: Option<u64>,
    pub subagent_depth: u32,
    /// Max number of tool calls from a single assistant message that
    /// execute concurrently (from `mo.toml`'s `max_tool_concurrency`;
    /// clamped to at least 1).
    pub max_tool_concurrency: usize,
    /// The fraction of the model's `context_window` at which the worker
    /// asks the model to generate a handoff prompt and starts sending only
    /// the compressed context (from `mo.toml`'s
    /// `context_compression_threshold`, default 0.75). Only applies when
    /// `context_window` is set.
    pub context_compression_threshold: f64,
}

/// Run the full agent loop for one session, journaling as it goes.
/// The caller is responsible for DB status transitions.
pub async fn run_agent(config: AgentConfig, journal: &mut JournalWriter) -> Result<()> {
    let chat_client = ChatClient::init(config.model_base_url.clone(), config.auth_token.clone());

    // The session scratch dir: non-Build modes may create/edit/remove files
    // here while the codebase stays read-only. Created up front so its path
    // is real when the system prompt (which mentions it) is journaled.
    let scratch = config
        .data_dir
        .join("sessions")
        .join(&config.session.id)
        .join("tmp");
    std::fs::create_dir_all(&scratch).context("failed to create session scratch dir")?;
    let scratch = scratch
        .canonicalize()
        .context("failed to resolve session scratch dir")?;

    // The conversation context is the journal history: user messages are
    // journaled by the gateway before the worker is spawned, and assistant /
    // tool messages by previous runs of this session. The system prompt is
    // journaled on the first run (as a `SystemPrompt` event) and reused
    // verbatim on every later run — never rebuilt, so mid-session changes
    // to `AGENTS.md`, skills or the mode never invalidate it (and the same
    // system text is sent to the LLM on every run, which is prompt-cache
    // friendly). Context compression is the one exception: after a
    // `Handoff` event the history rebuild returns the *fresh* system prompt
    // the worker journaled right after it, and drops everything before the
    // handoff from the model context.
    //
    // `last_usage` is the context length of the last LLM call (seeded from
    // the journal so a resumed session that already crossed the compression
    // threshold compresses before its first call); the compression gate
    // checks it at the top of the loop, *before* the next LLM call.
    let (journaled_system, mut messages, mut last_usage) =
        history_from_journal(Path::new(&config.session.journal_path))?;
    let system_prompt = match journaled_system {
        Some(prompt) => prompt,
        None => {
            let prompt = build_system_prompt(
                &config.workdir,
                &config.agents_dir,
                config.subagent_depth,
                config.session.mode,
                &scratch,
            );
            journal.append(JournalEventKind::SystemPrompt {
                content: prompt.clone(),
                mode: config.session.mode,
            })?;
            prompt
        }
    };
    messages.insert(0, system_message(system_prompt));
    let tools = tools::tool_definitions();
    let tool_ctx = ToolContext {
        workdir: config.workdir.clone(),
        data_dir: config.data_dir.clone(),
        agents_dir: config.agents_dir.clone(),
        session: config.session.clone(),
        scratch: scratch.clone(),
        subagent_depth: config.subagent_depth,
        max_tool_concurrency: config.max_tool_concurrency,
        model_base_url: config.model_base_url.clone(),
        model_name: config.model_name.clone(),
        auth_token: config.auth_token.clone(),
        context_window: config.context_window,
        context_compression_threshold: config.context_compression_threshold,
    };

    // Compression is attempted once per "epoch" (until it succeeds): a
    // failed handoff generation is not retried every iteration, because the
    // endpoint is presumably down and the next regular `generate` will fail
    // with its own backoff anyway.
    let mut compression_attempted = false;

    loop {
        // Context-compression gate, before the next LLM call. `last_usage`
        // is the previous call's prompt-token count (which includes the
        // system prompt + history + tool outputs up to that call), so the
        // call that crosses the threshold has already completed comfortably
        // under it and the handoff generation still has headroom.
        if let (Some(window), Some(tokens)) = (config.context_window, last_usage)
            && window > 0
            && (tokens as f64 / window as f64) >= config.context_compression_threshold
            && !compression_attempted
        {
            match generate_handoff(&chat_client, &config.model_name, &messages, journal).await {
                Ok(text) => {
                    // The handoff prompt, journaled as the compression
                    // boundary: everything before it is dropped from the
                    // model context on future rebuilds (it stays visible in
                    // the journal/UI), and a fresh system prompt built from
                    // the current mode anchors the compressed "session"
                    // (the mode system prompt per spec).
                    let new_system = build_system_prompt(
                        &config.workdir,
                        &config.agents_dir,
                        config.subagent_depth,
                        config.session.mode,
                        &scratch,
                    );
                    journal.append(JournalEventKind::Handoff {
                        content: text,
                        mode: config.session.mode,
                    })?;
                    journal.append(JournalEventKind::SystemPrompt {
                        content: new_system,
                        mode: config.session.mode,
                    })?;
                    tracing::info!(
                        session = %config.session.id,
                        "context compressed: journaled a handoff prompt; earlier events will no longer be sent to the model"
                    );
                    // Rebuild the compressed context for the next call: the
                    // fresh system prompt + the handoff as the first user
                    // message (the pre-handoff events are gone from the
                    // model context). The next `generate` reports the
                    // compressed usage, so `last_usage` resets here.
                    let (fresh_system, msgs, _) =
                        history_from_journal(Path::new(&config.session.journal_path))?;
                    messages = msgs;
                    messages.insert(
                        0,
                        system_message(
                            fresh_system.expect("the fresh SystemPrompt was just journaled"),
                        ),
                    );
                    last_usage = None;
                    compression_attempted = false;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(session = %config.session.id, "context compression failed: {e:#}");
                    compression_attempted = true;
                }
            }
        }

        // `generate` streams the completion and journals a `MessageDelta`
        // event per content chunk as it arrives; the final `Message` event
        // (full assembled text) is journaled right after, so readers see
        // tokens arrive live and then settle on the canonical message.
        let (assistant, prompt_tokens) =
            generate(&chat_client, &config.model_name, &messages, &tools, journal)
                .await
                .context("LLM generation failed after retries")?;
        // The API reported the tokens this call consumed (system prompt +
        // history + tool outputs). Journal them as the session's current
        // context length; the status bar shows the latest value. Skipped
        // when the provider did not report usage (ignored `include_usage`).
        if let Some(tokens) = prompt_tokens {
            last_usage = Some(tokens);
            journal.append(JournalEventKind::ContextUsage {
                tokens,
                context_window: config.context_window,
            })?;
        }
        journal.append(JournalEventKind::Message(journal_message_from(&assistant)))?;

        let Some(tool_calls) = assistant.tool_calls.clone() else {
            messages.push(assistant);
            return Ok(());
        };

        // Execute the message's tool calls CONCURRENTLY, bounded by
        // `max_tool_concurrency` (from `mo.toml`):
        //   1. Every `ToolCallStart` event is journaled first, so the UI
        //      shows all tool blocks at once.
        //   2. The calls run in parallel; each `ToolResult` is journaled as
        //      its call completes, so the UI updates blocks independently
        //      and some finish before others.
        //   3. The model-facing tool messages are rebuilt in the ORIGINAL
        //      call order (results looked up by call id), so the model sees
        //      a deterministic context regardless of completion order.
        let mut tool_messages = Vec::with_capacity(tool_calls.len());
        if !tool_calls.is_empty() {
            for tc in &tool_calls {
                journal.append(JournalEventKind::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                })?;
            }

            // Shared journal sink: the concurrent tool calls append their
            // events (bash `ToolOutputDelta`s, `ModeChangeRequest`,
            // `SubagentStarted`, `ToolResult`) through this closure, which
            // locks the writer per event. Appends are short synchronous
            // write+flush sections — never held across an await — and `seq`
            // stays monotonic because assignment happens under the lock.
            let sink = {
                let shared = Arc::new(Mutex::new(&mut *journal));
                move |kind: JournalEventKind| {
                    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = guard.append(kind);
                }
            };

            // `buffer_unordered(n)` polls up to `n` tool futures at once;
            // per-call timeouts (bash's 120s) only start once a call is
            // actually polled. Each future journals its own `ToolResult` as
            // it completes.
            let results: Vec<(String, String)> = futures_util::stream::iter(tool_calls.iter())
                .map(|tc| async {
                    let (ok, output) = match tools::execute_tool(
                        &tool_ctx,
                        &tc.function.name,
                        &tc.function.arguments,
                        &tc.id,
                        &sink,
                    )
                    .await
                    {
                        Ok(output) => (true, output),
                        Err(err) => (false, err),
                    };
                    sink(JournalEventKind::ToolResult {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        ok,
                        output: output.clone(),
                    });
                    (tc.id.clone(), output)
                })
                .buffer_unordered(config.max_tool_concurrency.max(1))
                .collect()
                .await;
            let by_id: HashMap<&str, &str> = results
                .iter()
                .map(|(id, output)| (id.as_str(), output.as_str()))
                .collect();
            for tc in &tool_calls {
                let output = by_id
                    .get(tc.id.as_str())
                    .copied()
                    .unwrap_or("[tool call failed to produce a result]");
                tool_messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: ChatMessageContentValue::Text(output.to_string()),
                    reasoning_content: None,
                    tool_call_id: Some(tc.id.clone()),
                    tool_calls: None,
                });
            }
        }
        messages.push(assistant);
        messages.extend(tool_messages);
    }
}

/// Rebuild the chat context from a session journal, returning the journaled
/// system prompt (if any), the chat messages, and the context length of the
/// last `context_usage` event (the status bar's latest value, and the seed
/// for the compression gate on a resumed run).
///
/// `message` events map directly to user/assistant messages (tool calls
/// included); `tool_result` events become the `tool`-role messages that must
/// follow an assistant tool call. The journal interleaves them in the
/// correct order, so a completed session can be resumed exactly where it
/// left off. Tool results are journaled in *completion* order (tool calls
/// in one message run concurrently), so the tool messages following an
/// assistant `tool_calls` message are reordered to match the calls' array
/// order — the model-facing context is deterministic regardless of which
/// call finished first.
///
/// A worker that died or was stopped mid-tool leaves the journal ending
/// with an assistant `tool_calls` message that no `tool_result` ever
/// answered. Such a message is dropped here (together with any partial tool
/// results that followed it), because the model API rejects `tool_calls`
/// without matching tool messages — a followup on a killed session would
/// otherwise fail with "insufficient tool messages following tool_calls".
///
/// Context compression: the *last* `Handoff` event marks the compression
/// boundary. Every event with a `seq` below it is folded into the handoff
/// and dropped from the model context (it stays in the journal for the
/// user); the handoff text becomes the first user message, and the fresh
/// `SystemPrompt` event the worker journaled right after it becomes the
/// system prompt. `last_prompt_tokens` is the last `context_usage` at or
/// after the boundary — after a compression that is the *compressed* count,
/// so a resumed compressed session does not re-trigger the gate.
fn history_from_journal(
    journal_path: &Path,
) -> Result<(Option<String>, Vec<ChatMessage>, Option<u64>)> {
    let events = mo_core::read_events(journal_path).context("failed to read session journal")?;
    let boundary = events
        .iter()
        .rev()
        .find_map(|e| matches!(&e.kind, JournalEventKind::Handoff { .. }).then_some(e.seq));
    let mut system_prompt: Option<String> = None;
    let mut messages = Vec::new();
    let mut last_prompt_tokens: Option<u64> = None;
    for event in events {
        if boundary.is_some_and(|b| event.seq < b) {
            // Before the last handoff: folded into the handoff prompt, not
            // sent to the model anymore (still in the journal/UI).
            continue;
        }
        match event.kind {
            JournalEventKind::SystemPrompt { content, .. } => {
                // Session metadata, not a chat message; the caller prepends
                // it as the system message.
                system_prompt = Some(content);
            }
            JournalEventKind::Handoff { content, .. } => {
                // The handoff prompt is the compressed context's first user
                // message, wrapped in a marker so the model understands it
                // replaces the earlier history.
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContentValue::Text(format!(
                        "{}{}",
                        crate::prompt::HANDOFF_USER_PREFIX,
                        content
                    )),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            JournalEventKind::ModeChange { content, .. } => {
                // The mode-change notice injected by the gateway right
                // before a followup user message. It is passed through as a
                // user-role message (safe across providers), so the model
                // sees it directly before the real user message and does not
                // keep the stale framing of the journaled system prompt.
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContentValue::Text(content),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
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
            JournalEventKind::ContextUsage { tokens, .. } => {
                // The session's current context length (status bar + the
                // compression gate's seed on a resumed run).
                last_prompt_tokens = Some(tokens);
            }
            _ => {}
        }
    }
    // Drop dangling tool-call messages (and the partial tool results that
    // followed them) so the rebuilt context never carries `tool_calls`
    // without matching tool messages — and, for answered calls, reorder the
    // tool messages that follow to match the assistant's `tool_calls` array
    // order (the journal records parallel results in completion order, which
    // may vary; the model-facing context must be deterministic).
    let mut i = 0;
    let mut cleaned = Vec::with_capacity(messages.len());
    while i < messages.len() {
        if messages[i].role == "assistant" && !tool_calls_answered(&messages, i) {
            i += 1;
            while i < messages.len() && messages[i].role == "tool" {
                i += 1;
            }
            continue;
        }
        if messages[i].role == "assistant"
            && messages[i]
                .tool_calls
                .as_ref()
                .is_some_and(|c| !c.is_empty())
        {
            let calls = messages[i].tool_calls.clone().expect("checked above");
            let mut run: Vec<ChatMessage> = Vec::new();
            let mut j = i + 1;
            while j < messages.len() && messages[j].role == "tool" {
                run.push(messages[j].clone());
                j += 1;
            }
            // Deterministic order: by the index of each tool message's call
            // id within the assistant's tool_calls array; unknown ids sink
            // last (they should not occur for answered calls).
            run.sort_by_key(|m| {
                calls
                    .iter()
                    .position(|c| c.id == m.tool_call_id.as_deref().unwrap_or(""))
                    .unwrap_or(usize::MAX)
            });
            cleaned.push(messages[i].clone());
            cleaned.extend(run);
            i = j;
        } else {
            cleaned.push(messages[i].clone());
            i += 1;
        }
    }
    Ok((system_prompt, cleaned, last_prompt_tokens))
}

/// Build a system-role chat message (the standard shape used everywhere the
/// worker prepends the session's system prompt to the model context).
fn system_message(content: String) -> ChatMessage {
    ChatMessage {
        role: "system".to_string(),
        content: ChatMessageContentValue::Text(content),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// The context-compression handoff call: one extra LLM request that asks
/// the model to summarize the whole conversation into a handoff prompt (see
/// `crate::prompt::handoff_instruction`), with retry/backoff like a regular
/// call. The request carries no tools (a plain-text answer is wanted) and
/// the streamed deltas are *not* journaled — this is an internal call; only
/// the final `Handoff` event surfaces in the journal/UI.
async fn generate_handoff(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    journal: &mut JournalWriter,
) -> Result<String> {
    let mut ctx = messages.to_vec();
    ctx.push(ChatMessage {
        role: "user".to_string(),
        content: ChatMessageContentValue::Text(crate::prompt::handoff_instruction()),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    });
    let (message, _) = generate_with_retry(chat_client, model, &ctx, &[], journal, false).await?;
    Ok(message.content.to_string().trim().to_string())
}

/// True when the assistant message at `idx` had every tool call answered by
/// the immediately following tool messages (one message per call id, in any
/// order). False when the journal ends mid-tool-call — the worker died or
/// was stopped before the results landed.
fn tool_calls_answered(messages: &[ChatMessage], idx: usize) -> bool {
    let calls = match &messages[idx].tool_calls {
        Some(calls) if !calls.is_empty() => calls,
        _ => return true,
    };
    let mut remaining: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
    for m in &messages[idx + 1..] {
        if m.role != "tool" {
            break;
        }
        if let Some(id) = m.tool_call_id.as_deref()
            && let Some(pos) = remaining.iter().position(|r| *r == id)
        {
            remaining.remove(pos);
        }
        if remaining.is_empty() {
            return true;
        }
    }
    false
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
///
/// Returns the assembled assistant message plus the prompt-token count the
/// API reported for the call (`None` when the provider did not report
/// usage, e.g. it ignored `stream_options.include_usage`).
async fn generate(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    tools: &[Value],
    journal: &mut JournalWriter,
) -> Result<(ChatMessage, Option<u64>)> {
    generate_with_retry(chat_client, model, messages, tools, journal, true).await
}

/// Shared retry/backoff wrapper for regular calls and the internal handoff
/// call. `journal_deltas` controls whether streamed chunks become
/// `MessageDelta` events — the handoff call keeps the journal clean.
async fn generate_with_retry(
    chat_client: &ChatClient,
    model: &str,
    messages: &[ChatMessage],
    tools: &[Value],
    journal: &mut JournalWriter,
    journal_deltas: bool,
) -> Result<(ChatMessage, Option<u64>)> {
    let mut last_error: Option<anyhow::Error> = None;
    for (attempt, delay) in RETRY_DELAYS_SECS.iter().enumerate() {
        match generate_once(chat_client, model, messages, tools, journal, journal_deltas).await {
            Ok(result) => return Ok(result),
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
    journal_deltas: bool,
) -> Result<(ChatMessage, Option<u64>)> {
    let mut params = ChatCompletionParamsBuilder::new();
    // No hard `max_tokens` cap: reasoning models may spend arbitrary tokens
    // on `reasoning_content` before the actual answer, and a cap truncates
    // the stream before any content arrives. Let the model run until it
    // finishes on its own.
    params.temperature(0.7);
    // Tool definitions are only advertised on the regular loop calls; the
    // internal handoff call sends none (a plain-text answer is wanted).
    if !tools.is_empty() {
        params.insert("tools", json!(tools));
    }
    // Ask the server to report the call's token usage in the final stream
    // chunk (`ChatCompletionStreamEvent::Usage` right before `[DONE]`); the
    // prompt tokens become the session's context length in the status bar.
    params.include_usage();
    let stream = chat_client
        .chat_completion_stream(model, messages, &params)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start chat completion stream: {e}"))?;
    pin_mut!(stream);
    let mut message = ChatMessage::new();
    let mut prompt_tokens: Option<u64> = None;
    while let Some(event) = stream.next().await {
        match event.map_err(|e| anyhow::anyhow!("error in stream delta: {e}"))? {
            ChatCompletionStreamEvent::Delta(delta) => {
                // Journal every chunk that carries visible text or reasoning,
                // so both stream token-by-token (reasoning models emit
                // `reasoning_content` deltas first, then `content` deltas).
                // Skipped for the internal handoff call.
                let content = delta.content.clone().unwrap_or_default();
                let reasoning_content = delta.reasoning_content.clone().filter(|s| !s.is_empty());
                if journal_deltas && (!content.is_empty() || reasoning_content.is_some()) {
                    journal.append(JournalEventKind::MessageDelta {
                        content,
                        reasoning_content,
                    })?;
                }
                message.apply_model_response_chunk(delta);
            }
            ChatCompletionStreamEvent::Usage(usage) => {
                // The final chunk reports the whole call's usage; keep the
                // last one seen (some proxies send it earlier).
                prompt_tokens = usage.prompt_tokens;
            }
        }
    }
    let empty = message.role.is_empty()
        && message.content.to_string().is_empty()
        && message.tool_calls.is_none();
    if empty {
        bail!("model returned an empty response");
    }
    if prompt_tokens.is_none() {
        // `include_usage` was requested but the server never sent a `Usage`
        // event — either the endpoint ignores `stream_options` or reports
        // usage in a shape nah_chat cannot parse. The session keeps working;
        // only the status bar's context length is missing. Surfaces in
        // `data/sessions/<id>/worker.log`.
        tracing::warn!(
            model,
            "LLM API did not report token usage in the stream (stream_options.include_usage was requested); context length will be unavailable"
        );
    }
    Ok((message, prompt_tokens))
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
    fn sse_payload_with_usage(
        deltas: &[Value],
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> String {
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
                                    delta_tool_call(
                                        0,
                                        "call_1",
                                        "read_file",
                                        r#"{"path":"a.txt"}"#,
                                    ),
                                    delta_tool_call(
                                        1,
                                        "call_2",
                                        "read_file",
                                        r#"{"path":"b.txt"}"#,
                                    ),
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
}
