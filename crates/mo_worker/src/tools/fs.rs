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
    let workdir_canon = workdir
        .canonicalize()
        .map_err(|e| format!("working directory does not exist: {} ({e})", workdir.display()))?;
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
    let parent_canon = parent
        .canonicalize()
        .map_err(|e| format!("parent directory does not exist: {} ({e})", parent.display()))?;
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
    let resolved = resolve_path(workdir, raw)?;
    let canonical = resolved
        .canonicalize()
        .map_err(|e| format!("file does not exist: {raw} ({e})"))?;
    let workdir_canon = workdir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&workdir_canon) {
        return Err(format!("path escapes the working directory: {raw}"));
    }
    Ok(canonical)
}

/// Read a UTF-8 file (capped at ~1 MB with an explicit truncation note).
pub fn read_file(workdir: &Path, raw: &str) -> Result<String, String> {
    let full = resolve_existing(workdir, raw)?;
    let content = fs::read_to_string(&full)
        .map_err(|e| format!("failed to read {raw}: {e}"))?;
    Ok(cap_output(&content))
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
    let content = fs::read_to_string(&full)
        .map_err(|e| format!("failed to read {raw}: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("work");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("notes.txt"), "line one\nline two\n").unwrap();
        (dir, workdir)
    }

    #[test]
    fn read_relative_and_absolute() {
        let (_dir, workdir) = setup();
        assert_eq!(read_file(&workdir, "notes.txt").unwrap(), "line one\nline two\n");
        assert_eq!(
            read_file(&workdir, &workdir.join("notes.txt").display().to_string()).unwrap(),
            "line one\nline two\n"
        );
    }

    #[test]
    fn rejects_escapes() {
        let (dir, workdir) = setup();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        // Traversal outside the workdir.
        assert!(read_file(&workdir, "../secret.txt").is_err());
        assert!(read_file(&workdir, "sub/../../secret.txt").is_err());
        // Absolute path outside.
        let outside = dir.path().join("secret.txt").display().to_string();
        assert!(read_file(&workdir, &outside).is_err());
        // Nonexistent parent.
        assert!(read_file(&workdir, "no/such/dir/file.txt").is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let (dir, workdir) = setup();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), workdir.join("link.txt")).unwrap();
        assert!(read_file(&workdir, "link.txt").is_err());
    }

    #[test]
    fn edit_unique_match_and_content_returned() {
        let (_dir, workdir) = setup();
        let new_content = edit_file(&workdir, "notes.txt", "line one", "ONE", false).unwrap();
        assert_eq!(new_content, "ONE\nline two\n");
        assert_eq!(read_file(&workdir, "notes.txt").unwrap(), "ONE\nline two\n");
    }

    #[test]
    fn edit_requires_unique_match() {
        let (_dir, workdir) = setup();
        fs::write(workdir.join("dup.txt"), "same\nsame\n").unwrap();
        let err = edit_file(&workdir, "dup.txt", "same", "x", false).unwrap_err();
        assert!(err.contains("exactly once"), "got: {err}");
        // replace_all handles it.
        let new_content = edit_file(&workdir, "dup.txt", "same", "x", true).unwrap();
        assert_eq!(new_content, "x\nx\n");
    }

    #[test]
    fn edit_missing_old_string_errors() {
        let (_dir, workdir) = setup();
        let err = edit_file(&workdir, "notes.txt", "nope", "x", false).unwrap_err();
        assert!(err.contains("found 0 matches"), "got: {err}");
    }
}
