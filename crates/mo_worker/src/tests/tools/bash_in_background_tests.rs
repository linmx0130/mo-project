//! Unit tests for the `tools::bash_in_background` module — production code
//! lives in `mo_worker/src/tools/bash_in_background.rs`. Wired from there
//! with `#[cfg(test)] #[path = "../tests/tools/bash_in_background_tests.rs"]
//! mod tests;` so the tests keep `use super::*` access to the module's items
//! (private ones included).

use super::*;
use std::time::Duration;

/// Kills the job on drop so a failing assertion never leaves a `sleep`
/// orphan behind.
struct KillOnDrop(String);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = kill(&self.0);
    }
}

#[tokio::test]
async fn run_returns_process_id_and_status_reports_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    // A unique sleep argument: other tests in this binary also spawn
    // `sleep` (concurrently, under `cargo test`'s parallel threads), so a
    // broad pattern would flag a peer test's process as this test's orphan.
    let id = run(dir.path(), "sleep 30.987").unwrap();
    assert!(id.starts_with("bg-"), "got: {id}");
    let _guard = KillOnDrop(id.clone());

    // The command is alive right after spawning.
    let running = status(&id).unwrap();
    assert!(running.contains("still running"), "got: {running}");

    // `kill` acknowledges, and `status` then reports the process is gone.
    let ack = kill(&id).unwrap();
    assert!(ack.contains("SIGKILL"), "got: {ack}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let s = status(&id).unwrap();
        if s.contains("not running") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "process still running after kill: {s}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The kill must reach the whole process group, not just the direct
    // `sh` child — no orphaned `sleep` may survive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("pgrep -f '^sleep 30\\.987$' || true")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "orphaned sleep survived the kill: {stdout}"
    );
}

#[tokio::test]
async fn status_reports_exit_code_after_command_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let id = run(dir.path(), "exit 7").unwrap();
    let _guard = KillOnDrop(id.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let s = status(&id).unwrap();
        if s.contains("not running") {
            assert!(s.contains("7"), "got: {s}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "process never exited: {s}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn unknown_process_id_errors() {
    let status_err = status("bg-nope").unwrap_err();
    assert!(
        status_err.contains("unknown background process"),
        "got: {status_err}"
    );
    let kill_err = kill("bg-nope").unwrap_err();
    assert!(
        kill_err.contains("unknown background process"),
        "got: {kill_err}"
    );
}

#[tokio::test]
async fn run_rejects_empty_command() {
    let dir = tempfile::tempdir().unwrap();
    let err = run(dir.path(), "   ").unwrap_err();
    assert!(err.contains("command must not be empty"), "got: {err}");
}

#[tokio::test]
async fn execute_validates_action_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let err = execute(dir.path(), "nope", None, None).unwrap_err();
    assert!(err.contains("unknown action"), "got: {err}");
    let err = execute(dir.path(), "run", None, None).unwrap_err();
    assert!(err.contains("requires `command`"), "got: {err}");
    let err = execute(dir.path(), "status", None, None).unwrap_err();
    assert!(err.contains("requires `process_id`"), "got: {err}");
    let err = execute(dir.path(), "kill", None, None).unwrap_err();
    assert!(err.contains("requires `process_id`"), "got: {err}");
}
