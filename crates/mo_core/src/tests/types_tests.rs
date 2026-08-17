//! Unit tests for the `types` module — production code lives in
//! `mo_core/src/types.rs`. Wired from there with `#[cfg(test)] #[path = "tests/types_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

fn question() -> AskUserQuestion {
    AskUserQuestion {
        question_id: "q1".to_string(),
        question_title: "Select a programming language".to_string(),
        question_text: "Choose a language for implementing the project.".to_string(),
        options: vec![
            AskUserOption {
                option_title: "C++".to_string(),
                option_text: "High performance system language.".to_string(),
            },
            AskUserOption {
                option_title: "Python".to_string(),
                option_text: "Easy to write but could be slow.".to_string(),
            },
        ],
    }
}

fn event(kind: JournalEventKind) -> JournalEvent {
    JournalEvent {
        seq: 1,
        ts: chrono::Utc::now(),
        kind,
    }
}

/// Both ask-user events survive a serde round-trip with the expected JSON
/// shape (snake_case kind tags, answers as a JSON object keyed by
/// question_id).
#[test]
fn ask_user_events_round_trip() {
    let request = event(JournalEventKind::AskUserRequest {
        question: question(),
    });
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["kind"]["kind"], "ask_user_request");
    assert_eq!(json["kind"]["question"]["question_id"], "q1");
    assert_eq!(
        json["kind"]["question"]["options"][0]["option_title"],
        "C++"
    );
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);

    let mut answers = BTreeMap::new();
    answers.insert("q1".to_string(), "Rust".to_string());
    let answered = event(JournalEventKind::AskUserAnswered { answers });
    let json = serde_json::to_value(&answered).unwrap();
    assert_eq!(json["kind"]["kind"], "ask_user_answered");
    assert_eq!(json["kind"]["answers"]["q1"], "Rust");
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, answered);
}

/// An empty options list is legal (a free-text-only question).
#[test]
fn ask_user_question_allows_empty_options() {
    let q = AskUserQuestion {
        options: vec![],
        ..question()
    };
    let json = serde_json::to_value(&q).unwrap();
    assert_eq!(json["options"], serde_json::Value::Array(vec![]));
    let back: AskUserQuestion = serde_json::from_value(json).unwrap();
    assert_eq!(back, q);
}

/// `last_ask_user_marker` scans from the end: a request with no answer
/// after it is pending; an answered event resolves it; unrelated events
/// (mode markers, messages) are skipped.
#[test]
fn last_ask_user_marker_scans_from_end() {
    // No marker at all.
    let events = vec![event(JournalEventKind::Message(JournalMessage {
        role: "user".into(),
        content: "hi".into(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }))];
    assert_eq!(last_ask_user_marker(&events), None);

    // A request with nothing after it is pending — even with a mode
    // request in between (the two markers are independent).
    let events = vec![
        event(JournalEventKind::AskUserRequest {
            question: question(),
        }),
        event(JournalEventKind::ModeChangeRequest {
            mode: Mode::Build,
            message: "may I?".into(),
        }),
    ];
    assert_eq!(
        last_ask_user_marker(&events),
        Some(AskUserMarker::RequestPending)
    );

    // An answer after the request resolves it.
    let mut answers = BTreeMap::new();
    answers.insert("q1".to_string(), "Rust".to_string());
    let events = vec![
        event(JournalEventKind::AskUserRequest {
            question: question(),
        }),
        event(JournalEventKind::AskUserAnswered { answers }),
    ];
    assert_eq!(last_ask_user_marker(&events), Some(AskUserMarker::Answered));

    // A request after a resolved one is pending again.
    let mut answers = BTreeMap::new();
    answers.insert("q1".to_string(), "Python".to_string());
    let events = vec![
        event(JournalEventKind::AskUserRequest {
            question: question(),
        }),
        event(JournalEventKind::AskUserAnswered { answers }),
        event(JournalEventKind::AskUserRequest {
            question: question(),
        }),
    ];
    assert_eq!(
        last_ask_user_marker(&events),
        Some(AskUserMarker::RequestPending)
    );
}

/// Both permission events survive a serde round-trip with the expected JSON
/// shape (snake_case kind tags, `allowed` as a boolean).
#[test]
fn permission_events_round_trip() {
    let request = event(JournalEventKind::PermissionRequest {
        request_id: "p1".to_string(),
        tool: "read_file".to_string(),
        operation: "read".to_string(),
        path: "/etc/hostname".to_string(),
    });
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["kind"]["kind"], "permission_request");
    assert_eq!(json["kind"]["request_id"], "p1");
    assert_eq!(json["kind"]["tool"], "read_file");
    assert_eq!(json["kind"]["operation"], "read");
    assert_eq!(json["kind"]["path"], "/etc/hostname");
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);

    let answered = event(JournalEventKind::PermissionAnswered {
        request_id: "p1".to_string(),
        tool: "read_file".to_string(),
        operation: "read".to_string(),
        path: "/etc/hostname".to_string(),
        allowed: true,
    });
    let json = serde_json::to_value(&answered).unwrap();
    assert_eq!(json["kind"]["kind"], "permission_answered");
    assert_eq!(json["kind"]["allowed"], true);
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, answered);
}

/// `last_permission_marker` scans from the end: a request with no answer
/// after it is pending; an answered event resolves it; unrelated events
/// (ask-user markers, messages) are skipped and do not interfere.
#[test]
fn last_permission_marker_scans_from_end() {
    // No marker at all.
    let events = vec![event(JournalEventKind::Message(JournalMessage {
        role: "user".into(),
        content: "hi".into(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }))];
    assert_eq!(last_permission_marker(&events), None);

    // A request with nothing after it is pending — even with an ask-user
    // marker in between (the two markers are independent).
    let events = vec![
        event(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: "read_file".into(),
            operation: "read".into(),
            path: "/etc/passwd".into(),
        }),
        event(JournalEventKind::AskUserRequest {
            question: question(),
        }),
    ];
    assert_eq!(
        last_permission_marker(&events),
        Some(PermissionMarker::RequestPending)
    );

    // An answer after the request resolves it.
    let events = vec![
        event(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: "read_file".into(),
            operation: "read".into(),
            path: "/etc/passwd".into(),
        }),
        event(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: "read_file".into(),
            operation: "read".into(),
            path: "/etc/passwd".into(),
            allowed: true,
        }),
    ];
    assert_eq!(
        last_permission_marker(&events),
        Some(PermissionMarker::Answered)
    );

    // A request after a resolved one is pending again.
    let events = vec![
        event(JournalEventKind::PermissionAnswered {
            request_id: "p1".into(),
            tool: "read_file".into(),
            operation: "read".into(),
            path: "/etc/passwd".into(),
            allowed: false,
        }),
        event(JournalEventKind::PermissionRequest {
            request_id: "p1".into(),
            tool: "write".into(),
            operation: "write".into(),
            path: "/tmp/x".into(),
        }),
    ];
    assert_eq!(
        last_permission_marker(&events),
        Some(PermissionMarker::RequestPending)
    );
}

/// The `ModelChange` event survives a serde round-trip with the expected
/// JSON shape (`model_change`, `from` and `to` model names).
#[test]
fn model_change_round_trips() {
    let change = event(JournalEventKind::ModelChange {
        from: "model-a".to_string(),
        to: "model-b".to_string(),
    });
    let json = serde_json::to_value(&change).unwrap();
    assert_eq!(json["kind"]["kind"], "model_change");
    assert_eq!(json["kind"]["from"], "model-a");
    assert_eq!(json["kind"]["to"], "model-b");
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, change);
}

/// `SystemPrompt` carries the model it was journaled under; a journal line
/// written before the field existed parses with an empty model (and the
/// mode defaulting to `build`), so legacy journals keep working.
#[test]
fn system_prompt_round_trips_with_and_without_model() {
    let prompt = event(JournalEventKind::SystemPrompt {
        content: "you are an agent".to_string(),
        mode: Mode::Plan,
        model: "model-a".to_string(),
    });
    let json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(json["kind"]["kind"], "system_prompt");
    assert_eq!(json["kind"]["model"], "model-a");
    assert_eq!(json["kind"]["mode"], "plan");
    let back: JournalEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back, prompt);

    // Legacy line (no `model`, no `mode`): defaults kick in.
    let legacy = serde_json::json!({
        "seq": 1,
        "ts": "2026-01-01T00:00:00Z",
        "kind": { "kind": "system_prompt", "content": "old prompt" }
    });
    let parsed: JournalEvent = serde_json::from_value(legacy).unwrap();
    match &parsed.kind {
        JournalEventKind::SystemPrompt {
            content,
            mode,
            model,
        } => {
            assert_eq!(content, "old prompt");
            assert_eq!(*mode, Mode::Build);
            assert!(model.is_empty(), "legacy journals have no model");
        }
        other => panic!("expected system_prompt, got: {other:?}"),
    }
}

/// `last_model_marker` scans from the end for the model the conversation
/// last ran under: the `SystemPrompt`'s model or a previously injected
/// `ModelChange`'s `to`. Empty legacy models and unrelated events are
/// skipped.
#[test]
fn last_model_marker_scans_from_end() {
    // No marker at all (a session that never ran).
    let events = vec![event(JournalEventKind::Message(JournalMessage {
        role: "user".into(),
        content: "hi".into(),
        reasoning_content: None,
        tool_call_id: None,
        tool_calls: None,
    }))];
    assert_eq!(last_model_marker(&events), None);

    // The journaled SystemPrompt pins the model of the first run.
    let events = vec![
        event(JournalEventKind::SystemPrompt {
            content: "sys".into(),
            mode: Mode::Build,
            model: "model-a".into(),
        }),
        event(JournalEventKind::Message(JournalMessage {
            role: "assistant".into(),
            content: "done".into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        })),
    ];
    assert_eq!(last_model_marker(&events), Some("model-a".to_string()));

    // A ModelChange is the marker for every run after it happened.
    let events = vec![
        event(JournalEventKind::SystemPrompt {
            content: "sys".into(),
            mode: Mode::Build,
            model: "model-a".into(),
        }),
        event(JournalEventKind::ModelChange {
            from: "model-a".into(),
            to: "model-b".into(),
        }),
    ];
    assert_eq!(last_model_marker(&events), Some("model-b".to_string()));

    // The most recent marker wins (a later SystemPrompt — e.g. the fresh
    // one after a context compression — beats an earlier ModelChange).
    let events = vec![
        event(JournalEventKind::ModelChange {
            from: "model-a".into(),
            to: "model-b".into(),
        }),
        event(JournalEventKind::SystemPrompt {
            content: "fresh sys".into(),
            mode: Mode::Build,
            model: "model-b".into(),
        }),
    ];
    assert_eq!(last_model_marker(&events), Some("model-b".to_string()));

    // A legacy SystemPrompt (empty model) carries no marker; a later
    // ModelChange still does.
    let events = vec![
        event(JournalEventKind::SystemPrompt {
            content: "legacy sys".into(),
            mode: Mode::Build,
            model: String::new(),
        }),
        event(JournalEventKind::ModelChange {
            from: "model-a".into(),
            to: "model-b".into(),
        }),
    ];
    assert_eq!(last_model_marker(&events), Some("model-b".to_string()));

    // Only legacy SystemPrompts (no model) → no marker at all.
    let events = vec![event(JournalEventKind::SystemPrompt {
        content: "legacy sys".into(),
        mode: Mode::Build,
        model: String::new(),
    })];
    assert_eq!(last_model_marker(&events), None);
}
