//! Unit tests for the `prompt` module — production code lives in
//! `mo_worker/src/prompt.rs`. Wired from there with `#[cfg(test)] #[path = "tests/prompt_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use mo_core::Mode;

/// A scratch dir under the tempdir, as the worker creates it per session.
fn scratch(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("data/sessions/s1/tmp");
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn prompt_for(mode: Mode, dir: &tempfile::TempDir, agents: &tempfile::TempDir) -> String {
    build_system_prompt(dir.path(), agents.path(), 0, mode, &scratch(dir))
}

#[test]
fn includes_workdir_and_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Use uv for Python.\n").unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains(&dir.path().display().to_string()));
    assert!(prompt.contains("Use uv for Python."));
    assert!(prompt.contains("only inside the working directory"));
}

#[test]
fn no_agents_md_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(dir.path(), agents.path(), 2, Mode::Build, &scratch(&dir));
    assert!(prompt.contains("subagent (nesting depth 2)"));
}

/// The subagent framing is keyed on depth: depth 0 (a root session)
/// must never be told "you are a subagent", while a depth > 0 session
/// is framed as a subagent of that nesting depth.
#[test]
fn depth_zero_is_not_framed_as_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(dir.path(), agents.path(), 0, Mode::Build, &scratch(&dir));
    assert!(
        !prompt.contains("spawned by another agent"),
        "a root session must not be framed as a subagent: {prompt}"
    );
    assert!(!prompt.contains("nesting depth"), "got: {prompt}");
    assert!(
        !prompt.contains("return a concise summary"),
        "got: {prompt}"
    );
}

#[test]
fn depth_one_is_framed_as_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(dir.path(), agents.path(), 1, Mode::Build, &scratch(&dir));
    assert!(
        prompt.contains("You are a subagent (nesting depth 1) spawned by another agent."),
        "got: {prompt}"
    );
    assert!(prompt.contains("return a concise summary"), "got: {prompt}");
}

#[test]
fn includes_global_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    std::fs::write(agents.path().join("AGENTS.md"), "Never commit to main.\n").unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains("Global instructions from"));
    assert!(prompt.contains("Never commit to main."));
    // Global instructions come before project instructions.
    assert!(
        prompt.find("Global instructions from").unwrap()
            < prompt
                .find("Project instructions from")
                .unwrap_or(usize::MAX)
    );
}

#[test]
fn includes_skills_from_both_layouts() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    // Layout 1: `<agents>/<skill>/SKILL.md`
    let top = agents.path().join("top-skill");
    std::fs::create_dir_all(&top).unwrap();
    std::fs::write(
        top.join("SKILL.md"),
        "---\nname: top-skill\ndescription: A top-level skill.\n---\n# Top skill body\n",
    )
    .unwrap();
    // Layout 2: `<agents>/skills/<skill>/SKILL.md`
    let nested = agents.path().join("skills").join("nested-skill");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("SKILL.md"),
        "---\nname: nested-skill\ndescription: \"A nested skill.\"\n---\n# Nested skill body\n",
    )
    .unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains("Global skills available from"));
    assert!(prompt.contains("### top-skill"));
    assert!(prompt.contains("Description: A top-level skill."));
    assert!(prompt.contains("### nested-skill"));
    assert!(prompt.contains("Description: A nested skill."));
    // The full bodies stay out of the prompt; they are fetched on demand
    // via the load_skill tool, and read_file may read skill folders.
    assert!(!prompt.contains("# Top skill body"));
    assert!(!prompt.contains("# Nested skill body"));
    assert!(prompt.contains("load_skill"));
    assert!(prompt.contains("read_file may read reference"));
}

#[test]
fn skill_without_frontmatter_falls_back_to_dir_name() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let skill = agents.path().join("plain-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# Just instructions\n").unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains("### plain-skill"));
    assert!(prompt.contains("Description: (no description)"));
    // The body is not inlined into the system prompt.
    assert!(!prompt.contains("# Just instructions"));
}

#[test]
fn build_mode_mentions_full_access() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains("Build mode"));
    // Build keeps the create/edit instructions.
    assert!(prompt.contains("modify existing files"));
    assert!(prompt.contains("edit_file"));
}

#[test]
fn plan_mode_frames_plan_only_and_readonly_codebase() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = prompt_for(Mode::Plan, &dir, &agents);
    let scratch = scratch(&dir);
    assert!(prompt.contains("Plan mode"));
    assert!(prompt.contains("implementation plan"));
    assert!(prompt.contains("READ-ONLY"));
    assert!(prompt.contains(&scratch.display().to_string()));
    assert!(prompt.contains("absolute paths"));
    // The create/edit instructions are replaced by the scratch rule.
    assert!(!prompt.contains("modify existing files with edit_file"));
    // Plan mode points the model at request_mode_change instead of
    // working around the sandbox (subagents included — they cannot
    // bypass the sandbox either).
    assert!(prompt.contains("request_mode_change"));
    assert!(prompt.contains("Finish with the plan as your final answer"));
    assert!(
        prompt.contains("work around the sandbox or use subagent to bypass"),
        "got: {prompt}"
    );
    // The finishing rule is an explicit two-branch decision: call
    // request_mode_change when the plan is ready and has no must-answer
    // open questions, otherwise list the questions and wait.
    assert!(prompt.contains("exactly one of two exits"));
    assert!(prompt.contains("must-answer"));
    assert!(prompt.contains("open"));
    assert!(prompt.contains("mode \"build\""));
    assert!(prompt.contains("do NOT call"));
    assert!(prompt.contains("shall I proceed?"));
    assert!(prompt.contains("wait for the user's answers"));
}

#[test]
fn explore_mode_frames_investigation_and_readonly_codebase() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = prompt_for(Mode::Explore, &dir, &agents);
    assert!(prompt.contains("Explore mode"));
    assert!(prompt.contains("READ-ONLY"));
    assert!(prompt.contains("Prefer read_file"));
    assert!(prompt.contains(&scratch(&dir).display().to_string()));
    // Explore mode also points at request_mode_change for build tasks.
    assert!(prompt.contains("request_mode_change"));
}

/// Every mode's system prompt mentions context compression, so the
/// model understands a handoff user message when one arrives mid-task.
#[test]
fn all_modes_mention_context_compression() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        let prompt = prompt_for(mode, &dir, &agents);
        assert!(prompt.contains("handoff prompt"), "mode {mode}: {prompt}");
        assert!(
            prompt.contains("Treat it as your memory of the task and continue"),
            "mode {mode}: {prompt}"
        );
    }
}

/// Every mode's system prompt explicitly tells the model to ask the user
/// for clarification (via the `ask_user` tool) when it needs more input —
/// a choice, a preference, or a detail only the user can decide.
#[test]
fn all_modes_tell_the_model_to_ask_clarification() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        let prompt = prompt_for(mode, &dir, &agents);
        assert!(prompt.contains("`ask_user`"), "mode {mode}: {prompt}");
        assert!(
            prompt.contains("If you need more input from the user to continue"),
            "mode {mode}: {prompt}"
        );
        assert!(
            prompt.contains("preset options and a free-text input box"),
            "mode {mode}: {prompt}"
        );
        assert!(
            prompt.contains("the user's answer arrives as a user message"),
            "mode {mode}: {prompt}"
        );
    }
}

/// The handoff-generation instruction must demand all five sections.
#[test]
fn handoff_instruction_covers_all_sections() {
    let instruction = handoff_instruction();
    assert!(instruction.contains("original user input"));
    assert!(instruction.contains("environment"));
    assert!(instruction.contains("build/test commands"));
    assert!(instruction.contains("Key decisions"));
    assert!(instruction.contains("Current progress"));
    assert!(instruction.contains("todo list"));
    assert!(instruction.contains("next step"));
    assert!(instruction.contains("ONLY the handoff prompt"));
    // The prefix is a self-explanatory marker for the synthesized user
    // message of a compressed context.
    assert!(HANDOFF_USER_PREFIX.starts_with("[Context compressed:"));
    assert!(HANDOFF_USER_PREFIX.contains("continue from its next step"));
}
