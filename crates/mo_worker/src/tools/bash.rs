//! `bash` tool: run a command via `sh -c` inside the workdir.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

const OUTPUT_CAP: usize = 1024 * 1024; // 1 MB

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Run `command` via `sh -c` in `workdir`. Returns stdout+stderr plus the
/// exit code, capped at ~1 MB. On timeout the child is killed and an error
/// is returned.
pub async fn bash(workdir: &Path, command: &str, timeout: Duration) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn sh: {e}"))?;

    let run = async {
        child.wait_with_output().await.map_err(|e| format!("failed to wait for command: {e}"))
    };
    let output = match tokio::time::timeout(timeout, run).await {
        Err(_) => {
            return Err(format!(
                "command timed out after {}s: {command}",
                timeout.as_secs()
            ))
        }
        Ok(result) => result?,
    };

    let mut text = String::new();
    text.push_str(&format!(
        "exit code: {}\n",
        output.status.code().unwrap_or(-1)
    ));
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        text.push_str(&format!("[stderr]\n{}", String::from_utf8_lossy(&output.stderr)));
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
        let out = bash(dir.path(), "echo hello && exit 3", Duration::from_secs(10))
            .await
            .unwrap();
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("exit code: 3"), "got: {out}");
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = bash(dir.path(), "echo boom 1>&2", Duration::from_secs(10))
            .await
            .unwrap();
        assert!(out.contains("boom"), "got: {out}");
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let dir = tempfile::tempdir().unwrap();
        let err = bash(dir.path(), "sleep 30", Duration::from_millis(300)).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("timed out"));
    }
}
