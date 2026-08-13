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
