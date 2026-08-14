//! `bash_in_background` tool: run a long-lived command via `sh -c` and
//! return immediately, tracking it so later calls can query (`status`) or
//! stop (`kill`) it.
//!
//! Unlike `bash`, this tool never waits for the command, never streams
//! output, and never applies the 120s timeout. It is meant for commands
//! that run for minutes or longer: the model is told to redirect stdout/
//! stderr to files (they are discarded here) and to check on the process
//! with `status` / stop it with `kill`.

use std::collections::HashMap;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{LazyLock, Mutex};

use uuid::Uuid;

/// A background job spawned by this worker. The `Child` handle stays in the
/// map (rather than being dropped) so `try_wait` can reap it on demand; the
/// OS process keeps running independently until `kill`/`kill_all` is called
/// or it exits on its own.
struct BackgroundJob {
    child: Option<tokio::process::Child>,
    /// The process-group id of the child (equal to its pid because the
    /// command is spawned with `.process_group(0)`); `kill` signals the
    /// whole group so pipeline children don't survive the kill.
    pgid: u32,
    /// Set once `try_wait` has reaped the child; repeated `status` calls
    /// keep reporting the same outcome.
    finished: Option<ExitStatus>,
}

/// All background jobs started by this worker, keyed by the opaque process
/// id returned to the model. The mutex is only held for short synchronous
/// operations (spawn bookkeeping, `try_wait`, signal sending) — never
/// across an await.
static JOBS: LazyLock<Mutex<HashMap<String, BackgroundJob>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn new_process_id() -> String {
    format!("bg-{}", Uuid::new_v4())
}

/// A short human-readable description of an exit status (exit code or
/// terminating signal).
fn describe_exit(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => format!("terminated by {status}"),
    }
}

/// Run `command` via `sh -c` in `workdir` and return the process id.
///
/// The child runs in its own process group with stdin/stdout/stderr
/// connected to `/dev/null`; callers that care about output must redirect
/// it in the command itself.
fn run(workdir: &Path, command: &str) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group: `kill`/`kill_all` can then stop the whole
        // tree, not just the direct `sh` child.
        .process_group(0)
        // The process must outlive this `Child` handle (and the tool call);
        // `run` returns immediately and the job stays tracked in `JOBS`.
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("failed to spawn sh: {e}"))?;
    let Some(pgid) = child.id() else {
        // A successfully spawned child should always have a pid; guard
        // against the unexpected case without leaking the child.
        let _ = child.start_kill();
        return Err("spawned child has no pid".to_string());
    };
    let id = new_process_id();
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).insert(
        id.clone(),
        BackgroundJob {
            child: Some(child),
            pgid,
            finished: None,
        },
    );
    Ok(id)
}

/// Report whether the process identified by `process_id` is still running.
fn status(process_id: &str) -> Result<String, String> {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let job = jobs
        .get_mut(process_id)
        .ok_or_else(|| format!("unknown background process: {process_id}"))?;
    if let Some(status) = job.finished {
        return Ok(format!(
            "process {process_id} is not running ({})",
            describe_exit(&status)
        ));
    }
    let Some(child) = job.child.as_mut() else {
        return Err(format!("process {process_id} is no longer tracked"));
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            let description = describe_exit(&status);
            job.finished = Some(status);
            job.child = None;
            Ok(format!(
                "process {process_id} is not running ({description})"
            ))
        }
        Ok(None) => Ok(format!("process {process_id} is still running")),
        Err(e) => Err(format!(
            "failed to check background process {process_id}: {e}"
        )),
    }
}

/// Kill the process identified by `process_id` (and its whole process
/// group). Returns an acknowledgement; the job stays tracked so a later
/// `status` reports that it is no longer running.
fn kill(process_id: &str) -> Result<String, String> {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let job = jobs
        .get_mut(process_id)
        .ok_or_else(|| format!("unknown background process: {process_id}"))?;
    if let Some(status) = job.finished {
        return Ok(format!(
            "process {process_id} had already finished ({}); no kill was needed",
            describe_exit(&status)
        ));
    }
    if let Some(child) = job.child.as_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                let description = describe_exit(&status);
                job.finished = Some(status);
                job.child = None;
                return Ok(format!(
                    "process {process_id} had already finished ({description}); no kill was needed"
                ));
            }
            Ok(None) => {}
            Err(e) => {
                return Err(format!(
                    "failed to check background process {process_id}: {e}"
                ));
            }
        }
    }
    unsafe {
        // Kill the whole process group (the direct `sh` child is the group
        // leader, so `-pgid` and `pgid` cover the child and its descendants).
        libc::kill(-(job.pgid as i32), libc::SIGKILL);
        libc::kill(job.pgid as i32, libc::SIGKILL);
    }
    Ok(format!(
        "sent SIGKILL to background process {process_id} (pid {}) and its process group",
        job.pgid
    ))
}

/// Stop every still-running background job. Called when the worker exits
/// (both the SIGTERM cancel path and a normal run completion), so
/// long-running background commands never outlive the session that started
/// them.
pub(crate) fn kill_all() {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    for job in jobs.values_mut() {
        if job.finished.is_some() {
            continue;
        }
        let running = match job.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    job.finished = Some(status);
                    job.child = None;
                    false
                }
                Ok(None) => true,
                // If the child's status can't be read, assume it is still
                // running and kill the tracked group.
                Err(_) => true,
            },
            None => false,
        };
        if running {
            unsafe {
                libc::kill(-(job.pgid as i32), libc::SIGKILL);
                libc::kill(job.pgid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Dispatch one `bash_in_background` call. `command` is required for
/// `action=run`; `process_id` is required for `action=status`/`action=kill`.
pub(crate) fn execute(
    workdir: &Path,
    action: &str,
    command: Option<&str>,
    process_id: Option<&str>,
) -> Result<String, String> {
    match action {
        "run" => run(
            workdir,
            command.ok_or_else(|| "action `run` requires `command`".to_string())?,
        ),
        "status" => {
            status(process_id.ok_or_else(|| "action `status` requires `process_id`".to_string())?)
        }
        "kill" => {
            kill(process_id.ok_or_else(|| "action `kill` requires `process_id`".to_string())?)
        }
        other => Err(format!(
            "unknown action: {other} (expected `run`, `kill`, or `status`)"
        )),
    }
}

// Unit tests live in `mo_worker/src/tests/tools/bash_in_background_tests.rs`
// (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/bash_in_background_tests.rs"]
mod tests;
