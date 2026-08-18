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
    build_system_prompt(dir.path(), agents.path(), 0, mode, &scratch(dir), &[])
}

#[test]
fn includes_workdir_and_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Use uv for Python.\n").unwrap();
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains(&dir.path().display().to_string()));
    assert!(prompt.contains("Use uv for Python."));
    // The sandbox paragraph: files inside the workdir are freely
    // accessible; paths outside require the user's approval.
    assert!(prompt.contains("Files inside the working directory"));
    assert!(prompt.contains("requires the user's approval"));
}

/// Every mode's system prompt tells the model about the file-access
/// permission flow: paths outside the allowed roots prompt the user in the
/// UI — the call is held until the user decides, then returns its outcome.
#[test]
fn all_modes_mention_permission_requests() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    for mode in [Mode::Build, Mode::Plan, Mode::Explore] {
        let prompt = prompt_for(mode, &dir, &agents);
        assert!(
            prompt.contains("permission request"),
            "mode {mode}: {prompt}"
        );
        assert!(
            prompt.contains("holds the call until the user decides")
                || prompt.contains("the call is held until the user decides"),
            "mode {mode}: {prompt}"
        );
    }
    // Build: outside paths are asked about.
    let prompt = prompt_for(Mode::Build, &dir, &agents);
    assert!(prompt.contains("holds the call until the user decides"));
    // Plan/explore: reads outside prompt the user; writes outside the
    // scratch dir are denied outright.
    let prompt = prompt_for(Mode::Plan, &dir, &agents);
    assert!(prompt.contains("denied outright"));
    assert!(prompt.contains("never"));
    assert!(prompt.contains("asked about"));
}

#[test]
fn no_agents_md_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        2,
        Mode::Build,
        &scratch(&dir),
        &[],
    );
    assert!(prompt.contains("subagent (nesting depth 2)"));
}

/// The subagent framing is keyed on depth: depth 0 (a root session)
/// must never be told "you are a subagent", while a depth > 0 session
/// is framed as a subagent of that nesting depth.
#[test]
fn depth_zero_is_not_framed_as_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        0,
        Mode::Build,
        &scratch(&dir),
        &[],
    );
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
    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        1,
        Mode::Build,
        &scratch(&dir),
        &[],
    );
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

/// Force-loading a skill inlines its FULL SKILL.md into the system prompt
/// (a "forced skills" section before the on-demand listing) and takes it
/// out of the load-on-demand list — the model never needs `load_skill` for
/// it. Non-forced skills stay listed on demand with metadata only.
#[test]
fn forced_skills_inline_full_body_and_leave_on_demand_list() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    // Two skills: one forced, one on-demand.
    let forced_dir = agents.path().join("forced-skill");
    std::fs::create_dir_all(&forced_dir).unwrap();
    std::fs::write(
        forced_dir.join("SKILL.md"),
        "---\nname: forced-skill\ndescription: A forced skill.\n---\n# Forced body text\n",
    )
    .unwrap();
    let on_demand_dir = agents.path().join("on-demand-skill");
    std::fs::create_dir_all(&on_demand_dir).unwrap();
    std::fs::write(
        on_demand_dir.join("SKILL.md"),
        "---\nname: on-demand-skill\ndescription: An on-demand skill.\n---\n# On-demand body text\n",
    )
    .unwrap();

    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        0,
        Mode::Build,
        &scratch(&dir),
        &["forced-skill".to_string()],
    );
    // The forced skill's full body is inlined under its own section.
    assert!(prompt.contains("Forced skills"));
    assert!(prompt.contains("# Forced body text"));
    assert!(prompt.contains("Description: A forced skill."));
    // The forced skill is NOT in the load-on-demand list...
    let on_demand_section = prompt
        .split("Global skills available from")
        .nth(1)
        .unwrap_or("");
    assert!(!on_demand_section.contains("forced-skill"));
    // ...but the other skill still is, metadata only.
    assert!(on_demand_section.contains("### on-demand-skill"));
    assert!(on_demand_section.contains("Description: An on-demand skill."));
    assert!(!on_demand_section.contains("# On-demand body text"));
    // The forced-skill section precedes the on-demand list.
    assert!(
        prompt.find("Forced skills").unwrap()
            < prompt
                .find("Global skills available from")
                .unwrap_or(usize::MAX)
    );
}

/// A forced skill that no longer exists on disk (deleted after the session
/// was created) is skipped defensively — the prompt still builds and the
/// other skills behave normally.
#[test]
fn forced_skill_that_disappeared_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let skill = agents.path().join("real-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: real-skill\ndescription: Real.\n---\n# Real body\n",
    )
    .unwrap();
    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        0,
        Mode::Build,
        &scratch(&dir),
        &["real-skill".to_string(), "ghost-skill".to_string()],
    );
    // The existing forced skill is inlined; the ghost is not mentioned.
    assert!(prompt.contains("Forced skills"));
    assert!(prompt.contains("# Real body"));
    assert!(!prompt.contains("ghost-skill"));
    // The real skill is out of the on-demand list (it is forced).
    let on_demand_section = prompt
        .split("Global skills available from")
        .nth(1)
        .unwrap_or("");
    assert!(!on_demand_section.contains("real-skill"));
}

/// When every discovered skill is forced, the load-on-demand section (and
/// its load_skill blurb) is omitted entirely — nothing is left to load.
#[test]
fn all_skills_forced_omits_on_demand_section() {
    let dir = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let skill = agents.path().join("only-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: only-skill\ndescription: Only.\n---\n# Only body\n",
    )
    .unwrap();
    let prompt = build_system_prompt(
        dir.path(),
        agents.path(),
        0,
        Mode::Build,
        &scratch(&dir),
        &["only-skill".to_string()],
    );
    assert!(prompt.contains("Forced skills"));
    assert!(prompt.contains("# Only body"));
    assert!(!prompt.contains("Global skills available from"));
    // The on-demand blurb (which teaches the load_skill tool) is gone; the
    // only remaining mention is the forced-section header telling the model
    // not to call load_skill for already-loaded skills.
    assert!(!prompt.contains("Load a skill on demand by calling the load_skill tool"));
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
