//! Unit tests for the `modes` module — production code lives in
//! `mo_core/src/modes.rs`. Wired from there with `#[cfg(test)] #[path = "tests/modes_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

#[test]
fn every_mode_has_a_registry_entry() {
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        let info = mode.info();
        assert_eq!(info.name, mode.as_str());
        assert!(!info.description.is_empty());
        assert!(!info.tools.is_empty());
        assert_eq!(info.tools.len(), TOOL_NAMES.len());
    }
}

#[test]
fn build_is_writable_and_plan_explore_are_scratch_only() {
    assert_eq!(Mode::Build.info().writable, "codebase");
    assert_eq!(Mode::Plan.info().writable, "scratch only");
    assert_eq!(Mode::Explore.info().writable, "scratch only");
}

#[test]
fn mode_round_trips() {
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        assert_eq!(mode.as_str().parse::<Mode>().unwrap(), mode);
        assert_eq!(mode.to_string(), mode.as_str());
    }
    assert!("nope".parse::<Mode>().is_err());
    assert!(serde_json::from_str::<Mode>("\"build\"").is_ok());
    assert_eq!(serde_json::to_string(&Mode::Plan).unwrap(), "\"plan\"");
}

#[test]
fn mode_change_message_states_mode_restriction_and_goal() {
    let scratch = Path::new("/data/sessions/s1/tmp");
    let build = mode_change_message(Mode::Build, scratch);
    assert!(build.starts_with("[Session mode changed to build]"));
    assert!(build.contains("Build mode"));
    assert!(build.contains("modify the codebase"));
    // Build has no scratch dir to write to; the message must not send
    // the model looking for one.
    assert!(!build.contains(scratch.to_str().unwrap()));

    let plan = mode_change_message(Mode::Plan, scratch);
    assert!(plan.starts_with("[Session mode changed to plan]"));
    assert!(plan.contains("Plan mode"));
    assert!(plan.contains("implementation plan"));
    assert!(plan.contains("READ-ONLY"));
    assert!(plan.contains(scratch.to_str().unwrap()));
    assert!(plan.contains("absolute paths"));
    // The plan message mirrors the system prompt's finishing rule: call
    // request_mode_change when the plan is ready and no must-answer open
    // questions remain, otherwise list them and wait.
    assert!(plan.contains("request_mode_change"));
    assert!(plan.contains("must-answer"));
    assert!(plan.contains("wait for the user's answers"));

    let explore = mode_change_message(Mode::Explore, scratch);
    assert!(explore.starts_with("[Session mode changed to explore]"));
    assert!(explore.contains("Explore mode"));
    assert!(explore.contains("READ-ONLY"));
    assert!(explore.contains("Prefer read_file"));
    assert!(explore.contains(scratch.to_str().unwrap()));
}

#[test]
fn approved_message_adds_approval_sentence() {
    let scratch = Path::new("/data/sessions/s1/tmp");
    let approved = mode_change_approved_message(Mode::Build, scratch);
    assert!(approved.starts_with("[Session mode changed to build]"));
    assert!(approved.contains("You are now in Build mode"));
    assert!(approved.contains("approved your request"));
    assert!(approved.contains("Continue with the task"));
}

#[test]
fn tool_names_include_request_mode_change() {
    assert!(TOOL_NAMES.contains(&"request_mode_change"));
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        assert!(mode.info().tools.contains(&"request_mode_change"));
    }
}

#[test]
fn last_mode_marker_resolves_pending_requests() {
    use crate::types::{JournalEvent, JournalEventKind, JournalMessage};
    let mk = |kind: JournalEventKind| JournalEvent {
        seq: 0,
        ts: chrono::Utc::now(),
        kind,
    };
    let user = || {
        mk(JournalEventKind::Message(JournalMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }))
    };
    let request = || {
        mk(JournalEventKind::ModeChangeRequest {
            mode: Mode::Build,
            message: "may I switch?".to_string(),
        })
    };
    let approved = || {
        mk(JournalEventKind::ModeChange {
            mode: Mode::Build,
            content: "[Session mode changed to build]".to_string(),
        })
    };
    let declined = || mk(JournalEventKind::ModeChangeRequestDeclined { mode: Mode::Build });

    // No markers at all.
    assert_eq!(last_mode_marker(&[user()]), None);
    // A lone request is pending.
    assert_eq!(
        last_mode_marker(&[user(), request()]),
        Some(ModeMarker::RequestPending { mode: Mode::Build })
    );
    // A ModeChange after the request resolves it (approved).
    assert_eq!(
        last_mode_marker(&[user(), request(), approved()]),
        Some(ModeMarker::Approved { mode: Mode::Build })
    );
    // A declined marker resolves it too.
    assert_eq!(
        last_mode_marker(&[user(), request(), declined()]),
        Some(ModeMarker::Declined { mode: Mode::Build })
    );
    // An approved request followed by a *new* request is pending again.
    assert_eq!(
        last_mode_marker(&[user(), request(), approved(), request()]),
        Some(ModeMarker::RequestPending { mode: Mode::Build })
    );
}
