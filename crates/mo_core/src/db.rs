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
        "INSERT INTO sessions (id, parent_id, workdir, prompt, model, status, mode, pid, journal_path, created_at, updated_at, heartbeat_at, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            session.id,
            session.parent_id,
            session.workdir,
            session.prompt,
            session.model,
            session.status.as_str(),
            session.mode.as_str(),
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
            "SELECT id, parent_id, workdir, prompt, model, status, mode, pid, journal_path, created_at, updated_at, heartbeat_at, error
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
        "SELECT id, parent_id, workdir, prompt, model, status, mode, pid, journal_path, created_at, updated_at, heartbeat_at, error
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
        "SELECT id, parent_id, workdir, prompt, model, status, mode, pid, journal_path, created_at, updated_at, heartbeat_at, error
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

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let status_str: String = row.get(5)?;
    let status = SessionStatus::from_str(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
    })?;
    let mode_str: String = row.get(6)?;
    let mode = Mode::from_str(&mode_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, e.into())
    })?;
    let pid: Option<i64> = row.get(7)?;
    Ok(Session {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        workdir: row.get(2)?,
        prompt: row.get(3)?,
        model: row.get(4)?,
        status,
        mode,
        pid: pid.map(|p| p as u32),
        journal_path: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        heartbeat_at: row.get(11)?,
        error: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mode, SessionStatus};

    fn sample_session(id: &str) -> Session {
        let now = chrono::Utc::now().to_rfc3339();
        Session {
            id: id.to_string(),
            parent_id: None,
            workdir: "/tmp/work".to_string(),
            prompt: "do the thing".to_string(),
            model: "test-model".to_string(),
            status: SessionStatus::Pending,
            mode: Mode::Build,
            pid: None,
            journal_path: format!("/tmp/work/{id}/journal.jsonl"),
            created_at: now.clone(),
            updated_at: now.clone(),
            heartbeat_at: None,
            error: None,
        }
    }

    #[test]
    fn crud_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        let session = sample_session("s1");
        create_session(&conn, &session).unwrap();

        let fetched = get_session(&conn, "s1").unwrap().expect("session exists");
        assert_eq!(fetched, session);

        update_status(&conn, "s1", SessionStatus::Running, None).unwrap();
        let fetched = get_session(&conn, "s1").unwrap().unwrap();
        assert_eq!(fetched.status, SessionStatus::Running);
        assert!(fetched.updated_at >= session.updated_at);

        set_pid(&conn, "s1", 4242).unwrap();
        assert_eq!(get_session(&conn, "s1").unwrap().unwrap().pid, Some(4242));

        update_heartbeat(&conn, "s1").unwrap();
        assert!(
            get_session(&conn, "s1")
                .unwrap()
                .unwrap()
                .heartbeat_at
                .is_some()
        );

        update_status(&conn, "s1", SessionStatus::Failed, Some("boom".into())).unwrap();
        let fetched = get_session(&conn, "s1").unwrap().unwrap();
        assert_eq!(fetched.status, SessionStatus::Failed);
        assert_eq!(fetched.error.as_deref(), Some("boom"));

        assert!(get_session(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        create_session(&conn, &sample_session("old")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        create_session(&conn, &sample_session("new")).unwrap();

        let sessions = list_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "new");
        assert_eq!(sessions[1].id, "old");
    }

    /// Subagent sessions (`parent_id` set) are hidden from the session
    /// list; they are reachable through `list_children` instead.
    #[test]
    fn list_excludes_subagents_and_lists_children() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        let mut root = sample_session("root");
        root.prompt = "the root session".to_string();
        create_session(&conn, &root).unwrap();
        let mut child = sample_session("child");
        child.parent_id = Some("root".to_string());
        child.prompt = "Subagent for the root session".to_string();
        create_session(&conn, &child).unwrap();
        // A grandchild (defensive: depth is capped at 1, but the query must
        // hide any nested session).
        let mut grandchild = sample_session("grandchild");
        grandchild.parent_id = Some("child".to_string());
        create_session(&conn, &grandchild).unwrap();

        // Only the root appears in the list.
        let sessions = list_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 1, "sessions: {sessions:#?}");
        assert_eq!(sessions[0].id, "root");

        // list_children returns each parent's direct children.
        let children = list_children(&conn, "root").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "child");
        let children = list_children(&conn, "child").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "grandchild");
        assert!(list_children(&conn, "missing").unwrap().is_empty());
    }

    #[test]
    fn set_prompt_overwrites_title() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        create_session(&conn, &sample_session("s1")).unwrap();

        set_prompt(&conn, "s1", "generated title").unwrap();
        let fetched = get_session(&conn, "s1").unwrap().unwrap();
        assert_eq!(fetched.prompt, "generated title");
        assert!(fetched.updated_at >= fetched.created_at);

        // Unknown session is a no-op (no rows updated).
        set_prompt(&conn, "missing", "nope").unwrap();
        assert!(get_session(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn delete_session_removes_row() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        create_session(&conn, &sample_session("s1")).unwrap();
        create_session(&conn, &sample_session("s2")).unwrap();

        assert!(delete_session(&conn, "s1").unwrap());
        assert!(get_session(&conn, "s1").unwrap().is_none());
        assert!(get_session(&conn, "s2").unwrap().is_some());

        // Deleting again (or an unknown id) reports no row deleted.
        assert!(!delete_session(&conn, "s1").unwrap());
        assert!(!delete_session(&conn, "missing").unwrap());
    }

    #[test]
    fn reopen_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mo.db");
        {
            let conn = open(&path).unwrap();
            create_session(&conn, &sample_session("s1")).unwrap();
        }
        let conn = open(&path).unwrap();
        assert!(get_session(&conn, "s1").unwrap().is_some());
    }

    #[test]
    fn update_mode_switches_mode() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("mo.db")).unwrap();
        create_session(&conn, &sample_session("s1")).unwrap();

        update_mode(&conn, "s1", Mode::Plan).unwrap();
        let fetched = get_session(&conn, "s1").unwrap().unwrap();
        assert_eq!(fetched.mode, Mode::Plan);
        assert!(fetched.updated_at >= fetched.created_at);

        // Unknown session is a no-op (no rows updated).
        update_mode(&conn, "missing", Mode::Explore).unwrap();
        assert!(get_session(&conn, "missing").unwrap().is_none());
    }

    /// A database created before modes existed (no `mode` column) must be
    /// migrated in place: the column is added and existing rows default to
    /// `build`, so old sessions keep working.
    #[test]
    fn migration_adds_mode_column_to_legacy_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mo.db");
        // Create a legacy-schema DB by hand (no mode column) with one row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, parent_id TEXT NULL, workdir TEXT NOT NULL,
                    prompt TEXT NOT NULL, model TEXT NOT NULL, status TEXT NOT NULL,
                    pid INTEGER NULL, journal_path TEXT NOT NULL,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    heartbeat_at TEXT NULL, error TEXT NULL
                );
                INSERT INTO sessions (id, workdir, prompt, model, status, journal_path, created_at, updated_at)
                VALUES ('legacy', '/tmp', 'old session', 'm', 'completed', '/tmp/j.jsonl', 't', 't');",
            )
            .unwrap();
        }
        // Opening through `open` migrates it.
        let conn = open(&path).unwrap();
        let fetched = get_session(&conn, "legacy").unwrap().unwrap();
        assert_eq!(fetched.mode, Mode::Build, "legacy rows default to build");
        assert_eq!(fetched.status, SessionStatus::Completed);

        // Re-opening is idempotent (no duplicate column error).
        drop(conn);
        let conn = open(&path).unwrap();
        let fetched = get_session(&conn, "legacy").unwrap().unwrap();
        assert_eq!(fetched.mode, Mode::Build);
    }
}
