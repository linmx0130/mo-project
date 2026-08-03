//! System prompt construction: a fixed harness preamble, the global
//! instructions + skills from the global agents dir (`MO_AGENTS_DIR`,
//! default `$HOME/.agents`), and the contents of `<workdir>/AGENTS.md`.

use std::path::Path;

pub fn build_system_prompt(workdir: &Path, agents_dir: &Path, subagent_depth: u32) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are an autonomous coding agent running inside the mo harness. \
         You complete tasks by reasoning and by calling the provided tools.\n\n",
    );
    prompt.push_str(&format!("Working directory: {}\n", workdir.display()));
    prompt.push_str(
        "You may read and write files only inside the working directory, except that \
         read_file may also read global skill folders (see \"Global skills\" below). \
         Never attempt to access files or run commands that escape it.\n\n",
    );
    prompt.push_str(
        "Tool usage rules:\n\
         - When you need information, read files first.\n\
         - Create new files with create_file (the parent directory must already\n\
           exist and the file must not exist); modify existing files with\n\
           edit_file.\n\
         - Make precise edits: provide a unique old_string that appears exactly once\n\
           (use replace_all only when every occurrence should change).\n\
         - Use bash for anything outside the file tools: builds, tests, git, etc.\n\
         - After making changes, verify them with read_file or bash before finishing.\n\
         - Report your final answer as a plain text message when no more tool calls\n\
           are needed.\n\n",
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_workdir_and_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use uv for Python.\n").unwrap();
        let prompt = build_system_prompt(dir.path(), agents.path(), 0);
        assert!(prompt.contains(&dir.path().display().to_string()));
        assert!(prompt.contains("Use uv for Python."));
        assert!(prompt.contains("only inside the working directory"));
    }

    #[test]
    fn no_agents_md_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        let prompt = build_system_prompt(dir.path(), agents.path(), 2);
        assert!(prompt.contains("subagent (nesting depth 2)"));
    }

    #[test]
    fn includes_global_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        std::fs::write(agents.path().join("AGENTS.md"), "Never commit to main.\n").unwrap();
        let prompt = build_system_prompt(dir.path(), agents.path(), 0);
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
        let prompt = build_system_prompt(dir.path(), agents.path(), 0);
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
        let prompt = build_system_prompt(dir.path(), agents.path(), 0);
        assert!(prompt.contains("### plain-skill"));
        assert!(prompt.contains("Description: (no description)"));
        // The body is not inlined into the system prompt.
        assert!(!prompt.contains("# Just instructions"));
    }
}
