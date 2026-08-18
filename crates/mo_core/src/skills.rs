//! Global skill discovery: `<agents_dir>/<skill>/SKILL.md` and
//! `<agents_dir>/skills/<skill>/SKILL.md`.
//!
//! A skill's `SKILL.md` carries YAML frontmatter (`name`, `description`)
//! followed by the instructions body. The system prompt lists only the name
//! and description metadata; the `load_skill` tool returns the full
//! `SKILL.md` plus the absolute path of the skill folder, so the agent can
//! also read reference files, scripts, and other resources bundled with the
//! skill.
//!
//! Shared by the gateway (the skill list for the UI and the status-bar
//! "load skill" endpoint) and the worker (the system prompt's on-demand
//! listing and forced-skill inlining, plus the `load_skill` tool).

use std::path::{Path, PathBuf};

/// A discovered global skill: frontmatter metadata plus the absolute path of
/// the skill folder (which contains `SKILL.md` and any reference resources).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// The marker prefix the gateway wraps a status-bar skill load in when it
/// journals the skill's `SKILL.md` as a user message, so the model
/// understands that the pasted file is the user force-loading a skill (the
/// same pattern as the worker's handoff / answer prefixes).
pub const SKILL_LOAD_USER_PREFIX: &str = "[The user loaded the skill \"%s\" — its SKILL.md \
     instructions follow. Treat them as active and follow them.]\n\n";

/// The user-role message the gateway journals when the user loads a skill
/// from the status bar: the skill's full `SKILL.md` content wrapped in a
/// marker so the model understands what it is (see
/// `SKILL_LOAD_USER_PREFIX`).
pub fn skill_load_message(name: &str, content: &str) -> String {
    format!(
        "{}{}",
        SKILL_LOAD_USER_PREFIX.replace("%s", name),
        content.trim()
    )
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

// Unit tests live in `mo_core/src/tests/skills_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/skills_tests.rs"]
mod tests;
