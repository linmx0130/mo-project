//! SQLite metadata DB shared by the gateway and every worker process.
//!
//! WAL mode + a busy timeout let the gateway and multiple workers open the
//! same `data/mo.db` concurrently without blocking each other.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Session, SessionStatus};

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
    Ok(())
}

pub fn create_session(conn: &Connection, session: &Session) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, parent_id, workdir, prompt, model, status, pid, journal_path, created_at, updated_at, heartbeat_at, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            session.id,
            session.parent_id,
            session.workdir,
            session.prompt,
            session.model,
            session.status.as_str(),
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
            "SELECT id, parent_id, workdir, prompt, model, status, pid, journal_path, created_at, updated_at, heartbeat_at, error
             FROM sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .optional()?;
    Ok(row)
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, workdir, prompt, model, status, pid, journal_path, created_at, updated_at, heartbeat_at, error
         FROM sessions ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_session)?
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
/// and flip the freshly-queued session to `failed`.
pub fn clear_pid(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET pid = NULL, error = NULL, updated_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// Set `prompt` only when it is currently empty (a session's first message
/// becomes its title; followups leave the title untouched).
pub fn set_prompt_if_empty(conn: &Connection, id: &str, prompt: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET prompt = ?1, updated_at = ?2 WHERE id = ?3 AND prompt = ''",
        params![prompt, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let status_str: String = row.get(5)?;
    let status = SessionStatus::from_str(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
    })?;
    let pid: Option<i64> = row.get(6)?;
    Ok(Session {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        workdir: row.get(2)?,
        prompt: row.get(3)?,
        model: row.get(4)?,
        status,
        pid: pid.map(|p| p as u32),
        journal_path: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        heartbeat_at: row.get(10)?,
        error: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;

    fn sample_session(id: &str) -> Session {
        let now = chrono::Utc::now().to_rfc3339();
        Session {
            id: id.to_string(),
            parent_id: None,
            workdir: "/tmp/work".to_string(),
            prompt: "do the thing".to_string(),
            model: "test-model".to_string(),
            status: SessionStatus::Pending,
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
}
