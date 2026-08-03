//! System prompt construction: a fixed harness preamble, the global
//! instructions + skills from the global agents dir (`MO_AGENTS_DIR`,
//! default `$HOME/.agents`), and the contents of `<workdir>/AGENTS.md`.

use std::path::{Path, PathBuf};

pub fn build_system_prompt(workdir: &Path, agents_dir: &Path, subagent_depth: u32) -> String {
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
    // `<agents_dir>/skills/<skill>/SKILL.md`. Skills carry YAML frontmatter
    // (name, description); the full body is included because the harness has
    // no on-demand skill loader and the tools are sandboxed to the workdir,
    // so the model cannot read the skill files itself.
    let skills = discover_skills(agents_dir);
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
            prompt.push_str(&skill.body);
            prompt.push_str("\n\n");
        }
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

/// A discovered global skill: its frontmatter metadata plus the full body.
struct Skill {
    name: String,
    description: String,
    body: String,
}

/// Find every global skill under `agents_dir`. Both layouts are supported:
/// `agents_dir/<skill>/SKILL.md` and `agents_dir/skills/<skill>/SKILL.md`
/// (deduplicated by skill name, `skills/` layout scanned first). Results are
/// sorted by name for a deterministic prompt.
fn discover_skills(agents_dir: &Path) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for base in [agents_dir.join("skills"), agents_dir.to_path_buf()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(content) = std::fs::read_to_string(dir.join("SKILL.md")) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let (name, description, body) = parse_skill_md(&dir_name, &content);
            if !seen.insert(name.clone()) {
                continue;
            }
            skills.push(Skill {
                name,
                description,
                body,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse a SKILL.md: optional YAML frontmatter delimited by `---` lines
/// (flat `key: value` fields; `name` and `description` are used, everything
/// else is ignored) followed by the markdown body. Falls back to the
/// directory name when `name:` is absent.
fn parse_skill_md(dir_name: &str, content: &str) -> (String, String, String) {
    let mut name = dir_name.to_string();
    let mut description = String::new();
    let trimmed = content.trim_start();
    if let Some(after_open) = trimmed.strip_prefix("---") {
        let mut lines = after_open.lines();
        let mut fm_lines: Vec<&str> = Vec::new();
        let mut body_start: Option<usize> = None;
        for (i, line) in lines.by_ref().enumerate() {
            if line.trim() == "---" || line.trim() == "..." {
                body_start = Some(i + 1);
                break;
            }
            fm_lines.push(line);
        }
        if let Some(start) = body_start {
            for line in fm_lines {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim().trim_matches(['"', '\'']);
                match key.trim() {
                    "name" => name = value.to_string(),
                    "description" => description = value.to_string(),
                    _ => {}
                }
            }
            let body = after_open
                .lines()
                .skip(start)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            return (name, description, body);
        }
    }
    (name, description, trimmed.trim().to_string())
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
        assert!(prompt.contains("# Top skill body"));
        assert!(prompt.contains("### nested-skill"));
        assert!(prompt.contains("Description: A nested skill."));
        assert!(prompt.contains("# Nested skill body"));
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
        assert!(prompt.contains("# Just instructions"));
    }

    #[test]
    fn parse_skill_md_extracts_frontmatter() {
        let (name, desc, body) = parse_skill_md(
            "fallback",
            "---\nname: real-name\ndescription: Does things.\nlicense: MIT\n---\n\nBody here.\n",
        );
        assert_eq!(name, "real-name");
        assert_eq!(desc, "Does things.");
        assert_eq!(body, "Body here.");

        // Missing frontmatter: dir name fallback, whole content is the body.
        let (name, desc, body) = parse_skill_md("fallback", "# Just markdown\n");
        assert_eq!(name, "fallback");
        assert_eq!(desc, "");
        assert_eq!(body, "# Just markdown");

        // Quoted values are unquoted.
        let (_, desc, _) = parse_skill_md("x", "---\ndescription: 'quoted value'\n---\nbody");
        assert_eq!(desc, "quoted value");
    }
}
