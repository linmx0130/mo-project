//! Filesystem tools with workdir sandboxing.
//!
//! Every path is resolved against the session workdir, canonicalized, and
//! rejected if it would escape the workdir root (including via symlinks).

use std::fs;
use std::path::{Path, PathBuf};

const OUTPUT_CAP: usize = 1024 * 1024; // 1 MB

/// Resolve `raw` against `workdir`: absolute paths are taken as-is, relative
/// paths are joined to the canonical workdir. The parent directory must
/// exist and must stay inside the workdir. Symlinks in the parent chain are
/// resolved by canonicalization. Returns the resolved path (parent-canonical,
/// file name preserved).
pub fn resolve_path(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let workdir_canon = workdir.canonicalize().map_err(|e| {
        format!(
            "working directory does not exist: {} ({e})",
            workdir.display()
        )
    })?;
    if raw.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    let raw_path = Path::new(raw);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        workdir_canon.join(raw_path)
    };
    let parent = candidate
        .parent()
        .ok_or_else(|| format!("path has no parent: {raw}"))?;
    let parent_canon = parent.canonicalize().map_err(|e| {
        format!(
            "parent directory does not exist: {} ({e})",
            parent.display()
        )
    })?;
    if !parent_canon.starts_with(&workdir_canon) {
        return Err(format!("path escapes the working directory: {raw}"));
    }
    let file_name = candidate
        .file_name()
        .ok_or_else(|| format!("path has no file name: {raw}"))?;
    Ok(parent_canon.join(file_name))
}

/// Like `resolve_path` but also requires the file itself to exist and, once
/// canonicalized, to stay inside the workdir (catches symlink escapes).
pub fn resolve_existing(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    resolve_readable(workdir, raw, &[])
}

/// Resolve `raw` for reading: the file must exist and, once canonicalized,
/// must be inside `workdir` or inside one of `extra_roots` (used to allow
/// `read_file` to reach global skill folders). Relative paths are resolved
/// against the workdir; absolute paths are taken as-is.
pub fn resolve_readable(
    workdir: &Path,
    raw: &str,
    extra_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    let workdir_canon = workdir.canonicalize().map_err(|e| {
        format!(
            "working directory does not exist: {} ({e})",
            workdir.display()
        )
    })?;
    if raw.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    let raw_path = Path::new(raw);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        workdir_canon.join(raw_path)
    };
    let parent = candidate
        .parent()
        .ok_or_else(|| format!("path has no parent: {raw}"))?;
    let parent_canon = parent.canonicalize().map_err(|e| {
        format!(
            "parent directory does not exist: {} ({e})",
            parent.display()
        )
    })?;
    let file_name = candidate
        .file_name()
        .ok_or_else(|| format!("path has no file name: {raw}"))?;
    let resolved = parent_canon.join(file_name);
    let canonical = resolved
        .canonicalize()
        .map_err(|e| format!("file does not exist: {raw} ({e})"))?;
    let in_workdir = canonical.starts_with(&workdir_canon);
    let in_extra = extra_roots.iter().any(|root| canonical.starts_with(root));
    if !in_workdir && !in_extra {
        return Err(format!("path escapes the working directory: {raw}"));
    }
    Ok(canonical)
}

/// Read a UTF-8 file (capped at ~1 MB with an explicit truncation note).
/// The file must be inside the workdir or inside one of `extra_roots`
/// (global skill folders).
pub fn read_file(workdir: &Path, raw: &str, extra_roots: &[PathBuf]) -> Result<String, String> {
    let full = resolve_readable(workdir, raw, extra_roots)?;
    let content = fs::read_to_string(&full).map_err(|e| format!("failed to read {raw}: {e}"))?;
    Ok(cap_output(&content))
}

/// Create a new file with `content`. The parent directory must already
/// exist (and stay inside the workdir); the file itself must not exist.
/// Returns the content written.
pub fn create_file(workdir: &Path, raw: &str, content: &str) -> Result<String, String> {
    let full = resolve_path(workdir, raw)?;
    if full.exists() {
        return Err(format!("file already exists: {raw}"));
    }
    fs::write(&full, content).map_err(|e| format!("failed to write {raw}: {e}"))?;
    Ok(content.to_string())
}

/// Remove a regular file. Directories and symlinks are refused; the path
/// must stay inside the workdir. Returns a confirmation.
pub fn remove_file(workdir: &Path, raw: &str) -> Result<String, String> {
    let full = resolve_path(workdir, raw)?;
    let meta =
        fs::symlink_metadata(&full).map_err(|e| format!("file does not exist: {raw} ({e})"))?;
    if meta.file_type().is_symlink() {
        return Err(format!("refusing to remove symlink: {raw}"));
    }
    if !meta.is_file() {
        return Err(format!("not a regular file: {raw}"));
    }
    fs::remove_file(&full).map_err(|e| format!("failed to remove {raw}: {e}"))?;
    Ok(format!("removed {raw}"))
}

/// Replace `old_string` with `new_string` in the file. Without `replace_all`
/// the match must be unique. Returns the full new file content.
pub fn edit_file(
    workdir: &Path,
    raw: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    let full = resolve_existing(workdir, raw)?;
    let content = fs::read_to_string(&full).map_err(|e| format!("failed to read {raw}: {e}"))?;
    let matches = content.matches(old_string).count();
    let updated = if replace_all {
        if matches == 0 {
            return Err(format!("old_string not found in {raw}"));
        }
        content.replace(old_string, new_string)
    } else {
        if matches != 1 {
            return Err(format!(
                "old_string must match exactly once in {raw}, found {matches} matches"
            ));
        }
        content.replacen(old_string, new_string, 1)
    };
    fs::write(&full, &updated).map_err(|e| format!("failed to write {raw}: {e}"))?;
    Ok(updated)
}

fn cap_output(text: &str) -> String {
    if text.len() > OUTPUT_CAP {
        let cut = text.floor_char_boundary(OUTPUT_CAP);
        format!(
            "{}\n\n[output truncated: {} bytes total, showing first {}]",
            &text[..cut],
            text.len(),
            cut
        )
    } else {
        text.to_string()
    }
}

// Unit tests live in `mo_worker/src/tests/tools/fs_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "../tests/tools/fs_tests.rs"]
mod tests;
