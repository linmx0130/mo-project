//! `load_skill` tool: return a global skill's full `SKILL.md` instructions
//! plus the absolute path of its folder, so the agent can follow the skill
//! and read the reference files, scripts, and other resources bundled with
//! it via `read_file`.

use std::path::Path;

use crate::skills;

/// Load a skill by name. The output format is:
///
/// ```text
/// Path: <skill folder path>
///
/// <SKILL.md content>
/// ```
pub fn load_skill(agents_dir: &Path, name: &str) -> Result<String, String> {
    if name.trim().is_empty() {
        return Err("skill name must not be empty".to_string());
    }
    let skill =
        skills::find_skill(agents_dir, name).ok_or_else(|| format!("skill not found: {name}"))?;
    let body = std::fs::read_to_string(skill.path.join("SKILL.md"))
        .map_err(|e| format!("failed to read skill {name}: {e}"))?;
    Ok(format!("Path: {}\n\n{}", skill.path.display(), body))
}

#[cfg(test)]
mod tests {
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
}
