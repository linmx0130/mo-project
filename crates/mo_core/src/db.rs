//! SQLite metadata DB shared by the gateway and every worker process.
//!
//! WAL mode + a busy timeout let the gateway and multiple workers open the
//! same `data/mo.db` concurrently without blocking each other.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Mode, Session, SessionStatus};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid row: {0}")]
    Row(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Open (creating if needed) the DB, set WAL + busy timeout, run migrations.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DbError::Row(e.to_string()))?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            parent_id    TEXT NULL,
            workdir      TEXT NOT NULL,
            prompt       TEXT NOT NULL,
            model        TEXT NOT NULL,
            status       TEXT NOT NULL,
            pid          INTEGER NULL,
            journal_path TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL,
            heartbeat_at TEXT NULL,
            error        TEXT NULL
        );",
    )?;
    // Sessions created before modes existed have no `mode` column; add it
    // (default `build`) so every row round-trips through `Session`.
    let columns = table_columns(conn, "sessions")?;
    if !columns.iter().any(|c| c == "mode") {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'build';")?;
    }
    // Sessions created before tool selection existed have no `tools`
    // column; add it (`NULL` = no restriction = all tools enabled, the
    // legacy behavior) so every row round-trips through `Session`.
    if !columns.iter().any(|c| c == "tools") {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN tools TEXT;")?;
    }
    Ok(())
}

/// The column names of a table (`PRAGMA table_info`), used by migrations to
/// add new columns idempotently to databases created by older versions.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns)
}

pub fn create_session(conn: &Connection, session: &Session) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, parent_id, workdir, prompt, model, status, mode, tools, pid, journal_path, created_at, updated_at, heartbeat_at, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            session.id,
            session.parent_id,
            session.workdir,
            session.prompt,
            session.model,
            session.status.as_str(),
            session.mode.as_str(),
            serde_json::to_string(&session.tools).unwrap_or_default(),
            session.pid.map(|p| p as i64),
            session.journal_path,
            session.created_at,
            session.updated_at,
            session.heartbeat_at,
            session.error,
        ],
    )?;
    Ok(())
}

pub fn get_session(conn: &Connection, id: &str) -> Result<Option<Session>> {
    let row = conn
        .query_row(
            "SELECT id, parent_id, workdir, prompt, model, status, mode, tools, pid, journal_path, created_at, updated_at, heartbeat_at, error
             FROM sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .optional()?;
    Ok(row)
}

/// List root sessions (newest first). Subagent sessions (`parent_id`
/// set) are excluded: they are hidden from the session list — they are
/// reached through their parent's `spawn_subagent` tool blocks instead
/// (see the frontend's subagent modal).
pub fn list_sessions(conn: &Connection) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, workdir, prompt, model, status, mode, tools, pid, journal_path, created_at, updated_at, heartbeat_at, error
         FROM sessions WHERE parent_id IS NULL ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_session)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// List the subagent sessions spawned by a session (oldest first). Used by
/// the gateway when cancelling/deleting a session: its subagents must be
/// stopped and cleaned up with it.
pub fn list_children(conn: &Connection, parent_id: &str) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, workdir, prompt, model, status, mode, tools, pid, journal_path, created_at, updated_at, heartbeat_at, error
         FROM sessions WHERE parent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt
        .query_map(params![parent_id], row_to_session)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn update_status(
    conn: &Connection,
    id: &str,
    status: SessionStatus,
    error: Option<String>,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
        params![status.as_str(), error, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

pub fn update_heartbeat(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET heartbeat_at = ?1, updated_at = ?2 WHERE id = ?3",
        params![
            chrono::Utc::now().to_rfc3339(),
            chrono::Utc::now().to_rfc3339(),
            id
        ],
    )?;
    Ok(())
}

pub fn set_pid(conn: &Connection, id: &str, pid: u32) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET pid = ?1, updated_at = ?2 WHERE id = ?3",
        params![pid as i64, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// Clear the pid (and any error) before re-running a terminal session, so
/// the gateway's liveness check does not see the stale pid of a dead worker
/// and flip the freshly-queued session to `failed`. The stale heartbeat is
/// cleared too: it belongs to the previous worker, and the stall check must
/// not flag the freshly-spawned worker before its first heartbeat lands.
pub fn clear_pid(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET pid = NULL, error = NULL, heartbeat_at = NULL, updated_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// Permanently remove a session row. Returns `true` if a row was deleted
/// (i.e. the session existed), `false` for an unknown id.
pub fn delete_session(conn: &Connection, id: &str) -> Result<bool> {
    let deleted = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
    Ok(deleted > 0)
}

/// Overwrite the session title (the `prompt` column). Used by the gateway
/// when a generated title lands after session creation.
pub fn set_prompt(conn: &Connection, id: &str, prompt: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET prompt = ?1, updated_at = ?2 WHERE id = ?3",
        params![prompt, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// Switch the session's mode. Only the write-sandbox policy of subsequent
/// runs is affected: the system prompt is journaled at the first run and is
/// never rebuilt, so switching modes never changes it.
pub fn update_mode(conn: &Connection, id: &str, mode: Mode) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET mode = ?1, updated_at = ?2 WHERE id = ?3",
        params![mode.as_str(), chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// Switch the session's model (the value the UI sends from `/api/models`).
/// Only the next run is affected: the worker that respawns for the next
/// followup is spawned with the new model's env, and the gateway injects a
/// `ModelChange` notice before that run if the model differs from the
/// model of the last run. The journaled system prompt is model-agnostic
/// and is never rebuilt.
pub fn update_model(conn: &Connection, id: &str, model: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
        params![model, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let status_str: String = row.get(5)?;
    let status = SessionStatus::from_str(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
    })?;
    let mode_str: String = row.get(6)?;
    let mode = Mode::from_str(&mode_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, e.into())
    })?;
    // The enabled-tool list is a JSON array; `NULL` (rows created before
    // tool selection existed) means "no restriction" → empty list, which
    // the worker treats as "all tools enabled" (see `mo_core::tools`).
    let tools: Option<String> = row.get(7)?;
    let tools = tools
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default();
    let pid: Option<i64> = row.get(8)?;
    Ok(Session {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        workdir: row.get(2)?,
        prompt: row.get(3)?,
        model: row.get(4)?,
        status,
        mode,
        tools,
        pid: pid.map(|p| p as u32),
        journal_path: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        heartbeat_at: row.get(12)?,
        error: row.get(13)?,
    })
}

// Unit tests live in `mo_core/src/tests/db_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/db_tests.rs"]
mod tests;
