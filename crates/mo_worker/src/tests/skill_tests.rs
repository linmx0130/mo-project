//! Unit tests for the `tools::skill` module — production code lives in
//! `mo_worker/src/tools/skill.rs`. Wired from there with `#[cfg(test)] #[path = "../tests/skill_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

#[test]
fn returns_path_line_then_skill_md_content() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    let skill_dir = agents.join("greeter");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: greeter\ndescription: Greets people.\n---\n# Greeter\nSay hello.\n",
    )
    .unwrap();
    std::fs::write(skill_dir.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();

    let out = load_skill(&agents, "greeter").unwrap();
    let expected_path = skill_dir.canonicalize().unwrap();
    assert_eq!(
        out,
        format!(
            "Path: {}\n\n---\nname: greeter\ndescription: Greets people.\n---\n# Greeter\nSay hello.\n",
            expected_path.display()
        )
    );
}

#[test]
fn errors_for_missing_or_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    assert!(
        load_skill(&agents, "nope")
            .unwrap_err()
            .contains("skill not found")
    );
    assert!(
        load_skill(&agents, "")
            .unwrap_err()
            .contains("must not be empty")
    );
    assert!(
        load_skill(&agents, "  ")
            .unwrap_err()
            .contains("must not be empty")
    );
}
