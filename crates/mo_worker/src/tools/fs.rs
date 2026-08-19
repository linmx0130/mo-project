//! Filesystem tools with workdir sandboxing.
//!
//! Every path is resolved against the session workdir, canonicalized, and
//! rejected if it would escape the workdir root (including via symlinks) —
//! unless the exact resolved path was *approved* by the user: the tool
//! layer asks the user for permission to access paths outside the allowed
//! roots, and on approval re-runs the operation with the approved
//! canonical path in `approved`, which bypasses the containment check for
//! that one path (defense in depth stays intact for everything else).

use std::fs;
use std::path::{Path, PathBuf};

const OUTPUT_CAP: usize = 1024 * 1024; // 1 MB

/// How a raw path classifies against the sandbox roots: `Allowed` when it
/// resolves inside a root, `Outside` when it resolves to an existing path
/// outside every root. `Outside` carries the resolved path so the caller
/// can ask the user for permission and re-run the operation with that
/// exact path approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathClass {
    Allowed(PathBuf),
    Outside(PathBuf),
}

/// Resolve `raw` against `workdir`: absolute paths are taken as-is, relative
/// paths are joined to the canonical workdir. The parent directory must
/// exist and must stay inside the workdir (or be exactly one of the
/// user-approved resolved paths). Symlinks in the parent chain are resolved
/// by canonicalization. Returns the resolved path (parent-canonical, file
/// name preserved).
pub fn resolve_path(workdir: &Path, raw: &str, approved: &[PathBuf]) -> Result<PathBuf, String> {
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
    if !parent_canon.starts_with(&workdir_canon) && !approved.contains(&resolved) {
        return Err(format!("path escapes the working directory: {raw}"));
    }
    Ok(resolved)
}

/// Like `resolve_path` but also requires the file itself to exist and, once
/// canonicalized, to stay inside the workdir (catches symlink escapes) or
/// be one of the user-approved paths.
pub fn resolve_existing(
    workdir: &Path,
    raw: &str,
    approved: &[PathBuf],
) -> Result<PathBuf, String> {
    resolve_readable(workdir, raw, &[], approved)
}

/// Resolve `raw` for reading: the file must exist and, once canonicalized,
/// must be inside `workdir`, inside one of `extra_roots` (used to allow
/// `read_file` to reach global skill folders and the session scratch dir),
/// or be exactly one of the user-approved paths. Relative paths are
/// resolved against the workdir; absolute paths are taken as-is.
pub fn resolve_readable(
    workdir: &Path,
    raw: &str,
    extra_roots: &[PathBuf],
    approved: &[PathBuf],
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
    let approved = approved.iter().any(|p| p == &canonical);
    if !in_workdir && !in_extra && !approved {
        return Err(format!("path escapes the working directory: {raw}"));
    }
    Ok(canonical)
}

/// Classify `raw` for reading: `Allowed` when the file exists inside the
/// workdir or one of `extra_roots`, `Outside` when it exists outside every
/// root, `Err` for anything unreadable (missing file, missing parent).
/// Unlike `resolve_readable` this never rejects an escape — the caller
/// decides whether to ask the user for permission.
pub fn classify_read(
    workdir: &Path,
    raw: &str,
    extra_roots: &[PathBuf],
) -> Result<PathClass, String> {
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
    if in_workdir || in_extra {
        Ok(PathClass::Allowed(canonical))
    } else {
        Ok(PathClass::Outside(canonical))
    }
}

/// Classify `raw` for writing: `Allowed` when the resolved path's parent is
/// inside the workdir (the file itself need not exist), `Outside`
/// otherwise. Never rejects an escape — the caller decides whether to ask
/// the user for permission or deny outright.
pub fn classify_write(workdir: &Path, raw: &str) -> Result<PathClass, String> {
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
    if parent_canon.starts_with(&workdir_canon) {
        Ok(PathClass::Allowed(resolved))
    } else {
        Ok(PathClass::Outside(resolved))
    }
}

/// Read a UTF-8 file (capped at ~1 MB with an explicit truncation note).
/// The file must be inside the workdir, inside one of `extra_roots`
/// (global skill folders, the session scratch dir), or be exactly one of
/// the user-approved paths.
pub fn read_file(
    workdir: &Path,
    raw: &str,
    extra_roots: &[PathBuf],
    approved: &[PathBuf],
) -> Result<String, String> {
    let full = resolve_readable(workdir, raw, extra_roots, approved)?;
    let content = fs::read_to_string(&full).map_err(|e| format!("failed to read {raw}: {e}"))?;
    Ok(cap_output(&content))
}

/// Create a new file with `content`. The parent directory must already
/// exist (and stay inside the workdir, or be a user-approved resolved
/// path); the file itself must not exist. Returns the content written.
pub fn create_file(
    workdir: &Path,
    raw: &str,
    content: &str,
    approved: &[PathBuf],
) -> Result<String, String> {
    let full = resolve_path(workdir, raw, approved)?;
    if full.exists() {
        return Err(format!("file already exists: {raw}"));
    }
    fs::write(&full, content).map_err(|e| format!("failed to write {raw}: {e}"))?;
    Ok(content.to_string())
}

/// Remove a regular file. Directories and symlinks are refused; the path
/// must stay inside the workdir (or be a user-approved resolved path).
/// Returns a confirmation.
pub fn remove_file(workdir: &Path, raw: &str, approved: &[PathBuf]) -> Result<String, String> {
    let full = resolve_path(workdir, raw, approved)?;
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
/// the match must be unique. Returns a confirmation on success — the caller
/// can read the file back with `read_file` to inspect the result.
pub fn edit_file(
    workdir: &Path,
    raw: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    approved: &[PathBuf],
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    let full = resolve_existing(workdir, raw, approved)?;
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
    Ok(format!("edit applied successfully to {raw}"))
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
