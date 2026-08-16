//! Unit tests for the `tools` module — production code lives in
//! `mo_core/src/tools.rs`. Wired from there with `#[cfg(test)] #[path = "tests/tools_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

fn names(tools: &[ToolInfo]) -> Vec<&'static str> {
    tools.iter().map(|t| t.name).collect()
}

/// The registry is complete and consistent: every tool is either fixed
/// (always available) or toggleable (user-selectable), never both, and
/// the two groups partition `TOOL_NAMES`.
#[test]
fn registry_partitions_fixed_and_toggleable() {
    // Fixed and toggleable tools are disjoint and together cover every
    // tool name (and the registry's display order matches TOOL_NAMES).
    let mut fixed: Vec<&str> = FIXED_TOOLS.to_vec();
    let mut toggleable: Vec<&str> = TOGGLEABLE_TOOLS.to_vec();
    fixed.sort_unstable();
    toggleable.sort_unstable();
    assert!(
        fixed.iter().all(|f| !toggleable.contains(f)),
        "fixed and toggleable tools must be disjoint"
    );
    let mut all: Vec<&str> = fixed.iter().chain(toggleable.iter()).copied().collect();
    all.sort_unstable();
    let mut canonical: Vec<&str> = TOOL_NAMES.to_vec();
    canonical.sort_unstable();
    assert_eq!(all, canonical, "fixed ∪ toggleable must equal TOOL_NAMES");

    // The registry mirrors the partition: `fixed` flags match the group.
    let registry_names = names(TOOLS);
    assert_eq!(registry_names, TOOL_NAMES, "display order must match");
    for tool in TOOLS {
        assert_eq!(
            tool.fixed,
            is_fixed(tool.name),
            "{}: registry fixed flag must match the group",
            tool.name
        );
        assert!(
            is_fixed(tool.name) || is_toggleable(tool.name),
            "{}: every tool belongs to exactly one group",
            tool.name
        );
    }
    // The always-available set is exactly bash + the file operations.
    assert!(is_fixed("bash"));
    assert!(is_fixed("bash_in_background"));
    assert!(is_fixed("read_file"));
    assert!(is_fixed("edit_file"));
    assert!(is_fixed("create_file"));
    assert!(is_fixed("remove_file"));
    assert!(!is_fixed("spawn_subagent"));
    assert!(!is_fixed("load_skill"));
    assert!(!is_fixed("request_mode_change"));
    assert!(!is_fixed("ask_user"));
}

/// `is_enabled`: an empty enabled list (legacy sessions) means everything
/// is enabled; a non-empty list restricts to its members.
#[test]
fn is_enabled_respects_the_enabled_list() {
    // Empty list = no restriction = all tools enabled.
    assert!(is_enabled("ask_user", &[]));
    assert!(is_enabled("bash", &[]));

    let enabled: Vec<String> = ["bash", "read_file", "ask_user"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(is_enabled("bash", &enabled));
    assert!(is_enabled("read_file", &enabled));
    assert!(is_enabled("ask_user", &enabled));
    assert!(!is_enabled("edit_file", &enabled));
    assert!(!is_enabled("spawn_subagent", &enabled));
    assert!(!is_enabled("nope", &enabled));
}

/// The default resolution (nothing banned) enables every tool, with the
/// fixed ones first.
#[test]
fn resolve_with_nothing_banned_enables_all_tools() {
    let enabled = resolve_enabled_tools(&[]).unwrap();
    let expected: Vec<String> = TOOL_NAMES.iter().map(|s| s.to_string()).collect();
    assert_eq!(enabled, expected, "default must be all tools");
}

/// Banning toggleable tools removes exactly those from the enabled list;
/// the fixed tools are always included.
#[test]
fn resolve_bans_toggleable_tools_only() {
    let enabled = resolve_enabled_tools(&[
        "ask_user".to_string(),
        "request_mode_change".to_string(),
        "spawn_subagent".to_string(),
    ])
    .unwrap();
    // The fixed six stay, plus the one toggleable that was not banned.
    assert_eq!(enabled.len(), 7, "enabled: {enabled:?}");
    assert!(enabled.iter().all(|t| is_fixed(t) || t == "load_skill"));
    assert!(!enabled.contains(&"ask_user".to_string()));
    assert!(!enabled.contains(&"request_mode_change".to_string()));
    assert!(!enabled.contains(&"spawn_subagent".to_string()));
    assert!(enabled.contains(&"load_skill".to_string()));
    for fixed in FIXED_TOOLS {
        assert!(enabled.iter().any(|t| t == fixed), "{fixed} is always on");
    }
}

/// Banning a fixed tool or an unknown name is a client error.
#[test]
fn resolve_rejects_fixed_and_unknown_bans() {
    let err = resolve_enabled_tools(&["bash".to_string()]).unwrap_err();
    assert!(err.contains("bash"), "got: {err}");
    assert!(err.contains("always available"), "got: {err}");

    let err = resolve_enabled_tools(&["read_file".to_string()]).unwrap_err();
    assert!(err.contains("always available"), "got: {err}");

    let err = resolve_enabled_tools(&["nope".to_string()]).unwrap_err();
    assert!(err.contains("unknown tool"), "got: {err}");

    // A valid ban plus an invalid one rejects the whole request.
    let err = resolve_enabled_tools(&["ask_user".to_string(), "nope".to_string()]).unwrap_err();
    assert!(err.contains("unknown tool"), "got: {err}");
}
