//! Global skill discovery: `<agents_dir>/<skill>/SKILL.md` and
//! `<agents_dir>/skills/<skill>/SKILL.md`.
//!
//! A skill's `SKILL.md` carries YAML frontmatter (`name`, `description`)
//! followed by the instructions body. The system prompt lists only the name
//! and description metadata; the `load_skill` tool returns the full
//! `SKILL.md` plus the absolute path of the skill folder, so the agent can
//! also read reference files, scripts, and other resources bundled with the
//! skill.

use std::path::{Path, PathBuf};

/// A discovered global skill: frontmatter metadata plus the absolute path of
/// the skill folder (which contains `SKILL.md` and any reference resources).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Find every global skill under `agents_dir`. Both layouts are supported:
/// `agents_dir/<skill>/SKILL.md` and `agents_dir/skills/<skill>/SKILL.md`
/// (deduplicated by skill name, `skills/` layout scanned first). Results are
/// sorted by name for a deterministic prompt.
pub fn discover_skills(agents_dir: &Path) -> Vec<Skill> {
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
            let (name, description) = parse_skill_md(&dir_name, &content);
            if !seen.insert(name.clone()) {
                continue;
            }
            let path = dir.canonicalize().unwrap_or(dir);
            skills.push(Skill {
                name,
                description,
                path,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Find a skill by name (same layouts/deduplication as `discover_skills`).
/// Returns `None` when no skill with that name exists.
pub fn find_skill(agents_dir: &Path, name: &str) -> Option<Skill> {
    discover_skills(agents_dir)
        .into_iter()
        .find(|s| s.name == name)
}

/// Parse a SKILL.md: optional YAML frontmatter delimited by `---` lines
/// (flat `key: value` fields; `name` and `description` are used, everything
/// else is ignored) followed by the markdown body. Falls back to the
/// directory name when `name:` is absent.
pub fn parse_skill_md(dir_name: &str, content: &str) -> (String, String) {
    let mut name = dir_name.to_string();
    let mut description = String::new();
    let trimmed = content.trim_start();
    if let Some(after_open) = trimmed.strip_prefix("---") {
        let mut lines = after_open.lines();
        let mut fm_lines: Vec<&str> = Vec::new();
        for line in lines.by_ref() {
            if line.trim() == "---" || line.trim() == "..." {
                break;
            }
            fm_lines.push(line);
        }
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
    }
    (name, description)
}

#[cfg(test)]
mod tests {
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
}
