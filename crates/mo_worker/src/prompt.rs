//! System prompt construction: a fixed harness preamble (mode-aware), the
//! global instructions + skills from the global agents dir (`MO_AGENTS_DIR`,
//! default `$HOME/.agents`), and the contents of `<workdir>/AGENTS.md`.
//!
//! The caller journals the returned prompt as a `SystemPrompt` event on the
//! session's first run; every later run reuses the journaled text verbatim,
//! so this function runs once per session (mode changes and `AGENTS.md`
//! edits mid-session never alter the system prompt).

use std::path::Path;

use mo_core::Mode;

/// Build the system prompt for a session's first run.
///
/// `mode` frames the agent's job (Build = full access, Plan = plan only,
/// Explore = investigate only) and `scratch` is the session scratch dir
/// (`<data_dir>/sessions/<id>/tmp`) where non-Build modes may create/edit/
/// remove files — the codebase stays read-only for them.
pub fn build_system_prompt(
    workdir: &Path,
    agents_dir: &Path,
    subagent_depth: u32,
    mode: Mode,
    scratch: &Path,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are an autonomous coding agent running inside the mo harness. \
         You complete tasks by reasoning and by calling the provided tools.\n\n",
    );
    prompt.push_str(&mode_framing(mode, workdir, scratch));
    prompt.push_str(&format!("Working directory: {}\n", workdir.display()));
    prompt.push_str(
        "You may read and write files only inside the working directory, except that \
         read_file may also read global skill folders (see \"Global skills\" below). \
         Never attempt to access files or run commands that escape it.\n\n",
    );
    prompt.push_str(&tool_usage_rules(mode, scratch));
    if subagent_depth > 0 {
        prompt.push_str(&format!(
            "You are a subagent (nesting depth {}) spawned by another agent.\n\
             Work within your working directory, do not spawn further subagents unless\n\
             truly necessary, and return a concise summary of what you did and found.\n\n",
            subagent_depth
        ));
    }

    // Global agent instructions (`<agents_dir>/AGENTS.md`) — the broadest,
    // user-level rules that apply to every session, injected first.
    let global_agents_md = agents_dir.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&global_agents_md)
        && !content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "Global instructions from {}/AGENTS.md:\n{}\n\n",
            agents_dir.display(),
            content.trim()
        ));
    }

    // Global skills: `<agents_dir>/<skill>/SKILL.md` and
    // `<agents_dir>/skills/<skill>/SKILL.md`. Only the frontmatter name +
    // description go into the system prompt; the model pulls the full
    // instructions on demand via the `load_skill` tool, which also returns
    // the skill folder path so `read_file` can reach bundled resources.
    let skills = crate::skills::discover_skills(agents_dir);
    if !skills.is_empty() {
        prompt.push_str(&format!(
            "Global skills available from {}:\n",
            agents_dir.display()
        ));
        for skill in &skills {
            prompt.push_str(&format!("### {}\n", skill.name));
            prompt.push_str(&format!(
                "Description: {}\n",
                if skill.description.is_empty() {
                    "(no description)"
                } else {
                    &skill.description
                }
            ));
        }
        prompt.push_str(
            "\nLoad a skill on demand by calling the load_skill tool with the \
             skill name: it returns the full SKILL.md instructions and the \
             absolute path of the skill's folder. read_file may read reference \
             files, scripts, and other resources inside skill folders.\n\n",
        );
    }

    let agents_md = workdir.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&agents_md)
        && !content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "Project instructions from {}/AGENTS.md:\n{}\n\n",
            workdir.display(),
            content.trim()
        ));
    }
    prompt
}

/// The mode-specific framing paragraph, right after the harness intro.
fn mode_framing(mode: Mode, workdir: &Path, scratch: &Path) -> String {
    match mode {
        Mode::Build => "You are in Build mode: you can modify the codebase (create/edit/remove \
             files), run commands, and use subagents and skills to get the job done.\n\n"
            .to_string(),
        Mode::Plan => format!(
            "You are in Plan mode. Your job is to produce a clear, actionable \
             implementation plan — do not implement anything yet.\n\
             The codebase ({}) is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             bash is available but treat it as read-only (a soft restriction): use it to \
             gather facts (builds, tests, greps), not to change anything.\n\
             Finish with the plan as your final answer and call the `request_mode_change` tool \
             with mode \"build\" to ask them to switch this session to build mode — the user \
             approves or rejects in the UI, and on approval the session continues in build \
             mode. Do not try to work around the sandbox or use subagent to bypass.\n\n",
            workdir.display(),
            scratch.display()
        ),
        Mode::Explore => format!(
            "You are in Explore mode. Investigate the codebase to answer the user's \
             question or gather facts for a parent agent.\n\
             The codebase ({}) is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             Prefer read_file; run read-only bash commands when helpful.\n\
             If the task turns into modifying the codebase, call the `request_mode_change` \
             tool to ask the user to switch this session to build mode.\n\
             Report concise findings as your final answer.\n\n",
            workdir.display(),
            scratch.display()
        ),
    }
}

/// The tool-usage rules paragraph. Non-Build modes drop the create/edit
/// bullets in favour of the scratch-dir rule, so the model is never told to
/// do something the sandbox denies.
fn tool_usage_rules(mode: Mode, scratch: &Path) -> String {
    match mode {
        Mode::Build => "Tool usage rules:\n\
             - When you need information, read files first.\n\
             - Create new files with create_file (the parent directory must already\n\
               exist and the file must not exist); modify existing files with\n\
               edit_file.\n\
             - Make precise edits: provide a unique old_string that appears exactly once\n\
               (use replace_all only when every occurrence should change).\n\
             - Use bash for anything outside the file tools: builds, tests, git, etc.\n\
             - After making changes, verify them with read_file or bash before finishing.\n\
             - Tool calls in one message run concurrently (up to max_tool_concurrency);\n\
               completion order is not guaranteed, so never put dependent calls in the\n\
               same message — wait for a result, then make the next call.\n\
             - Report your final answer as a plain text message when no more tool calls\n\
               are needed.\n\n"
            .to_string(),
        _ => format!(
            "Tool usage rules:\n\
             - When you need information, read files first.\n\
             - create_file / edit_file / remove_file are allowed ONLY under the session\n\
               scratch directory {} (absolute paths); the codebase is read-only and\n\
               modifications there are denied.\n\
             - Use bash for anything outside the file tools (builds, tests, git, ...),\n\
               keeping it read-only.\n\
             - Tool calls in one message run concurrently (up to max_tool_concurrency);\n\
               completion order is not guaranteed, so never put dependent calls in the\n\
               same message — wait for a result, then make the next call.\n\
             - Report your final answer as a plain text message when no more tool calls\n\
               are needed.\n\n",
            scratch.display()
        ),
    }
}

#[cfg(test)]
mod tests {
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
}
