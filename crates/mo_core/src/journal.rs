//! Append-only JSONL journal for session chat/tool history.
//!
//! The worker is the single writer for a session's journal; the gateway
//! only reads. Every append is flushed so readers never observe torn lines
//! (only a trailing partial line, which readers tolerate).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
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

    /// Flush the underlying buffer. Every `append` already flushes; this is
    /// for callers that copy a batch of events and want a flush error to
    /// surface rather than be swallowed by the drop.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
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

/// Return only events with `seq > after_seq` (used by the history endpoint
/// for incremental client refetches).
pub fn read_events_after(path: &Path, after_seq: u64) -> Result<Vec<JournalEvent>> {
    Ok(read_events(path)?
        .into_iter()
        .filter(|e| e.seq > after_seq)
        .collect())
}

/// Copy the events of `src` with `seq < until_seq` into a journal at `dst`,
/// used when a session is forked at one of its user messages: the fork keeps
/// the history *before* that message, so the user can edit the message and
/// let the model generate a fresh answer from the same earlier context.
///
/// The copied events get fresh `seq`/`ts` values (assigned by the
/// [`JournalWriter`], so `dst` starts at seq 0 and stays contiguous) — `dst`
/// is expected to be a fresh journal. `SystemPrompt` events are **skipped**:
/// the system prompt embeds the session-specific scratch dir
/// (`<data_dir>/sessions/<id>/tmp`), so copying it verbatim would point the
/// forked session's model at the source session's scratch dir. Without a
/// journaled prompt the worker builds (and journals) a fresh one for the new
/// session on its first run.
///
/// Copies everything else verbatim, streaming `*_delta` previews included
/// (the UI folds them into their canonical events and the worker ignores
/// them). A missing `src` reads as an empty journal, so forking at the very
/// first message yields a session with an empty journal — exactly a fresh
/// session in the same setup. Returns the events written to `dst`.
pub fn copy_events_before(src: &Path, dst: &Path, until_seq: u64) -> Result<Vec<JournalEvent>> {
    let events = read_events(src)?;
    let mut writer = JournalWriter::open(dst)?;
    let mut copied = Vec::new();
    for event in events {
        if event.seq >= until_seq {
            break;
        }
        if matches!(
            event.kind,
            crate::types::JournalEventKind::SystemPrompt { .. }
        ) {
            continue;
        }
        copied.push(writer.append(event.kind)?);
    }
    writer.flush()?;
    Ok(copied)
}

/// Incremental journal tail reader for the gateway SSE endpoint: reads only
/// the bytes appended after `offset` and parses the complete JSON lines they
/// contain. Reading the whole history every poll is what made the gateway
/// burn CPU on a running session (streamed `message_delta` events can push a
/// journal to hundreds of thousands of lines), so this is the O(new bytes)
/// replacement for the poll loop.
///
/// `pending` carries the trailing, not-yet-newline-terminated bytes from the
/// previous read — the writer appends a line and its newline in two separate
/// writes, and a UTF-8 codepoint can straddle a read boundary — so it is
/// threaded back in on every call. Returns the events parsed from complete
/// lines plus the new byte offset to pass to the next call. A journal that
/// does not exist yet (the worker has not created it) yields no events and
/// leaves `offset` unchanged.
pub fn read_events_tail(
    path: &Path,
    offset: u64,
    pending: &mut Vec<u8>,
) -> Result<(Vec<JournalEvent>, u64)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), offset));
        }
        Err(e) => return Err(e.into()),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let new_offset = offset + buf.len() as u64;
    pending.extend_from_slice(&buf);

    // Split on newlines; the final segment has no trailing newline yet and
    // stays in `pending` for the next read. Non-JSON lines are skipped, as
    // in `read_events`, so a stray/corrupt line never breaks the tail.
    let mut events = Vec::new();
    let mut consumed = 0usize;
    while let Some(nl) = pending[consumed..].iter().position(|&b| b == b'\n') {
        let line_end = consumed + nl;
        let line = &pending[consumed..line_end];
        consumed = line_end + 1;
        let trimmed = std::str::from_utf8(line).unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<JournalEvent>(trimmed) {
            events.push(event);
        }
    }
    pending.drain(..consumed);
    Ok((events, new_offset))
}

// Unit tests live in `mo_core/src/tests/journal_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/journal_tests.rs"]
mod tests;
