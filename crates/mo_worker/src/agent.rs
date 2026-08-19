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

#[derive(Clone)]
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

    // Complete any held permission batch the user answered while this
    // session was terminal: journal the held calls' real outcomes *before*
    // the history rebuild, so the resumed model context carries them as
    // ordinary tool results (allowed calls return their actual result,
    // denied calls a denial error) — the permission flow stays invisible to
    // the model. Idempotent: a session with no answered batch does nothing.
    resume_held_permission_calls(&tool_ctx, journal).await?;

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
                &config.session.skills,
            );
            journal.append(JournalEventKind::SystemPrompt {
                content: prompt.clone(),
                mode: config.session.mode,
                model: config.model_name.clone(),
            })?;
            prompt
        }
    };
    messages.insert(0, system_message(system_prompt));
    // The session's enabled tool set (from the "New session" form): the
    // schemas of disabled tools are filtered out, so the model can only
    // call what the user allowed (see `tools::tool_definitions`). The
    // fixed tools (bash + file operations) are always included; an empty
    // list is the legacy "all tools" default.
    let tools = tools::tool_definitions(&config.session.tools);

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
                    // (the mode system prompt per spec). The forced skills
                    // are re-inlined too — the compressed session keeps the
                    // user's loaded skills.
                    let new_system = build_system_prompt(
                        &config.workdir,
                        &config.agents_dir,
                        config.subagent_depth,
                        config.session.mode,
                        &scratch,
                        &config.session.skills,
                    );
                    journal.append(JournalEventKind::Handoff {
                        content: text,
                        mode: config.session.mode,
                    })?;
                    journal.append(JournalEventKind::SystemPrompt {
                        content: new_system,
                        mode: config.session.mode,
                        model: config.model_name.clone(),
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

            // Pre-flight the message's calls: those targeting paths outside
            // the allowed roots are *held* for a single batched permission
            // request — one card in the UI listing every path, instead of a
            // per-call request with "already pending" errors for the
            // siblings. Nothing is sent to the LLM until the user decides:
            // the run ends right after the request is journaled, and the
            // held calls' real outcomes are delivered on the resumed run
            // (see `resume_held_permission_calls`). The calls that need no
            // permission still execute concurrently below — their results
            // are journaled (the UI shows them) but not fed to the model.
            let held: Vec<mo_core::PermissionRequestItem> = tool_calls
                .iter()
                .filter_map(|tc| {
                    match tools::preflight(
                        &tool_ctx,
                        &tc.function.name,
                        &tc.function.arguments,
                        &tc.id,
                    ) {
                        tools::Preflight::Permission(item) => Some(item),
                        _ => None,
                    }
                })
                .collect();
            let hold = if held.is_empty() {
                false
            } else {
                match tools::permission::ask_permission_batch(&tool_ctx, &held, &sink) {
                    Ok(()) => true,
                    Err(e) => {
                        // Only reachable for subagents (their journal has no
                        // UI) or a pathological pending conflict: fall back
                        // to normal execution — the per-call
                        // `ask_permission` then surfaces the refusal as an
                        // ordinary tool error.
                        tracing::warn!(
                            session = %config.session.id,
                            "permission batch refused, falling back to per-call execution: {e}"
                        );
                        false
                    }
                }
            };
            let held_ids: std::collections::HashSet<&str> =
                held.iter().map(|i| i.call_id.as_str()).collect();
            let to_run: Vec<&ToolCallRequest> = tool_calls
                .iter()
                .filter(|tc| !(hold && held_ids.contains(tc.id.as_str())))
                .collect();

            // `buffer_unordered(n)` polls up to `n` tool futures at once;
            // per-call timeouts (bash's 120s) only start once a call is
            // actually polled. Each future journals its own `ToolResult` as
            // it completes.
            let results: Vec<(String, String)> = futures_util::stream::iter(to_run.iter())
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

            if hold {
                // The user must decide first: end the run without feeding
                // anything back to the model. The safe calls' results are
                // already in the journal (the UI shows them); the held
                // calls' outcomes land here on the resumed run, before the
                // history rebuild.
                return Ok(());
            }

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

/// Complete any *held* permission batch whose answer landed while the
/// session was terminal: re-run the batch's file-tool calls and journal
/// their `ToolResult`s *before* the history rebuild, so the resumed model
/// context carries them as ordinary tool results — allowed calls return
/// their real outcome (file content, edit confirmation, ...), denied calls
/// return the remembered-denial error. This is what makes the permission
/// flow transparent to the model: it never sees the request, only the
/// results, and never has to "retry" the calls itself.
///
/// Re-running goes through `execute_tool` with the item's original
/// arguments, so the remembered `(tool, path)` decisions (journaled by the
/// answer) route each call: allowed → runs with the path in the approved
/// list; denied → the policy refuses with the denial error. Idempotent: an
/// item whose call id already has a `ToolResult` after the answer is
/// skipped, and a session with no answered batch does nothing. The held
/// calls are always file tools, so the no-op event sink is safe (they emit
/// no non-result journal events).
async fn resume_held_permission_calls(
    ctx: &ToolContext,
    journal: &mut JournalWriter,
) -> Result<()> {
    let events = mo_core::read_events(Path::new(&ctx.session.journal_path))
        .context("failed to read session journal")?;
    // The most recent batched answer and the request it resolves.
    let Some((answered_seq, request_id)) = events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::PermissionAnswered {
            request_id,
            decisions,
            ..
        } if !decisions.is_empty() => Some((e.seq, request_id.clone())),
        _ => None,
    }) else {
        return Ok(());
    };
    let Some(request_items) = events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::PermissionRequest {
            request_id: rid,
            items,
            ..
        } if rid == &request_id && !items.is_empty() && e.seq < answered_seq => Some(items.clone()),
        _ => None,
    }) else {
        return Ok(());
    };
    let noop = |_: JournalEventKind| {};
    for item in &request_items {
        // Skip items already executed (a prior resume finished them, or a
        // safe sibling that happened to share the call id — defense in
        // depth for the mock-replay case).
        let has_result = events.iter().any(|e| {
            matches!(&e.kind, JournalEventKind::ToolResult { id, .. } if id == &item.call_id && e.seq > answered_seq)
        });
        if has_result {
            continue;
        }
        let (ok, output) =
            match tools::execute_tool(ctx, &item.tool, &item.arguments, &item.call_id, &noop).await
            {
                Ok(output) => (true, output),
                Err(err) => (false, err),
            };
        tracing::debug!(
            session = %ctx.session.id,
            call = %item.call_id,
            ok,
            "resumed held permission call"
        );
        journal.append(JournalEventKind::ToolResult {
            id: item.call_id.clone(),
            name: item.tool.clone(),
            ok,
            output,
        })?;
    }
    Ok(())
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
            JournalEventKind::AskUserAnswered { answers } => {
                // The user answered the clarification question the model
                // asked via the `ask_user` tool. Synthesized as a user-role
                // message carrying the answers as a JSON object keyed by
                // question_id — the tool's "return value" to the model —
                // exactly like the ModeChange notice above. (`AskUserRequest`
                // itself is flow metadata and falls into the default skip.)
                let json = serde_json::to_string_pretty(&answers).unwrap_or_default();
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContentValue::Text(format!(
                        "{}{}",
                        crate::prompt::ASK_USER_ANSWER_PREFIX,
                        json
                    )),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            JournalEventKind::PermissionAnswered {
                tool: Some(tool),
                operation: Some(operation),
                path: Some(path),
                allowed: Some(allowed),
                ..
            } => {
                // Legacy (pre-batch) shape: the user decided a single
                // file-access permission request the harness showed them.
                // Synthesized as a user-role message carrying the decision —
                // the tool's "return value" to the model — so the model
                // retries the tool call when allowed or finds another way
                // when denied. Kept exactly as before so journals written
                // before batching resume correctly. Batched answers
                // (`decisions`) are NOT synthesized here: the resume
                // delivers the held calls' real outcomes as ordinary tool
                // results (see `resume_held_permission_calls`), so a
                // decision message would be redundant. (`PermissionRequest`
                // itself is flow metadata and falls into the default skip.)
                let verdict = if allowed { "allowed" } else { "denied" };
                let followup = if allowed {
                    " Retry the tool call with the same arguments."
                } else {
                    " Do not retry this exact request; find another way or \
                     explain why you cannot proceed."
                };
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContentValue::Text(format!(
                        "{}{} the request {tool} ({operation}) on \"{path}\".{followup}",
                        crate::prompt::PERMISSION_ANSWER_PREFIX,
                        verdict
                    )),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            JournalEventKind::Message(m) => {
                // Normalize an empty role to "assistant": journals written
                // before the role-defaulting fix may carry messages whose
                // provider never streamed a `role` field. An empty role is
                // rejected by strict OpenAI-compatible endpoints (HTTP 400
                // "Invalid provider request") when the history is re-sent —
                // which is exactly what happens on a followup / resumed
                // session — so the rebuild must not reproduce it.
                let role = if m.role.is_empty() {
                    "assistant".to_string()
                } else {
                    m.role
                };
                messages.push(ChatMessage {
                    role,
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
                })
            }
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
    // Some providers never send a `role` field in their streamed deltas
    // (e.g. the `dots3-note-prev` proxy), so the assembled message's role
    // stays empty. A completion is an assistant message by definition, and
    // an empty role is rejected by strict OpenAI-compatible endpoints when
    // the message is echoed back in a later request (HTTP 400 "Invalid
    // provider request") — normalize it before the message is journaled
    // and fed back.
    if message.role.is_empty() {
        message.role = "assistant".to_string();
    }
    let empty = message.content.to_string().is_empty() && message.tool_calls.is_none();
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

// Unit tests live in `mo_worker/src/tests/agent_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/agent_tests.rs"]
mod tests;
