//! Worker process management: spawn (own process group), liveness, cancel.

use std::process::Stdio;
use std::time::Duration;

use mo_core::Session;
use tracing::warn;

use crate::state::AppState;

/// Spawn `mo_worker --session-id <id>` with stdout/stderr going to
/// `data/sessions/<id>/worker.log`. The worker gets its own process group
/// so cancel can signal the whole tree (including subagents). Returns the
/// child pid. A background task reaps the child so no zombies accumulate.
pub fn spawn_worker(state: &AppState, session: &Session) -> std::io::Result<u32> {
    let session_dir = state.data_dir.join("sessions").join(&session.id);
    std::fs::create_dir_all(&session_dir)?;
    let log_file = std::fs::File::create(session_dir.join("worker.log"))?;
    let log_err = log_file.try_clone()?;

    let mut cmd = tokio::process::Command::new(&state.worker_bin);
    cmd.arg("--session-id")
        .arg(&session.id)
        .env("MO_DATA_DIR", &state.data_dir)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("child pid unavailable"))?;
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => warn!(pid, "worker exited: {status}"),
            Err(e) => warn!(pid, "failed to wait for worker: {e}"),
        }
    });
    Ok(pid)
}

/// Cheap liveness check: `kill(pid, 0)`.
pub fn is_pid_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    // EPERM means the process exists but belongs to another user.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// "worker died" error enriched with the tail of the worker's log so the
/// UI surfaces the real cause (e.g. a missing env var or a startup error).
pub fn worker_died_error(data_dir: &std::path::Path, id: &str) -> String {
    let log_path = data_dir.join("sessions").join(id).join("worker.log");
    let tail = std::fs::read_to_string(&log_path)
        .ok()
        .map(|content| {
            content
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|tail| !tail.trim().is_empty())
        .map(|tail| format!(": {tail}"))
        .unwrap_or_default();
    format!("worker died{tail}")
}

/// SIGTERM the process group, give it a grace period, then SIGKILL the
/// group and the pid. The worker runs in its own process group (pgid ==
/// pid), so subagents spawned by it are covered too.
pub async fn cancel_session_pid(pid: u32) {
    let group = -(pid as i32);
    unsafe {
        libc::kill(group, libc::SIGTERM);
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    unsafe {
        libc::kill(group, libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}
