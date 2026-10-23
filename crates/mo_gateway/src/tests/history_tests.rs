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

fn tool_start(id: &str) -> JournalEventKind {
    JournalEventKind::ToolCallStart {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: "{}".to_string(),
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
fn dead_runs_preview_survives_a_later_run_with_the_same_id() {
    // Run 1's tool `call_1` died mid-stream (start + deltas, no result); run 2
    // repeated the id and completed. The dead run's preview must stay — it is
    // below the SSE cursor, so nothing else would ever restore it.
    let events = vec![
        ev(0, tool_start("call_1")),
        ev(1, tool_delta("call_1", "dead output")),
        ev(2, tool_start("call_1")),
        ev(3, tool_delta("call_1", "live output")),
        ev(4, tool_result("call_1", "live output")),
    ];
    let light = light_history(events);
    assert_eq!(
        kinds(&light),
        [
            "tool_call_start",
            "tool_output_delta",
            "tool_call_start",
            "tool_result"
        ],
        "{light:?}"
    );
    assert_eq!(light[1]["kind"]["output"], json!("dead output"));
    // The finalized run's own preview stays dropped (its result carries the
    // complete output) and the events keep their seqs.
    assert_eq!(light[1]["seq"], json!(1));
    assert_eq!(light[2]["seq"], json!(2));
}

#[test]
fn in_flight_run_keeps_its_deltas_when_an_earlier_run_used_the_same_id() {
    // Run 1 completed with `call_1`; run 2 reuses the id and is still
    // streaming. Its preview is unfinalized, so it stays verbatim.
    let events = vec![
        ev(0, tool_start("call_1")),
        ev(1, tool_delta("call_1", "run1 output")),
        ev(2, tool_result("call_1", "run1 output")),
        ev(3, tool_start("call_1")),
        ev(4, tool_delta("call_1", "run2 streaming")),
    ];
    let light = light_history(events);
    assert_eq!(
        kinds(&light),
        [
            "tool_call_start",
            "tool_result",
            "tool_call_start",
            "tool_output_delta"
        ],
        "{light:?}"
    );
    assert_eq!(light[3]["kind"]["output"], json!("run2 streaming"));
}

#[test]
fn every_delta_of_a_finalized_run_is_dropped() {
    let events = vec![
        ev(0, tool_start("c1")),
        ev(1, tool_delta("c1", "a")),
        ev(2, tool_delta("c1", "b")),
        ev(3, tool_result("c1", "ab")),
    ];
    let light = light_history(events);
    assert_eq!(kinds(&light), ["tool_call_start", "tool_result"]);
}

#[test]
fn deltas_of_a_sibling_tool_are_not_dropped() {
    // Parallel calls: `c2` is still running while `c1` finalized.
    let events = vec![
        ev(0, tool_start("c1")),
        ev(1, tool_start("c2")),
        ev(2, tool_delta("c1", "one")),
        ev(3, tool_result("c1", "one")),
        ev(4, tool_delta("c2", "two")),
    ];
    let light = light_history(events);
    assert_eq!(
        kinds(&light),
        [
            "tool_call_start",
            "tool_call_start",
            "tool_result",
            "tool_output_delta"
        ],
        "{light:?}"
    );
    assert_eq!(light[3]["kind"]["output"], json!("two"));
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

#[test]
fn long_tool_output_is_slimmed_to_a_preview() {
    let long = "x".repeat(TOOL_OUTPUT_LAZY_THRESHOLD + 10);
    let light = light_history(vec![ev(0, tool_result("c1", &long))]);
    let kind = &light[0]["kind"];
    assert_eq!(
        kind["output"].as_str().unwrap().chars().count(),
        TOOL_OUTPUT_PREVIEW
    );
    assert_eq!(kind["output_omitted"], json!(true));
    assert_eq!(kind["output_chars"], json!(TOOL_OUTPUT_LAZY_THRESHOLD + 10));
    // The rest of the event is untouched.
    assert_eq!(kind["kind"], json!("tool_result"));
    assert_eq!(kind["ok"], json!(true));
}

#[test]
fn short_tool_output_is_verbatim() {
    let light = light_history(vec![ev(0, tool_result("c1", "exit code: 0"))]);
    assert_eq!(light[0]["kind"]["output"], json!("exit code: 0"));
    assert!(light[0]["kind"].get("output_omitted").is_none());
    assert!(light[0]["kind"].get("output_chars").is_none());
}

#[test]
fn output_at_the_threshold_is_verbatim() {
    let at_limit = "y".repeat(TOOL_OUTPUT_LAZY_THRESHOLD);
    let light = light_history(vec![ev(0, tool_result("c1", &at_limit))]);
    assert_eq!(light[0]["kind"]["output"], json!(at_limit));
    assert!(light[0]["kind"].get("output_omitted").is_none());
}

#[test]
fn preview_cuts_on_char_boundaries() {
    // Multibyte chars: a byte-wise cut would split a codepoint.
    let long = "é".repeat(TOOL_OUTPUT_LAZY_THRESHOLD + 5);
    let light = light_history(vec![ev(0, tool_result("c1", &long))]);
    let preview = light[0]["kind"]["output"].as_str().unwrap();
    assert_eq!(preview.chars().count(), TOOL_OUTPUT_PREVIEW);
    assert!(preview.chars().all(|c| c == 'é'));
    assert_eq!(
        light[0]["kind"]["output_chars"],
        json!(TOOL_OUTPUT_LAZY_THRESHOLD + 5)
    );
}
