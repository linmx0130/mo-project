//! `spawn_subagent` tool: spawn a child `mo_worker` process for a nested
//! session, wait for it to finish, and return its final assistant message.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mo_core::{JournalEventKind, Session, SessionStatus, db, open_db};
use uuid::Uuid;

use crate::config::MAX_SUBAGENT_DEPTH;
use crate::tools::ToolContext;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_WAIT: Duration = Duration::from_secs(20 * 60);

pub async fn spawn_subagent(ctx: &ToolContext, prompt: &str) -> Result<String, String> {
    if ctx.subagent_depth >= MAX_SUBAGENT_DEPTH {
        return Err(format!(
            "subagent depth cap ({MAX_SUBAGENT_DEPTH}) reached; cannot spawn further subagents"
        ));
    }
    if prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }

    // 1. Insert the child session row (same workdir, parent = self).
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let session_dir = ctx.data_dir.join("sessions").join(&id);
    let journal_path = session_dir.join("journal.jsonl");
    let child_session = Session {
        id: id.clone(),
        parent_id: Some(ctx.session.id.clone()),
        workdir: ctx.workdir.display().to_string(),
        prompt: prompt.to_string(),
        model: ctx.model_name.clone(),
        status: SessionStatus::Pending,
        pid: None,
        journal_path: journal_path.display().to_string(),
        created_at: now.clone(),
        updated_at: now,
        heartbeat_at: None,
        error: None,
    };
    {
        let conn = open_db(&ctx.data_dir.join("mo.db")).map_err(|e| e.to_string())?;
        db::create_session(&conn, &child_session).map_err(|e| e.to_string())?;
    }

    // 2. Spawn the child worker (same binary, child session id, depth + 1).
    std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
    let log_file =
        std::fs::File::create(session_dir.join("worker.log")).map_err(|e| e.to_string())?;
    let stderr_file = log_file.try_clone().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot resolve worker executable path: {e}"))?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("--session-id")
        .arg(&id)
        .env("MO_SUBAGENT_DEPTH", (ctx.subagent_depth + 1).to_string())
        .env("MO_DATA_DIR", &ctx.data_dir)
        .env("MO_AGENTS_DIR", &ctx.agents_dir)
        .env("MO_MODEL_BASE_URL", &ctx.model_base_url)
        .env("MO_MODEL_NAME", &ctx.model_name)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);
    if let Some(token) = &ctx.auth_token {
        cmd.env("MO_AUTH_TOKEN", token);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn subagent worker: {e}"))?;

    // 3. Poll the child session row until terminal, then read its journal.
    let deadline = Instant::now() + MAX_WAIT;
    loop {
        if Instant::now() > deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("subagent did not finish within 20 minutes".to_string());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        let fetched = {
            let conn = open_db(&ctx.data_dir.join("mo.db")).map_err(|e| e.to_string())?;
            db::get_session(&conn, &id).map_err(|e| e.to_string())?
        };
        let Some(sess) = fetched else {
            return Err("subagent session row disappeared".to_string());
        };
        if !sess.status.is_terminal() {
            continue;
        }
        let _ = child.wait().await; // reap
        let final_message = last_assistant_message(Path::new(&sess.journal_path));
        let status = sess.status.as_str();
        let error_note = sess
            .error
            .as_deref()
            .map(|e| format!("\n[subagent error] {e}"))
            .unwrap_or_default();
        return match final_message {
            Some(msg) if !msg.trim().is_empty() => {
                Ok(format!("[subagent {status}] {msg}{error_note}"))
            }
            _ => Ok(format!(
                "[subagent {status} — no final message]{error_note}"
            )),
        };
    }
}

fn last_assistant_message(journal_path: &Path) -> Option<String> {
    let events = mo_core::read_events(journal_path).ok()?;
    events.iter().rev().find_map(|e| match &e.kind {
        JournalEventKind::Message(m) if m.role == "assistant" => Some(m.content.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_cap_refuses() {
        let ctx = ToolContext {
            workdir: std::path::PathBuf::from("/tmp"),
            data_dir: std::path::PathBuf::from("/tmp/data"),
            agents_dir: std::path::PathBuf::from("/tmp/agents"),
            session: Session {
                id: "s".into(),
                parent_id: None,
                workdir: "/tmp".into(),
                prompt: "p".into(),
                model: "m".into(),
                status: SessionStatus::Running,
                pid: None,
                journal_path: "/tmp/j.jsonl".into(),
                created_at: "now".into(),
                updated_at: "now".into(),
                heartbeat_at: None,
                error: None,
            },
            subagent_depth: MAX_SUBAGENT_DEPTH,
            model_base_url: "http://localhost:1".into(),
            model_name: "m".into(),
            auth_token: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(spawn_subagent(&ctx, "do it")).unwrap_err();
        assert!(err.contains("depth cap"), "got: {err}");
    }
}
