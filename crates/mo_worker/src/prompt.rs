//! System prompt construction: a fixed harness preamble plus the contents
//! of `<workdir>/AGENTS.md` when present.

use std::path::Path;

pub fn build_system_prompt(workdir: &Path, subagent_depth: u32) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are an autonomous coding agent running inside the mo harness. \
         You complete tasks by reasoning and by calling the provided tools.\n\n",
    );
    prompt.push_str(&format!("Working directory: {}\n", workdir.display()));
    prompt.push_str(
        "You may read and write files only inside the working directory. \
         Never attempt to access files or run commands that escape it.\n\n",
    );
    prompt.push_str(
        "Tool usage rules:\n\
         - When you need information, read files first.\n\
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
        std::fs::write(dir.path().join("AGENTS.md"), "Use uv for Python.\n").unwrap();
        let prompt = build_system_prompt(dir.path(), 0);
        assert!(prompt.contains(&dir.path().display().to_string()));
        assert!(prompt.contains("Use uv for Python."));
        assert!(prompt.contains("only inside the working directory"));
    }

    #[test]
    fn no_agents_md_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = build_system_prompt(dir.path(), 2);
        assert!(prompt.contains("subagent (nesting depth 2)"));
    }
}
