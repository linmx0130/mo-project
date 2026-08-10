//! `bash` tool: run a command via `sh -c` inside the workdir.
//!
//! stdout/stderr are read incrementally: each chunk is emitted through the
//! caller-provided sink as a `ToolOutputDelta` journal event while the
//! command still runs, so the frontend can render output live. The return
//! value keeps the canonical (capped) full output for the `ToolResult`
//! event and for the model context.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use mo_core::JournalEventKind;
use tokio::io::AsyncReadExt;

const OUTPUT_CAP: usize = 1024 * 1024; // 1 MB — retained result/model context
const CHUNK_SIZE: usize = 8192;

/// Live delta-stream budget: total bytes of `tool_output_delta` journaled
/// per command before a single marker replaces further output. Streaming
/// stays live up to this cap; the journal and the browser are protected
/// from unbounded growth beyond it.
pub const DELTA_STREAM_CAP: usize = 10 * 1024 * 1024; // 10 MB

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The process-group ids of the currently running bash children, if any.
/// The worker's SIGTERM handler kills these groups so a cancel also stops
/// the commands' pipeline children (gradlew, tail, ...), which would
/// otherwise survive as orphans once the worker process dies. The value
/// lives in a shared static (rather than a tokio channel) so the signal
/// handler can read it even when the runtime is under load. A set (not a
/// single slot): tool calls in one message run concurrently, so several
/// commands may be in flight at once.
pub static ACTIVE_BASH_PGIDS: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn active_bash_pgids() -> Vec<u32> {
    ACTIVE_BASH_PGIDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect()
}

fn add_active_bash_pgid(pgid: u32) {
    ACTIVE_BASH_PGIDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pgid);
}

fn remove_active_bash_pgid(pgid: u32) {
    ACTIVE_BASH_PGIDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&pgid);
}

/// Removes its own process group from `ACTIVE_BASH_PGIDS` when a bash call
/// ends (any return path) — only its own, so concurrent commands' entries
/// are never clobbered.
struct BashPgidGuard {
    pgid: u32,
}
impl Drop for BashPgidGuard {
    fn drop(&mut self) {
        remove_active_bash_pgid(self.pgid);
    }
}

/// Run `command` via `sh -c` in `workdir`. Returns stdout+stderr plus the
/// exit code, capped at ~1 MB. On timeout the child's whole process group
/// is killed and an error is returned.
///
/// While the command runs, raw stdout/stderr chunks are forwarded to
/// `on_event` as `ToolOutputDelta { id: tool_call_id, name: "bash", .. }`
/// events so readers can stream the output live. `on_event` is a shared
/// (non-`mut`) sink because concurrent tool calls journal through the same
/// closure.
pub async fn bash(
    workdir: &Path,
    command: &str,
    timeout: Duration,
    tool_call_id: &str,
    on_event: &(dyn Fn(JournalEventKind) + Send + Sync),
) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group: the timeout and the worker's SIGTERM handler
        // kill the whole group, so pipeline children (`gradlew | tail`,
        // background jobs, ...) never outlive the tool call.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn sh: {e}"))?;
    add_active_bash_pgid(child.id().expect("spawned child must have a pid"));
    let _pgid_guard = BashPgidGuard {
        pgid: child.id().expect("spawned child must have a pid"),
    };

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to take stdout pipe".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to take stderr pipe".to_string())?;

    // Two reader tasks forward raw chunks over a channel; the single
    // consumer below owns `on_event` and also accumulates the full
    // stdout/stderr buffers for the final result. Source tag: 0 = stdout,
    // 1 = stderr. Both pipes are drained concurrently, so a command that
    // floods one pipe can never deadlock the other.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(64);
    let tx_stdout = tx.clone();
    let stdout_reader = tokio::spawn(async move {
        let mut buf = [0u8; CHUNK_SIZE];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx_stdout.send((0, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let stderr_reader = tokio::spawn(async move {
        let mut buf = [0u8; CHUNK_SIZE];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send((1, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    // The channel closes when both readers hit EOF (each holds one sender);
    // the JoinHandles are intentionally detached — the tasks only forward
    // chunks and never touch `on_event` or `child`.
    drop((stdout_reader, stderr_reader));

    // Retained output buffers live OUTSIDE the timeout future so the
    // partial output can be reported when the command times out (previously
    // they were dropped with the future and the model saw only the timeout
    // string). They are capped at `OUTPUT_CAP`; the live delta stream is
    // capped separately at `DELTA_STREAM_CAP`.
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    let mut retained_truncated = false;
    let mut streamed_bytes: usize = 0;
    let mut stream_capped = false;

    let run = async {
        while let Some((src, chunk)) = rx.recv().await {
            // Live stream first: journal every chunk (bounded at
            // DELTA_STREAM_CAP, then a single marker), so the UI keeps
            // filling in while the command runs.
            if streamed_bytes < DELTA_STREAM_CAP {
                streamed_bytes += chunk.len();
                let text = String::from_utf8_lossy(&chunk);
                on_event(JournalEventKind::ToolOutputDelta {
                    id: tool_call_id.to_string(),
                    name: "bash".to_string(),
                    output: text.into_owned(),
                });
            } else if !stream_capped {
                stream_capped = true;
                on_event(JournalEventKind::ToolOutputDelta {
                    id: tool_call_id.to_string(),
                    name: "bash".to_string(),
                    output: "\n[output capped at 10 MB — further output is suppressed]\n"
                        .to_string(),
                });
            }
            // Retained copy for the final result/model context (bounded at
            // OUTPUT_CAP; overflow is dropped, the live stream is not).
            let retained = if src == 0 {
                &mut stdout_buf
            } else {
                &mut stderr_buf
            };
            if retained.len() < OUTPUT_CAP {
                let room = OUTPUT_CAP - retained.len();
                retained.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if retained.len() >= OUTPUT_CAP {
                    retained_truncated = true;
                }
            } else {
                retained_truncated = true;
            }
        }
        Ok::<_, String>(())
    };
    let timed_out = match tokio::time::timeout(timeout, run).await {
        Err(_) => true,
        Ok(result) => {
            result?;
            false
        }
    };
    if timed_out {
        // Kill the whole process group: the direct child is `sh`, and its
        // pipeline children (gradlew, tail, ...) would otherwise survive as
        // orphans — `kill_on_drop` only reaches `sh`.
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await; // reap
        return Err(format!(
            "command timed out after {}s: {command}\n{}",
            timeout.as_secs(),
            partial_output(&stdout_buf, &stderr_buf, retained_truncated)
        ));
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for command: {e}"))?;

    let mut text = String::new();
    text.push_str(&format!("exit code: {}\n", status.code().unwrap_or(-1)));
    text.push_str(&String::from_utf8_lossy(&stdout_buf));
    if !stderr_buf.is_empty() {
        text.push_str(&format!(
            "[stderr]\n{}",
            String::from_utf8_lossy(&stderr_buf)
        ));
    }
    if text.len() > OUTPUT_CAP {
        let cut = text.floor_char_boundary(OUTPUT_CAP);
        text = format!(
            "{}\n\n[output truncated: {} bytes total, showing first {}]",
            &text[..cut],
            text.len(),
            cut
        );
    }
    Ok(text)
}

/// Build the `[partial output]` section of a timeout error from the
/// retained buffers — the model gets to see what the command produced
/// before it was killed — or a hint when nothing was captured (the command
/// may be buffering through `tail`/`head`, or hung before producing any
/// output at all).
fn partial_output(stdout_buf: &[u8], stderr_buf: &[u8], truncated: bool) -> String {
    let stdout = String::from_utf8_lossy(stdout_buf);
    let stderr = String::from_utf8_lossy(stderr_buf);
    let mut out = String::new();
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        out.push_str(
            "(no output was received — the command may be buffering through \
             `tail`/`head` or hung before producing output; run long builds \
             with `nohup ... > log 2>&1 &` and poll the log instead)",
        );
        return out;
    }
    if !stdout.trim().is_empty() {
        out.push_str("[partial output]\n");
        out.push_str(stdout.trim_end());
        out.push('\n');
    }
    if !stderr.trim().is_empty() {
        out.push_str("[partial stderr]\n");
        out.push_str(stderr.trim_end());
        out.push('\n');
    }
    if truncated {
        out.push_str("[output capped at 1 MB — the live UI stream continues]\n");
    }
    out
}

// Unit tests live in `mo_worker/src/tests/bash_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/bash_tests.rs"]
mod tests;
