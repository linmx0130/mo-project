//! Unit tests for the `skills` module — production code lives in
//! `mo_core/src/skills.rs`. Wired from there with `#[cfg(test)] #[path = "tests/skills_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

fn write_skill(agents: &Path, rel_dir: &str, name: &str, description: &str, body: &str) {
    let dir = agents.join(rel_dir);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n");
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

#[test]
fn discovers_both_layouts_sorted_with_paths() {
    let agents = tempfile::tempdir().unwrap();
    write_skill(
        agents.path(),
        "top-skill",
        "top-skill",
        "A top-level skill.",
        "# Top body",
    );
    write_skill(
        agents.path(),
        "skills/nested-skill",
        "nested-skill",
        "A nested skill.",
        "# Nested body",
    );
    let skills = discover_skills(agents.path());
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["nested-skill", "top-skill"]);
    assert_eq!(skills[0].description, "A nested skill.");
    assert_eq!(skills[1].description, "A top-level skill.");
    // Folder paths point at the SKILL.md's directory.
    assert!(skills[0].path.ends_with("skills/nested-skill"));
    assert!(skills[1].path.ends_with("top-skill"));
}

#[test]
fn dedupes_by_name_preferring_skills_layout() {
    let agents = tempfile::tempdir().unwrap();
    write_skill(agents.path(), "dup", "dup", "top layout", "# one");
    write_skill(agents.path(), "skills/dup", "dup", "skills layout", "# two");
    let skills = discover_skills(agents.path());
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].description, "skills layout");
}

#[test]
fn find_skill_by_name() {
    let agents = tempfile::tempdir().unwrap();
    write_skill(agents.path(), "alpha", "alpha", "Does alpha.", "# Alpha");
    write_skill(agents.path(), "beta", "beta", "Does beta.", "# Beta");
    let alpha = find_skill(agents.path(), "alpha").unwrap();
    assert_eq!(alpha.description, "Does alpha.");
    assert!(find_skill(agents.path(), "beta").is_some());
    assert!(find_skill(agents.path(), "gamma").is_none());
    assert!(find_skill(agents.path(), "").is_none());
    assert!(find_skill(agents.path(), "ALPHA").is_none());
}

#[test]
fn missing_or_empty_skills_are_ignored() {
    let agents = tempfile::tempdir().unwrap();
    // No SKILL.md at all.
    assert!(discover_skills(agents.path()).is_empty());
    // A directory without SKILL.md.
    std::fs::create_dir_all(agents.path().join("empty")).unwrap();
    assert!(discover_skills(agents.path()).is_empty());
    // An empty SKILL.md.
    std::fs::write(agents.path().join("empty").join("SKILL.md"), "  \n").unwrap();
    assert!(discover_skills(agents.path()).is_empty());
}

#[test]
fn parse_skill_md_extracts_frontmatter() {
    let (name, desc) = parse_skill_md(
        "fallback",
        "---\nname: real-name\ndescription: Does things.\nlicense: MIT\n---\n\nBody here.\n",
    );
    assert_eq!(name, "real-name");
    assert_eq!(desc, "Does things.");

    // Missing frontmatter: dir name fallback, no description.
    let (name, desc) = parse_skill_md("fallback", "# Just markdown\n");
    assert_eq!(name, "fallback");
    assert_eq!(desc, "");

    // Quoted values are unquoted.
    let (_, desc) = parse_skill_md("x", "---\ndescription: 'quoted value'\n---\nbody");
    assert_eq!(desc, "quoted value");
}

/// The status-bar load message wraps the skill's SKILL.md in a marker so
/// the model understands the pasted file is the user force-loading a skill.
#[test]
fn skill_load_message_wraps_content_with_marker() {
    let msg = skill_load_message("j-space", "# Just instructions\n");
    assert!(msg.starts_with("[The user loaded the skill \"j-space\""));
    assert!(msg.contains("Treat them as active and follow them."));
    assert!(msg.contains("# Just instructions"));
    // Content is trimmed (no trailing newline gluing).
    assert!(msg.ends_with("# Just instructions"));
}

/// The prefix's `%s` placeholder is substituted with the skill name.
#[test]
fn skill_load_prefix_substitutes_name() {
    let msg = skill_load_message("my-skill", "body");
    assert!(!msg.contains("%s"));
    assert!(msg.contains("my-skill"));
}
