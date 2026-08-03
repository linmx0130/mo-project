//! Worker process management: spawn (own process group), liveness, cancel.

use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mo_core::Session;
use tracing::warn;

use crate::state::AppState;

/// Heartbeat staleness threshold. The worker writes a heartbeat every 5s
/// from a dedicated task that is independent of tool execution, so a
/// `running` worker that has not heartbeated for this long is considered
/// *stalled*: the process is alive (the pid-based liveness check passes)
/// but its async runtime is not making progress. Without this signal a
/// wedged worker would leave the session `running` forever.
pub const HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(30);

/// True when the session's worker has stopped heartbeating (the process is
/// alive but wedged). A worker that never produced a heartbeat (`None`) is
/// not flagged here: a freshly spawned worker takes a moment to start, and
/// a worker that died before its first beat is caught by the pid check.
pub fn is_heartbeat_stale(heartbeat_at: &Option<String>) -> bool {
    let Some(ts) = heartbeat_at else {
        return false;
    };
    let Ok(t) = DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(t.with_timezone(&Utc));
    age > chrono::Duration::from_std(HEARTBEAT_STALE_AFTER).unwrap_or_default()
}

/// Spawn `mo_worker --session-id <id>` with stdout/stderr going to
/// `data/sessions/<id>/worker.log`. The worker gets its own process group
/// so cancel can signal the whole tree (including subagents). Returns the
/// child pid. A background task reaps the child so no zombies accumulate.
///
/// The worker's configuration travels through the environment: the session's
/// model (resolved from the config file by name; the default model is the
/// fallback), plus the data dir, global agents dir and subagent depth.
pub fn spawn_worker(state: &AppState, session: &Session) -> std::io::Result<u32> {
    let session_dir = state.data_dir.join("sessions").join(&session.id);
    std::fs::create_dir_all(&session_dir)?;
    let log_file = std::fs::File::create(session_dir.join("worker.log"))?;
    let log_err = log_file.try_clone()?;

    let mut cmd = tokio::process::Command::new(&state.worker_bin);
    cmd.arg("--session-id")
        .arg(&session.id)
        .env("MO_DATA_DIR", &state.data_dir)
        .env("MO_AGENTS_DIR", &state.agents_dir)
        .env("MO_SUBAGENT_DEPTH", state.subagent_depth.to_string())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .kill_on_drop(true);

    if let Some(model) = state
        .find_model(&session.model)
        .or_else(|| state.default_model())
    {
        cmd.env("MO_MODEL_BASE_URL", &model.base_url)
            .env("MO_MODEL_NAME", &model.name);
        if let Some(token) = &model.token {
            cmd.env("MO_AUTH_TOKEN", token);
        }
        // The context window travels as env so the worker can embed it in
        // `context_usage` journal events (the status bar renders the context
        // length against it); unset = unlimited.
        if let Some(window) = model.context_window {
            cmd.env("MO_CONTEXT_WINDOW", window.to_string());
        }
    }

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
    format!("worker died{}", worker_log_tail(data_dir, id))
}

/// "worker stalled" error: the process is alive but stopped heartbeating,
/// so its async runtime is presumably frozen — the pid-based liveness check
/// alone would keep the session `running` forever.
pub fn worker_stalled_error(
    data_dir: &std::path::Path,
    id: &str,
    heartbeat_at: &Option<String>,
) -> String {
    let age_secs = heartbeat_at
        .as_ref()
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| {
            Utc::now()
                .signed_duration_since(t.with_timezone(&Utc))
                .num_seconds()
        })
        .unwrap_or(-1);
    format!(
        "worker stalled: no heartbeat for {age_secs}s (process alive but unresponsive){}",
        worker_log_tail(data_dir, id)
    )
}

/// The tail of the session's worker log, or an empty string when there is
/// nothing to show. Shared by the "died" and "stalled" failure messages.
fn worker_log_tail(data_dir: &std::path::Path, id: &str) -> String {
    let log_path = data_dir.join("sessions").join(id).join("worker.log");
    std::fs::read_to_string(&log_path)
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
        .unwrap_or_default()
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

/// Permanently remove the per-session directory (`<data_dir>/sessions/<id>`:
/// journal, worker log, anything else the worker wrote there).
///
/// The id arrives from a URL path, so it is validated to be a single plain
/// path component and the resolved directory is re-checked to live under
/// `<data_dir>/sessions` before anything is deleted. Removing an already
/// missing directory is a no-op.
pub fn remove_session_dir(data_dir: &std::path::Path, id: &str) -> std::io::Result<()> {
    let is_plain_component =
        !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\\');
    if !is_plain_component {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid session id: {id:?}"),
        ));
    }
    let sessions_root = data_dir.join("sessions");
    let dir = sessions_root.join(id);
    // Defense in depth: the resolved path must be a child of the sessions
    // root (never the root itself).
    if dir == sessions_root || !dir.starts_with(&sessions_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to remove non-session path: {}", dir.display()),
        ));
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
