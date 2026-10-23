//! Light history projection for `GET /api/sessions/:id/history` (the default;
//! `detail=full` serves the complete events).
//!
//! A long journal is dominated by streaming previews (`message_delta` /
//! `tool_output_delta`) and by assistant `reasoning_content`. The light form
//! keeps the canonical events, drops superseded previews, and strips
//! reasoning from finalized assistant messages (marking them
//! `reasoning_omitted` so the UI can offer lazy per-message hydration via
//! `GET /api/sessions/:id/messages/:seq`). A delta run that no canonical
//! event finalized yet — the currently streaming message, or the truncated
//! preview of a dead run — is kept verbatim, so live reasoning keeps
//! rendering after a reload.

use std::collections::HashSet;

use mo_core::{JournalEvent, JournalEventKind};
use serde_json::{Value, json};

/// Project journal events into the light wire form (see the module docs).
///
/// Each element is the ordinary `JournalEvent` JSON (`{seq, ts, kind: …}`)
/// with these deltas:
/// - superseded `message_delta` / `tool_output_delta` events are absent;
/// - a `message` event whose `reasoning_content` was non-empty loses that
///   field and gains `reasoning_omitted: true`;
/// - everything else is serialized verbatim.
pub fn light_history(events: Vec<JournalEvent>) -> Vec<Value> {
    let superseded_runs = superseded_message_deltas(&events);
    let result_ids: HashSet<String> = events
        .iter()
        .filter_map(|event| match &event.kind {
            JournalEventKind::ToolResult { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();

    events
        .into_iter()
        .enumerate()
        .filter_map(|(i, event)| match &event.kind {
            JournalEventKind::MessageDelta { .. } if superseded_runs[i] => None,
            JournalEventKind::ToolOutputDelta { id, .. } if result_ids.contains(id.as_str()) => {
                None
            }
            JournalEventKind::Message(m)
                if m.reasoning_content
                    .as_deref()
                    .is_some_and(|reasoning| !reasoning.is_empty()) =>
            {
                let mut value = serde_json::to_value(&event).unwrap_or(Value::Null);
                if let Some(kind) = value.get_mut("kind").and_then(Value::as_object_mut) {
                    kind.remove("reasoning_content");
                    kind.insert("reasoning_omitted".to_string(), json!(true));
                }
                Some(value)
            }
            _ => Some(serde_json::to_value(&event).unwrap_or(Value::Null)),
        })
        .collect()
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

// Unit tests live in `mo_gateway/src/tests/history_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/history_tests.rs"]
mod tests;
