//! Unit tests for the `journal` module — production code lives in
//! `mo_core/src/journal.rs`. Wired from there with `#[cfg(test)] #[path = "tests/journal_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use crate::types::{JournalMessage, SessionStatus};

fn msg_event(role: &str, content: &str) -> JournalEventKind {
    JournalEventKind::Message(JournalMessage {
        role: role.to_string(),
        content: content.to_string(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

#[test]
fn round_trip_append_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    let e1 = writer.append(msg_event("user", "hello")).unwrap();
    let e2 = writer.append(msg_event("assistant", "hi")).unwrap();
    assert_eq!(e1.seq, 0);
    assert_eq!(e2.seq, 1);
    drop(writer);

    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[1].seq, 1);
    assert_eq!(
        events[1].kind,
        JournalEventKind::Message(JournalMessage {
            role: "assistant".to_string(),
            content: "hi".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        })
    );
}

#[test]
fn tolerant_of_trailing_partial_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer.append(msg_event("user", "a")).unwrap();
    writer.append(msg_event("assistant", "b")).unwrap();
    drop(writer);
    // Simulate a torn write at the tail.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"seq\":2,\"ts\":\"2026-").unwrap();
    f.flush().unwrap();

    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 2);

    // Reopening the writer must continue seq at 2 (partial line ignored).
    let mut writer = JournalWriter::open(&path).unwrap();
    let e = writer.append(msg_event("assistant", "c")).unwrap();
    assert_eq!(e.seq, 2);
}

#[test]
fn read_events_after_filters_by_seq() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    for i in 0..5 {
        writer.append(msg_event("user", &format!("m{i}"))).unwrap();
    }
    let after = read_events_after(&path, 2).unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].seq, 3);
    assert_eq!(after[1].seq, 4);
}

#[test]
fn missing_file_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        read_events(&dir.path().join("nope.jsonl"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn read_events_tail_reads_only_appended_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer.append(msg_event("user", "a")).unwrap();
    writer.append(msg_event("assistant", "b")).unwrap();
    drop(writer);

    let mut pending = Vec::new();
    let (events, offset) = read_events_tail(&path, 0, &mut pending).unwrap();
    assert_eq!(events.len(), 2);
    assert!(offset > 0);

    // Appending one more event must return only that event on the next read.
    let mut writer = JournalWriter::open(&path).unwrap();
    writer.append(msg_event("user", "c")).unwrap();
    drop(writer);

    let (events, offset2) = read_events_tail(&path, offset, &mut pending).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 2);
    assert!(offset2 > offset);

    // A re-read from the same offset is a no-op (nothing new appended).
    let (events, offset3) = read_events_tail(&path, offset2, &mut pending).unwrap();
    assert!(events.is_empty());
    assert_eq!(offset3, offset2);
}

#[test]
fn read_events_tail_missing_file_is_empty_and_keeps_offset() {
    let dir = tempfile::tempdir().unwrap();
    let mut pending = Vec::new();
    let (events, offset) =
        read_events_tail(&dir.path().join("nope.jsonl"), 0, &mut pending).unwrap();
    assert!(events.is_empty());
    assert_eq!(offset, 0);
}

#[test]
fn read_events_tail_tolerates_torn_trailing_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer.append(msg_event("user", "a")).unwrap();
    drop(writer);

    // A line written in two writes (the worker writes the JSON and the
    // newline separately): the first read sees the line without its tail.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(br#"{"seq":1,"ts":"2026-01-01T00:00:00Z","kind":{"kind":"message","role":"user","content":"b"}"#)
        .unwrap();
    f.flush().unwrap();
    drop(f);

    let mut pending = Vec::new();
    let (events, offset) = read_events_tail(&path, 0, &mut pending).unwrap();
    assert_eq!(events.len(), 1);
    assert!(!pending.is_empty(), "torn tail must be retained");

    // Finish the torn line (the retained fragment already closed the `kind`
    // object; only the outer object's brace is missing); the retained
    // fragment plus the new bytes form a complete event.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"}\n").unwrap();
    f.flush().unwrap();
    drop(f);

    let (events, _) = read_events_tail(&path, offset, &mut pending).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 1);
    assert!(pending.is_empty());
}

#[test]
fn read_events_tail_handles_multibyte_split_across_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer.append(msg_event("user", "ok")).unwrap();
    drop(writer);

    let event = JournalEvent {
        seq: 1,
        ts: chrono::Utc::now(),
        kind: msg_event("user", "héllo"),
    };
    let line = serde_json::to_string(&event).unwrap();
    let bytes = line.as_bytes();
    // 'é' is 0xC3 0xA9 in UTF-8: split between those two bytes so the first
    // read ends mid-codepoint.
    let pos = bytes.iter().position(|&b| b == 0xC3).unwrap();
    let split = pos + 1;

    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&bytes[..split]).unwrap();
    f.flush().unwrap();
    drop(f);

    let mut pending = Vec::new();
    let (events, offset) = read_events_tail(&path, 0, &mut pending).unwrap();
    assert_eq!(events.len(), 1); // only the first, complete line
    assert!(!pending.is_empty());

    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&bytes[split..]).unwrap();
    f.write_all(b"\n").unwrap();
    f.flush().unwrap();
    drop(f);

    let (events, _) = read_events_tail(&path, offset, &mut pending).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].kind {
        JournalEventKind::Message(m) => assert_eq!(m.content, "héllo"),
        other => panic!("expected message, got: {other:?}"),
    }
    assert!(pending.is_empty());
}

#[test]
fn status_change_event_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer
        .append(JournalEventKind::StatusChange {
            status: SessionStatus::Running,
            error: None,
        })
        .unwrap();
    writer
        .append(JournalEventKind::StatusChange {
            status: SessionStatus::Failed,
            error: Some("boom".to_string()),
        })
        .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(
        events[0].kind,
        JournalEventKind::StatusChange {
            status: SessionStatus::Running,
            error: None
        }
    );
    assert_eq!(
        events[1].kind,
        JournalEventKind::StatusChange {
            status: SessionStatus::Failed,
            error: Some("boom".to_string())
        }
    );
}

#[test]
fn streaming_delta_events_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer
        .append(JournalEventKind::MessageDelta {
            content: "Hello ".to_string(),
            reasoning_content: None,
        })
        .unwrap();
    writer
        .append(JournalEventKind::MessageDelta {
            content: String::new(),
            reasoning_content: Some("Let me think: ".to_string()),
        })
        .unwrap();
    writer
        .append(JournalEventKind::MessageDelta {
            content: "world".to_string(),
            reasoning_content: None,
        })
        .unwrap();
    writer
        .append(JournalEventKind::ToolOutputDelta {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            output: "hello\n".to_string(),
        })
        .unwrap();
    writer
        .append(JournalEventKind::ToolOutputDelta {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            output: "world\n".to_string(),
        })
        .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(
        events[0].kind,
        JournalEventKind::MessageDelta {
            content: "Hello ".to_string(),
            reasoning_content: None
        }
    );
    assert_eq!(
        events[1].kind,
        JournalEventKind::MessageDelta {
            content: String::new(),
            reasoning_content: Some("Let me think: ".to_string())
        }
    );
    assert_eq!(
        events[2].kind,
        JournalEventKind::MessageDelta {
            content: "world".to_string(),
            reasoning_content: None
        }
    );
    assert_eq!(
        events[3].kind,
        JournalEventKind::ToolOutputDelta {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            output: "hello\n".to_string()
        }
    );
    assert_eq!(
        events[4].kind,
        JournalEventKind::ToolOutputDelta {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            output: "world\n".to_string()
        }
    );
}

#[test]
fn system_prompt_event_round_trips_with_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer
        .append(JournalEventKind::SystemPrompt {
            content: "You are in Explore mode.".to_string(),
            mode: crate::types::Mode::Explore,
        })
        .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(
        events[0].kind,
        JournalEventKind::SystemPrompt {
            content: "You are in Explore mode.".to_string(),
            mode: crate::types::Mode::Explore,
        }
    );
}

#[test]
fn handoff_event_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer
        .append(JournalEventKind::Handoff {
            content: "original input … next step".to_string(),
            mode: crate::types::Mode::Plan,
        })
        .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(
        events[0].kind,
        JournalEventKind::Handoff {
            content: "original input … next step".to_string(),
            mode: crate::types::Mode::Plan,
        }
    );
}

#[test]
fn mode_change_event_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut writer = JournalWriter::open(&path).unwrap();
    writer
        .append(JournalEventKind::ModeChange {
            mode: crate::types::Mode::Plan,
            content: "[Session mode changed to plan] …".to_string(),
        })
        .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(
        events[0].kind,
        JournalEventKind::ModeChange {
            mode: crate::types::Mode::Plan,
            content: "[Session mode changed to plan] …".to_string(),
        }
    );
}

/// A journal line written before `SystemPrompt` carried a mode must
/// still parse: the missing field defaults to `build`, so journals
/// created by older versions keep working (and the mode-marker scan
/// treats them as a build-mode run).
#[test]
fn legacy_system_prompt_without_mode_defaults_to_build() {
    let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","kind":{"kind":"system_prompt","content":"legacy prompt"}}"#;
    let event: JournalEvent = serde_json::from_str(line).unwrap();
    match event.kind {
        JournalEventKind::SystemPrompt { content, mode } => {
            assert_eq!(content, "legacy prompt");
            assert_eq!(mode, crate::types::Mode::Build);
        }
        other => panic!("expected system_prompt, got: {other:?}"),
    }
}
