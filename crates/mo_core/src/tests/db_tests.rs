//! Unit tests for the `db` module — production code lives in
//! `mo_core/src/db.rs`. Wired from there with `#[cfg(test)] #[path = "tests/db_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

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
