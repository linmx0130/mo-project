//! `spawn_subagent` tool: spawn a child `mo_worker` process for a nested
//! session, wait for it to finish, and return its final assistant message.
//!
//! The parent picks the subagent's mode: `build` (full access), or
//! `plan`/`explore` (codebase read-only, writes go to the subagent's own
//! scratch dir). The child journals its own system prompt on its first run,
//! built from the chosen mode.
//!
//! Subagents are *leaves*: the depth hard limit is 1, so a session that is
//! itself a subagent (`parent_id` set) can never spawn further subagents.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mo_core::{
    JournalEventKind, JournalMessage, JournalWriter, Mode, Session, SessionStatus, db, open_db,
};
use uuid::Uuid;

use crate::config::MAX_SUBAGENT_DEPTH;
use crate::tools::ToolContext;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_WAIT: Duration = Duration::from_secs(20 * 60);

pub async fn spawn_subagent(
    ctx: &ToolContext,
    prompt: &str,
    mode: Mode,
    tool_call_id: &str,
    on_event: &(dyn Fn(JournalEventKind) + Send + Sync),
) -> Result<String, String> {
    // Hard limit: subagents cannot spawn further subagents. A session with
    // a `parent_id` is a subagent, so it is refused here regardless of the
    // numeric depth value it carries.
    if ctx.session.parent_id.is_some() {
        return Err("subagents cannot spawn further subagents (depth hard limit 1)".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }

    // 1. Insert the child session row, seed its journal with the task as
    //    its first user message, and journal a `SubagentStarted` event into
    //    the parent's journal (links the tool block to the child session).
    let id = create_child_session(ctx, prompt, mode, tool_call_id, on_event)?;

    // 2. Spawn the child worker (same binary, child session id, depth + 1
    //    clamped to the hard cap so the system-prompt framing never claims
    //    a deeper nesting).
    let session_dir = ctx.data_dir.join("sessions").join(&id);
    std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
    let log_file =
        std::fs::File::create(session_dir.join("worker.log")).map_err(|e| e.to_string())?;
    let stderr_file = log_file.try_clone().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot resolve worker executable path: {e}"))?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("--session-id")
        .arg(&id)
        .env(
            "MO_SUBAGENT_DEPTH",
            (ctx.subagent_depth + 1).min(MAX_SUBAGENT_DEPTH).to_string(),
        )
        .env("MO_DATA_DIR", &ctx.data_dir)
        .env("MO_AGENTS_DIR", &ctx.agents_dir)
        .env("MO_MODEL_BASE_URL", &ctx.model_base_url)
        .env("MO_MODEL_NAME", &ctx.model_name)
        .env(
            "MO_MAX_TOOL_CONCURRENCY",
            ctx.max_tool_concurrency.to_string(),
        )
        // The parent's resolved model window + compression threshold, so a
        // subagent compresses against the same settings (the config-file
        // fallback would only match when the parent uses the default model).
        .env(
            "MO_CONTEXT_COMPRESSION_THRESHOLD",
            ctx.context_compression_threshold.to_string(),
        )
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);
    if let Some(token) = &ctx.auth_token {
        cmd.env("MO_AUTH_TOKEN", token);
    }
    if let Some(window) = ctx.context_window {
        cmd.env("MO_CONTEXT_WINDOW", window.to_string());
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

/// Create the child session: insert the row, seed the child's journal with
/// the task as its first user message, and journal a `SubagentStarted`
/// event into the parent's journal (via `on_event`, so the frontend can
/// link the `spawn_subagent` tool block to the child session).
///
/// The child's *title* (the `prompt` column) is not the task text: a
/// subagent has no user-facing title of its own. The DB still needs a
/// value, so it reads `Subagent for <parent session title>`.
///
/// The task prompt is seeded into the child journal exactly like the
/// gateway seeds a root session's first user message — the worker rebuilds
/// its conversation context from the journal, so this is what delivers the
/// task to the child.
fn create_child_session(
    ctx: &ToolContext,
    prompt: &str,
    mode: Mode,
    tool_call_id: &str,
    on_event: &(dyn Fn(JournalEventKind) + Send + Sync),
) -> Result<String, String> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let session_dir = ctx.data_dir.join("sessions").join(&id);
    let journal_path = session_dir.join("journal.jsonl");
    let title = if ctx.session.prompt.trim().is_empty() {
        "Subagent".to_string()
    } else {
        format!("Subagent for {}", ctx.session.prompt)
    };
    let child_session = Session {
        id: id.clone(),
        parent_id: Some(ctx.session.id.clone()),
        workdir: ctx.workdir.display().to_string(),
        prompt: title,
        model: ctx.model_name.clone(),
        status: SessionStatus::Pending,
        mode,
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
    on_event(JournalEventKind::SubagentStarted {
        child_id: id.clone(),
        tool_call_id: tool_call_id.to_string(),
        mode,
    });
    let mut journal = JournalWriter::open(&journal_path).map_err(|e| e.to_string())?;
    journal
        .append(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
        .map_err(|e| e.to_string())?;
    Ok(id)
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
    use std::sync::{Arc, Mutex};

    /// A ctx whose data dir is a tempdir (so `create_child_session` can
    /// write the DB + journal) and whose session is a *root* session
    /// (`parent_id` None) unless overridden.
    fn test_ctx(dir: &tempfile::TempDir, parent_id: Option<String>) -> ToolContext {
        let session = Session {
            id: "parent".into(),
            parent_id,
            workdir: "/tmp".into(),
            prompt: "parent title".into(),
            model: "m".into(),
            status: SessionStatus::Running,
            mode: Mode::Build,
            pid: None,
            journal_path: "/tmp/j.jsonl".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            heartbeat_at: None,
            error: None,
        };
        ToolContext {
            workdir: std::path::PathBuf::from("/tmp"),
            data_dir: dir.path().join("data"),
            agents_dir: dir.path().join("agents"),
            session,
            scratch: dir.path().join("data/sessions/parent/tmp"),
            subagent_depth: 0,
            max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
            model_base_url: "http://localhost:1".into(),
            model_name: "m".into(),
            auth_token: None,
            context_window: Some(4096),
            context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
        }
    }

    /// A session that is itself a subagent (`parent_id` set) is refused:
    /// the depth hard limit is 1, so subagents are leaves.
    #[test]
    fn subagent_cannot_spawn_further() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, Some("root".to_string()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let no_event = |_: JournalEventKind| {};
        let err = rt
            .block_on(spawn_subagent(
                &ctx,
                "do it",
                Mode::Explore,
                "call_x",
                &no_event,
            ))
            .unwrap_err();
        assert!(
            err.contains("subagents cannot spawn further subagents"),
            "got: {err}"
        );
        assert!(err.contains("hard limit 1"), "got: {err}");
    }

    /// Creating a child session: the DB row carries the `Subagent for …`
    /// title (never the task text), the child journal is seeded with the
    /// task as its first user message, and the parent journal receives a
    /// `SubagentStarted` event linking the tool block to the child.
    #[test]
    fn create_child_session_titles_and_seeds_journal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, None);
        let events = Arc::new(Mutex::new(Vec::<JournalEventKind>::new()));
        let on_event = {
            let events = Arc::clone(&events);
            move |kind: JournalEventKind| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
            }
        };

        let id = create_child_session(
            &ctx,
            "subagent task text",
            Mode::Explore,
            "call_x",
            &on_event,
        )
        .unwrap();

        // The row: parent set, title "Subagent for <parent title>", mode
        // as chosen by the parent.
        let conn = open_db(&ctx.data_dir.join("mo.db")).unwrap();
        let row = db::get_session(&conn, &id).unwrap().unwrap();
        assert_eq!(row.parent_id.as_deref(), Some("parent"));
        assert_eq!(row.prompt, "Subagent for parent title");
        assert_eq!(row.mode, Mode::Explore);
        assert_eq!(row.status, SessionStatus::Pending);
        drop(conn);

        // The child journal's first (and only) event is the task as a user
        // message — the worker rebuilds its context from the journal.
        let journal_path = ctx
            .data_dir
            .join("sessions")
            .join(&id)
            .join("journal.jsonl");
        let child_events = mo_core::read_events(&journal_path).unwrap();
        assert_eq!(child_events.len(), 1, "events: {child_events:#?}");
        match &child_events[0].kind {
            JournalEventKind::Message(m) => {
                assert_eq!(m.role, "user");
                assert_eq!(m.content, "subagent task text");
            }
            other => panic!("expected user message, got: {other:?}"),
        }

        // The SubagentStarted event links the tool block to the child.
        let events = events.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(events.len(), 1, "events: {events:#?}");
        match &events[0] {
            JournalEventKind::SubagentStarted {
                child_id,
                tool_call_id,
                mode,
            } => {
                assert_eq!(child_id, &id);
                assert_eq!(tool_call_id, "call_x");
                assert_eq!(*mode, Mode::Explore);
            }
            other => panic!("expected subagent_started, got: {other:?}"),
        }
    }

    /// A parent without a title yields the bare "Subagent" fallback.
    #[test]
    fn create_child_session_falls_back_to_bare_subagent_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(&dir, None);
        ctx.session.prompt = "   ".to_string();
        let no_event = |_: JournalEventKind| {};
        let id = create_child_session(&ctx, "task", Mode::Build, "c1", &no_event).unwrap();
        let conn = open_db(&ctx.data_dir.join("mo.db")).unwrap();
        let row = db::get_session(&conn, &id).unwrap().unwrap();
        assert_eq!(row.prompt, "Subagent");
    }
}
