//! Append-only JSONL journal for session chat/tool history.
//!
//! The worker is the single writer for a session's journal; the gateway
//! only reads. Every append is flushed so readers never observe torn lines
//! (only a trailing partial line, which readers tolerate).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use chrono::Utc;

use crate::types::{JournalEvent, JournalEventKind};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, JournalError>;

/// Append-only writer. Assigns monotonically increasing `seq` values based
/// on the number of complete lines already present when the file is opened.
pub struct JournalWriter {
    file: BufWriter<File>,
    next_seq: u64,
}

impl JournalWriter {
    /// Open (create if missing) the journal at `path` in append mode.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        // Count existing complete lines so seq continues where the file left
        // off (a torn trailing line is skipped).
        let mut next_seq = 0u64;
        let reader = BufReader::new(&file);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if serde_json::from_str::<JournalEvent>(trimmed).is_ok() {
                next_seq += 1;
            }
        }
        Ok(JournalWriter {
            file: BufWriter::new(file),
            next_seq,
        })
    }

    /// Append an event, assign `seq`/`ts`, flush, and return the event.
    pub fn append(&mut self, kind: JournalEventKind) -> Result<JournalEvent> {
        let event = JournalEvent {
            seq: self.next_seq,
            ts: Utc::now(),
            kind,
        };
        let line = serde_json::to_string(&event)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.next_seq += 1;
        Ok(event)
    }
}

/// Parse the whole journal into events. Non-JSON lines are skipped so a
/// trailing partial line (or an occasional corrupt line) never breaks the
/// read; `seq` values are carried through verbatim.
pub fn read_events(path: &Path) -> Result<Vec<JournalEvent>> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut events = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<JournalEvent>(trimmed) {
            events.push(event);
        }
    }
    Ok(events)
}

/// Return only events with `seq > after_seq` (cheap re-fetch for the
/// history endpoint and the SSE tail).
pub fn read_events_after(path: &Path, after_seq: u64) -> Result<Vec<JournalEvent>> {
    Ok(read_events(path)?
        .into_iter()
        .filter(|e| e.seq > after_seq)
        .collect())
}

#[cfg(test)]
mod tests {
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
}
