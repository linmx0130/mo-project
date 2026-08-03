//! `bash` tool: run a command via `sh -c` inside the workdir.
//!
//! stdout/stderr are read incrementally: each chunk is emitted through the
//! caller-provided sink as a `ToolOutputDelta` journal event while the
//! command still runs, so the frontend can render output live. The return
//! value keeps the canonical (capped) full output for the `ToolResult`
//! event and for the model context.

use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use mo_core::JournalEventKind;
use tokio::io::AsyncReadExt;

const OUTPUT_CAP: usize = 1024 * 1024; // 1 MB
const CHUNK_SIZE: usize = 8192;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The process-group id of the currently running bash child, if any. The
/// worker's SIGTERM handler kills this group so a cancel also stops the
/// command's pipeline children (gradlew, tail, ...), which would otherwise
/// survive as orphans once the worker process dies. The value lives in a
/// shared static (rather than a tokio channel) so the signal handler can
/// read it even when the runtime is under load.
pub static ACTIVE_BASH_PGID: Mutex<Option<u32>> = Mutex::new(None);

pub fn active_bash_pgid() -> Option<u32> {
    *ACTIVE_BASH_PGID.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_active_bash_pgid(pgid: Option<u32>) {
    *ACTIVE_BASH_PGID.lock().unwrap_or_else(|e| e.into_inner()) = pgid;
}

/// Clears `ACTIVE_BASH_PGID` when a bash call ends (any return path).
struct BashPgidGuard;
impl Drop for BashPgidGuard {
    fn drop(&mut self) {
        set_active_bash_pgid(None);
    }
}

/// Run `command` via `sh -c` in `workdir`. Returns stdout+stderr plus the
/// exit code, capped at ~1 MB. On timeout the child's whole process group
/// is killed and an error is returned.
///
/// While the command runs, raw stdout/stderr chunks are forwarded to
/// `on_delta` as `ToolOutputDelta { id: tool_call_id, name: "bash", .. }`
/// events so readers can stream the output live.
pub async fn bash(
    workdir: &Path,
    command: &str,
    timeout: Duration,
    tool_call_id: &str,
    on_delta: &mut (dyn FnMut(JournalEventKind) + Send),
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
    set_active_bash_pgid(child.id());
    let _pgid_guard = BashPgidGuard;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to take stdout pipe".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to take stderr pipe".to_string())?;

    // Two reader tasks forward raw chunks over a channel; the single
    // consumer below owns `on_delta` and also accumulates the full
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
    // chunks and never touch `on_delta` or `child`.
    drop((stdout_reader, stderr_reader));

    let run = async {
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        while let Some((src, chunk)) = rx.recv().await {
            let text = String::from_utf8_lossy(&chunk);
            on_delta(JournalEventKind::ToolOutputDelta {
                id: tool_call_id.to_string(),
                name: "bash".to_string(),
                output: text.into_owned(),
            });
            if src == 0 {
                stdout_buf.extend_from_slice(&chunk);
            } else {
                stderr_buf.extend_from_slice(&chunk);
            }
        }
        Ok::<_, String>((stdout_buf, stderr_buf))
    };
    let (stdout_buf, stderr_buf) = match tokio::time::timeout(timeout, run).await {
        Err(_) => {
            // Kill the whole process group: the direct child is `sh`, and
            // its pipeline children (gradlew, tail, ...) would otherwise
            // survive as orphans — `kill_on_drop` only reaches `sh`.
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await; // reap
            return Err(format!(
                "command timed out after {}s: {command}\n\
                 (the command's process group was killed; note that piping \
                 output through `tail`/`head` hides all output until the \
                 command finishes — run long builds with `nohup ... > log \
                 2>&1 &` and poll the log instead)",
                timeout.as_secs()
            ));
        }
        Ok(result) => result?,
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_command_and_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = |_: JournalEventKind| {};
        let out = bash(
            dir.path(),
            "echo hello && exit 3",
            Duration::from_secs(10),
            "call_1",
            &mut sink,
        )
        .await
        .unwrap();
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("exit code: 3"), "got: {out}");
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = |_: JournalEventKind| {};
        let out = bash(
            dir.path(),
            "echo boom 1>&2",
            Duration::from_secs(10),
            "call_1",
            &mut sink,
        )
        .await
        .unwrap();
        assert!(out.contains("boom"), "got: {out}");
    }

    #[tokio::test]
    async fn streams_output_chunks_live() {
        let dir = tempfile::tempdir().unwrap();
        let mut streamed: Vec<String> = Vec::new();
        let out = {
            let mut sink = |kind: JournalEventKind| {
                if let JournalEventKind::ToolOutputDelta { output, .. } = kind {
                    streamed.push(output);
                }
            };
            bash(
                dir.path(),
                "printf 'one\\ntwo\\n'; printf 'boom' 1>&2",
                Duration::from_secs(10),
                "call_1",
                &mut sink,
            )
            .await
            .unwrap()
        };
        assert!(out.contains("one"), "got: {out}");
        assert!(out.contains("boom"), "got: {out}");
        // Every chunk was streamed through the sink, tagged with the tool id.
        let joined = streamed.concat();
        assert!(joined.contains("one"), "streamed: {joined}");
        assert!(joined.contains("two"), "streamed: {joined}");
        assert!(joined.contains("boom"), "streamed: {joined}");
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = |_: JournalEventKind| {};
        let err = bash(
            dir.path(),
            "sleep 30",
            Duration::from_millis(300),
            "call_1",
            &mut sink,
        )
        .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("timed out"));
    }

    #[tokio::test]
    async fn timeout_kills_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = |_: JournalEventKind| {};
        let err = bash(
            dir.path(),
            "sleep 30 & echo $!; wait",
            Duration::from_millis(300),
            "call_1",
            &mut sink,
        )
        .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("timed out"));

        // The backgrounded `sleep 30` shares the bash child's process group,
        // so the timeout must have killed it too — no orphan survives.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '^sleep 30$' || true")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.trim().is_empty(),
            "orphaned sleep survived the timeout: {stdout}"
        );
    }
}
