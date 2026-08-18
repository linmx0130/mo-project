//! Unit tests for the `ask_user` tool — production code lives in
//! `mo_worker/src/tools/ask_user.rs`. Wired from there with `#[cfg(test)]
//! #[path = "../tests/tools/ask_user_tests.rs"] mod tests;` so the tests keep
//! `use super::*` access to the module's items (private ones included).

use super::*;
use crate::tools::{TOOL_ASK_USER, execute_tool};
use mo_core::{AskUserQuestion, JournalEventKind, Mode, Session};

/// A root-session ctx with a real journal file, for the `ask_user` tests
/// (the tool reads the journal to decide whether a request is pending).
fn ask_ctx(dir: &tempfile::TempDir, parent_id: Option<String>) -> ToolContext {
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let scratch = dir.path().join("data/sessions/s/tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let mut ctx = ToolContext {
        workdir,
        data_dir: dir.path().join("data"),
        agents_dir: dir.path().join("agents"),
        session: Session {
            id: "s".into(),
            parent_id,
            workdir: dir.path().join("work").display().to_string(),
            prompt: "p".into(),
            model: "m".into(),
            status: mo_core::SessionStatus::Running,
            mode: Mode::Build,
            tools: vec![],
            skills: vec![],
            pid: None,
            journal_path: "/tmp/j.jsonl".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            heartbeat_at: None,
            error: None,
        },
        scratch,
        subagent_depth: 0,
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        model_base_url: "http://localhost:1".into(),
        model_name: "m".into(),
        auth_token: None,
        context_window: None,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
    };
    let journal = dir.path().join("data/sessions/s/journal.jsonl");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    ctx.session.journal_path = journal.display().to_string();
    ctx
}

const QUESTION_ARGS: &str = r#"{
    "question_title": "Select a programming language",
    "question_text": "Choose a language for implementing the project.",
    "options": [
        {"option_title": "C++", "option_text": "High performance system language."},
        {"option_title": "Python", "option_text": "Easy to write but could be slow."}
    ]
}"#;

/// The happy path: the tool journals an `AskUserRequest` (with the
/// worker-assigned `q1` id and the trimmed fields) through the event sink
/// and returns guidance telling the model to stop and wait for the answer.
#[tokio::test]
async fn ask_user_journals_event_and_returns_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(&ctx, TOOL_ASK_USER, QUESTION_ARGS, "call_ask", &on_event)
        .await
        .unwrap();
    assert!(out.contains("Clarification question sent"), "got: {out}");
    assert!(out.contains("Stop working now"), "got: {out}");
    assert!(out.contains("question_id"), "got: {out}");
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1, "events: {events:#?}");
    match &events[0] {
        JournalEventKind::AskUserRequest { question } => {
            assert_eq!(question.question_id, "q1");
            assert_eq!(question.question_title, "Select a programming language");
            assert_eq!(
                question.question_text,
                "Choose a language for implementing the project."
            );
            assert_eq!(question.options.len(), 2);
            assert_eq!(question.options[0].option_title, "C++");
            assert_eq!(
                question.options[0].option_text,
                "High performance system language."
            );
            assert_eq!(question.options[1].option_title, "Python");
        }
        other => panic!("expected ask_user_request, got: {other:?}"),
    }
}

/// Options are optional: an empty list is a free-text-only question.
#[tokio::test]
async fn ask_user_allows_empty_options() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, None);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(
        &ctx,
        TOOL_ASK_USER,
        r#"{"question_title":"What is your budget?","question_text":"In USD.","options":[]}"#,
        "call_ask",
        &on_event,
    )
    .await
    .unwrap();
    assert!(out.contains("Clarification question sent"), "got: {out}");
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    match &events[0] {
        JournalEventKind::AskUserRequest { question } => {
            assert_eq!(question.question_id, "q1");
            assert!(question.options.is_empty());
        }
        other => panic!("expected ask_user_request, got: {other:?}"),
    }
}

/// Invalid arguments are rejected before anything is journaled: missing or
/// blank title/text, and a blank option title.
#[tokio::test]
async fn ask_user_validates_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, None);
    let no_event = |_: JournalEventKind| {};
    for args in [
        r#"{"question_text":"x","options":[]}"#,
        r#"{"question_title":"   ","question_text":"x","options":[]}"#,
        r#"{"question_title":"t","options":[]}"#,
        r#"{"question_title":"t","question_text":"  ","options":[]}"#,
        r#"{"question_title":"t","question_text":"x","options":[{"option_title":"  ","option_text":"y"}]}"#,
    ] {
        let err = execute_tool(&ctx, TOOL_ASK_USER, args, "call_ask", &no_event)
            .await
            .unwrap_err();
        assert!(err.contains("invalid arguments for ask_user"), "got: {err}");
    }
}

/// Subagents cannot ask the user: the question is shown in the UI, which
/// only root sessions have.
#[tokio::test]
async fn ask_user_rejects_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, Some("parent-1".to_string()));
    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(&ctx, TOOL_ASK_USER, QUESTION_ARGS, "call_ask", &no_event)
        .await
        .unwrap_err();
    assert!(err.contains("subagents cannot ask the user"), "got: {err}");
}

/// A second question while one is already pending is refused — the user has
/// not answered yet (stage 1: one question at a time).
#[tokio::test]
async fn ask_user_rejects_when_pending() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, None);
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::AskUserRequest {
            question: AskUserQuestion {
                question_id: "q1".into(),
                question_title: "first question".into(),
                question_text: "text".into(),
                options: vec![],
            },
        })
        .unwrap();
    drop(journal);

    let no_event = |_: JournalEventKind| {};
    let err = execute_tool(&ctx, TOOL_ASK_USER, QUESTION_ARGS, "call_ask", &no_event)
        .await
        .unwrap_err();
    assert!(err.contains("already pending"), "got: {err}");
}

/// After the pending request was answered, a new question is allowed again.
#[tokio::test]
async fn ask_user_allowed_after_answer() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ask_ctx(&dir, None);
    let mut journal =
        mo_core::JournalWriter::open(std::path::Path::new(&ctx.session.journal_path)).unwrap();
    journal
        .append(JournalEventKind::AskUserRequest {
            question: AskUserQuestion {
                question_id: "q1".into(),
                question_title: "first question".into(),
                question_text: "text".into(),
                options: vec![],
            },
        })
        .unwrap();
    let mut answers = std::collections::BTreeMap::new();
    answers.insert("q1".to_string(), "the answer".to_string());
    journal
        .append(JournalEventKind::AskUserAnswered { answers })
        .unwrap();
    drop(journal);

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<JournalEventKind>::new()));
    let on_event = {
        let events = std::sync::Arc::clone(&events);
        move |kind: JournalEventKind| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(kind);
        }
    };
    let out = execute_tool(&ctx, TOOL_ASK_USER, QUESTION_ARGS, "call_ask", &on_event)
        .await
        .unwrap();
    assert!(out.contains("Clarification question sent"), "got: {out}");
    assert_eq!(events.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
}
