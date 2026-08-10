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

// Unit tests live in `mo_core/src/tests/journal_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/journal_tests.rs"]
mod tests;
