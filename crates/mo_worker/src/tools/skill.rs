//! `load_skill` tool: return a global skill's full `SKILL.md` instructions
//! plus the absolute path of its folder, so the agent can follow the skill
//! and read the reference files, scripts, and other resources bundled with
//! it via `read_file`.

use std::path::Path;

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
    let skill = mo_core::skills::find_skill(agents_dir, name)
        .ok_or_else(|| format!("skill not found: {name}"))?;
    let body = std::fs::read_to_string(skill.path.join("SKILL.md"))
        .map_err(|e| format!("failed to read skill {name}: {e}"))?;
    Ok(format!("Path: {}\n\n{}", skill.path.display(), body))
}

// Unit tests live in `mo_worker/src/tests/tools/skill_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/skill_tests.rs"]
mod tests;
