//! Light history projection for `GET /api/sessions/:id/history` (the default;
//! `detail=full` serves the complete events).
//!
//! A long journal is dominated by streaming previews (`message_delta` /
//! `tool_output_delta`), by assistant `reasoning_content` and by big
//! `tool_result` outputs (up to the worker's ~1 MB cap). The light form
//! keeps the canonical events, drops superseded previews, strips reasoning
//! from finalized assistant messages (marking them `reasoning_omitted`) and
//! trims long tool outputs to a preview (marking them `output_omitted`).
//! Both are hydrated on demand via `GET /api/sessions/:id/history/:seq`. A
//! delta run that no canonical event finalized yet — the currently
//! streaming message or tool, or the truncated preview of a dead run — is
//! kept verbatim, so live reasoning/output keeps rendering after a reload.
//!
//! Both projections are *run-aware*, because a tool-call id is not unique
//! across a journal: a provider may reuse a sequential id (`call_0`,
//! `call_1`, …) in a later run, and a run that died mid-stream leaves its id
//! behind for the resumed run to repeat. A preview is therefore only dropped
//! when the canonical event that finalizes *its own* run follows it.

use std::collections::HashSet;

use mo_core::{JournalEvent, JournalEventKind};
use serde_json::{Value, json};

/// Tool results longer than this (in chars) are served as a bounded preview
/// in the light history; the complete output is fetched on demand via
/// `GET /api/sessions/:id/history/:seq`.
const TOOL_OUTPUT_LAZY_THRESHOLD: usize = 4_000;
/// How much of an omitted tool output the light history still carries.
const TOOL_OUTPUT_PREVIEW: usize = 1_000;

/// `Some((preview, total_chars))` when a tool output is long enough to be
/// served lazily, `None` when it stays verbatim. The preview is cut with
/// `chars()`, so it is always valid UTF-8.
fn slim_tool_output(output: &str) -> Option<(String, usize)> {
    let total = output.chars().count();
    if total <= TOOL_OUTPUT_LAZY_THRESHOLD {
        return None;
    }
    Some((output.chars().take(TOOL_OUTPUT_PREVIEW).collect(), total))
}

/// Project journal events into the light wire form (see the module docs).
///
/// Each element is the ordinary `JournalEvent` JSON (`{seq, ts, kind: …}`)
/// with these deltas:
/// - superseded `message_delta` / `tool_output_delta` events are absent;
/// - a `message` event whose `reasoning_content` was non-empty loses that
///   field and gains `reasoning_omitted: true`;
/// - a `tool_result` event whose `output` is longer than
///   `TOOL_OUTPUT_LAZY_THRESHOLD` keeps only a `TOOL_OUTPUT_PREVIEW`-char
///   prefix and gains `output_omitted: true` + `output_chars`;
/// - everything else is serialized verbatim.
pub fn light_history(events: Vec<JournalEvent>) -> Vec<Value> {
    let superseded_messages = superseded_message_deltas(&events);
    let superseded_tools = superseded_tool_output_deltas(&events);

    events
        .into_iter()
        .enumerate()
        .filter_map(|(i, event)| match &event.kind {
            JournalEventKind::MessageDelta { .. } if superseded_messages[i] => None,
            JournalEventKind::ToolOutputDelta { .. } if superseded_tools[i] => None,
            JournalEventKind::Message(m)
                if m.reasoning_content
                    .as_deref()
                    .is_some_and(|reasoning| !reasoning.is_empty()) =>
            {
                let mut value = to_json(&event);
                if let Some(kind) = value.get_mut("kind").and_then(Value::as_object_mut) {
                    kind.remove("reasoning_content");
                    kind.insert("reasoning_omitted".to_string(), json!(true));
                }
                Some(value)
            }
            JournalEventKind::ToolResult { output, .. } => match slim_tool_output(output) {
                Some((preview, total)) => {
                    let mut value = to_json(&event);
                    if let Some(kind) = value.get_mut("kind").and_then(Value::as_object_mut) {
                        kind.insert("output".to_string(), json!(preview));
                        kind.insert("output_omitted".to_string(), json!(true));
                        kind.insert("output_chars".to_string(), json!(total));
                    }
                    Some(value)
                }
                None => Some(to_json(&event)),
            },
            _ => Some(to_json(&event)),
        })
        .collect()
}

/// Serialize one journal event for the history array.
///
/// `JournalEvent` only holds strings, integers, booleans, options and vecs
/// (no non-finite floats, no non-string map keys), so serialization cannot
/// fail. A lossy fallback would be worse than a panic: a `null` element
/// makes the frontend's `event.kind.kind` mapping throw and takes the whole
/// history load down, silently hiding the rest of the conversation.
fn to_json(event: &JournalEvent) -> Value {
    serde_json::to_value(event).expect("a JournalEvent is always JSON-serializable")
}

/// Indices of `message_delta` events superseded by a canonical assistant
/// `message` (whose assembled content the preview duplicates).
///
/// A maximal run of `message_delta`s — surviving interleaved `context_usage`
/// events, which the worker journals *between* the deltas and the final
/// message of the same turn — is superseded iff the next non-`context_usage`
/// event is a `message` with role `assistant` (or the empty role of legacy
/// journals, normalized to assistant by the worker). A run closed by any
/// other event (a user message, tool events, a status change, the end of the
/// journal) is *unfinalized* — a truncated preview of a dead run or the
/// currently streaming message — and keeps its deltas.
fn superseded_message_deltas(events: &[JournalEvent]) -> Vec<bool> {
    let mut superseded = vec![false; events.len()];
    let mut run: Vec<usize> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        match &event.kind {
            JournalEventKind::MessageDelta { .. } => run.push(i),
            JournalEventKind::ContextUsage { .. } => {}
            JournalEventKind::Message(m) if m.role == "assistant" || m.role.is_empty() => {
                for &j in &run {
                    superseded[j] = true;
                }
                run.clear();
            }
            _ => run.clear(), // unfinalized run: kept verbatim
        }
    }
    superseded
}

/// Indices of `tool_output_delta` events superseded by the canonical
/// `tool_result` of **their own** tool run.
///
/// Tool-call ids are not unique across a journal: providers that number their
/// calls per response (`call_0`, `call_1`, …) reuse the ids of earlier runs,
/// and a run that died mid-stream leaves its id for the resumed run to
/// repeat. Dropping by id membership would therefore also drop a dead run's
/// truncated preview — permanently, because those events sit below the SSE
/// `after_seq` cursor — or the live preview of a tool that reuses an earlier
/// run's id. So a delta is superseded iff the nearest id-matching event after
/// it is a `tool_result`; a `tool_call_start` with the same id in between
/// proves that result finalizes a *different* run.
///
/// Scanned backwards so each decision is a set lookup: going right-to-left,
/// `finalized` holds the ids whose still-unclaimed result lies ahead.
fn superseded_tool_output_deltas(events: &[JournalEvent]) -> Vec<bool> {
    let mut superseded = vec![false; events.len()];
    let mut finalized: HashSet<String> = HashSet::new();
    for (i, event) in events.iter().enumerate().rev() {
        match &event.kind {
            JournalEventKind::ToolResult { id, .. } => {
                finalized.insert(id.clone());
            }
            // A new run of this id starts here: any earlier delta belongs to
            // it, not to the result that lies further ahead.
            JournalEventKind::ToolCallStart { id, .. } => {
                finalized.remove(id);
            }
            JournalEventKind::ToolOutputDelta { id, .. } => {
                superseded[i] = finalized.contains(id);
            }
            _ => {}
        }
    }
    superseded
}

// Unit tests live in `mo_gateway/src/tests/history_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/history_tests.rs"]
mod tests;
