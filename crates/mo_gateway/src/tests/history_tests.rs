//! Unit tests for the `history` module — production code lives in
//! `mo_gateway/src/history.rs`. Wired from there with `#[cfg(test)] #[path = "tests/history_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use mo_core::{JournalEvent, JournalEventKind, JournalMessage};
use serde_json::json;

fn ev(seq: u64, kind: JournalEventKind) -> JournalEvent {
    JournalEvent {
        seq,
        ts: chrono::Utc::now(),
        kind,
    }
}

fn msg(role: &str, content: &str, reasoning: Option<&str>) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: role.to_string(),
        content: content.to_string(),
        reasoning_content: reasoning.map(String::from),
        tool_call_id: None,
        tool_calls: None,
        images: Vec::new(),
    })
}

fn delta(content: &str, reasoning: Option<&str>) -> JournalEventKind {
    JournalEventKind::MessageDelta {
        content: content.to_string(),
        reasoning_content: reasoning.map(String::from),
    }
}

fn tool_delta(id: &str, output: &str) -> JournalEventKind {
    JournalEventKind::ToolOutputDelta {
        id: id.to_string(),
        name: "bash".to_string(),
        output: output.to_string(),
    }
}

fn tool_result(id: &str, output: &str) -> JournalEventKind {
    JournalEventKind::ToolResult {
        id: id.to_string(),
        name: "bash".to_string(),
        ok: true,
        output: output.to_string(),
    }
}

fn kinds(light: &[Value]) -> Vec<&str> {
    light
        .iter()
        .map(|v| v["kind"]["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn finalized_delta_run_is_dropped_and_message_stripped() {
    let events = vec![
        ev(0, delta("he", Some("thin"))),
        ev(1, delta("llo", Some("king"))),
        // The worker journals context_usage between the deltas and the
        // final message of the same turn; it must not break the pairing.
        ev(
            2,
            JournalEventKind::ContextUsage {
                tokens: 10,
                context_window: None,
            },
        ),
        ev(3, msg("assistant", "hello", Some("thinking"))),
    ];
    let light = light_history(events);
    assert_eq!(kinds(&light), ["context_usage", "message"], "{light:?}");
    let m = &light[1]["kind"];
    assert_eq!(m["content"], json!("hello"));
    assert!(m.get("reasoning_content").is_none());
    assert_eq!(m["reasoning_omitted"], json!(true));
    // Seq/ts survive the projection (the UI addresses events by seq).
    assert_eq!(light[1]["seq"], json!(3));
}

#[test]
fn unfinalized_trailing_run_keeps_reasoning() {
    // The currently streaming message: no canonical message yet.
    let events = vec![
        ev(0, delta("so ", Some("str"))),
        ev(1, delta("far", Some("eaming"))),
    ];
    let light = light_history(events);
    assert_eq!(kinds(&light), ["message_delta", "message_delta"]);
    assert_eq!(light[1]["kind"]["reasoning_content"], json!("eaming"));
}

#[test]
fn run_closed_by_user_message_is_kept() {
    // A dead run's truncated preview ends at the next user message — it is
    // not superseded and must survive (with its reasoning).
    let events = vec![
        ev(0, delta("part", Some("ial"))),
        ev(1, msg("user", "next turn", None)),
    ];
    let light = light_history(events);
    assert_eq!(kinds(&light), ["message_delta", "message"]);
    assert_eq!(light[0]["kind"]["reasoning_content"], json!("ial"));
}

#[test]
fn tool_output_delta_dropped_only_when_result_follows() {
    let events = vec![
        ev(0, tool_delta("c1", "out")),
        ev(1, tool_result("c1", "out")),
        // A still-running tool (no result yet): keep its streamed output.
        ev(2, tool_delta("c2", "tail")),
    ];
    let light = light_history(events);
    assert_eq!(kinds(&light), ["tool_result", "tool_output_delta"]);
    assert_eq!(light[1]["kind"]["output"], json!("tail"));
}

#[test]
fn empty_reasoning_gets_no_flag_and_plain_messages_are_verbatim() {
    let events = vec![
        ev(0, msg("user", "hi", None)),
        ev(1, msg("assistant", "ok", Some(""))),
    ];
    let light = light_history(events);
    assert_eq!(light[0]["kind"]["content"], json!("hi"));
    assert!(light[1]["kind"].get("reasoning_omitted").is_none());
    assert_eq!(light[1]["kind"]["reasoning_content"], json!(""));
}
