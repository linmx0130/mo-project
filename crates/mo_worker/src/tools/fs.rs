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
        assert_eq!(
            read_file(&workdir, "notes.txt", &[]).unwrap(),
            "line one\nline two\n"
        );
        assert_eq!(
            read_file(
                &workdir,
                &workdir.join("notes.txt").display().to_string(),
                &[]
            )
            .unwrap(),
            "line one\nline two\n"
        );
    }

    #[test]
    fn read_allows_extra_roots() {
        let (dir, workdir) = setup();
        let skill = dir.path().join("skill-a");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
        let root = skill.canonicalize().unwrap();
        let abs = skill.join("script.sh").display().to_string();
        // Inside the extra root: readable (via its absolute path).
        assert_eq!(
            read_file(&workdir, &abs, std::slice::from_ref(&root)).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        // Without the extra root the same file is rejected.
        assert!(read_file(&workdir, &abs, &[]).is_err());
        // Files outside the root (e.g. a sibling dir) are still rejected.
        let other = dir.path().join("other").join("x.txt");
        fs::create_dir_all(dir.path().join("other")).unwrap();
        fs::write(&other, "x").unwrap();
        assert!(
            read_file(
                &workdir,
                &other.display().to_string(),
                std::slice::from_ref(&root)
            )
            .is_err()
        );
        // A symlink inside the root pointing outside is rejected.
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), skill.join("leak.txt")).unwrap();
        assert!(
            read_file(
                &workdir,
                &skill.join("leak.txt").display().to_string(),
                std::slice::from_ref(&root)
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_escapes() {
        let (dir, workdir) = setup();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        // Traversal outside the workdir.
        assert!(read_file(&workdir, "../secret.txt", &[]).is_err());
        assert!(read_file(&workdir, "sub/../../secret.txt", &[]).is_err());
        // Absolute path outside.
        let outside = dir.path().join("secret.txt").display().to_string();
        assert!(read_file(&workdir, &outside, &[]).is_err());
        // Nonexistent parent.
        assert!(read_file(&workdir, "no/such/dir/file.txt", &[]).is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let (dir, workdir) = setup();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), workdir.join("link.txt"))
            .unwrap();
        assert!(read_file(&workdir, "link.txt", &[]).is_err());
    }

    #[test]
    fn edit_unique_match_and_content_returned() {
        let (_dir, workdir) = setup();
        let new_content = edit_file(&workdir, "notes.txt", "line one", "ONE", false).unwrap();
        assert_eq!(new_content, "ONE\nline two\n");
        assert_eq!(
            read_file(&workdir, "notes.txt", &[]).unwrap(),
            "ONE\nline two\n"
        );
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

    #[test]
    fn create_file_writes_content_and_refuses_existing() {
        let (_dir, workdir) = setup();
        let written = create_file(&workdir, "new.txt", "hello\nworld\n").unwrap();
        assert_eq!(written, "hello\nworld\n");
        assert_eq!(
            read_file(&workdir, "new.txt", &[]).unwrap(),
            "hello\nworld\n"
        );
        // Refuses to overwrite an existing file.
        let err = create_file(&workdir, "new.txt", "again").unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        let err = create_file(&workdir, "notes.txt", "again").unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        // Parent directory must exist.
        assert!(create_file(&workdir, "no/such/dir/f.txt", "x").is_err());
    }

    #[test]
    fn create_file_rejects_escapes() {
        let (dir, workdir) = setup();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        let outside = dir.path().join("secret.txt").display().to_string();
        assert!(create_file(&workdir, &outside, "x").is_err());
        assert!(create_file(&workdir, "../evil.txt", "x").is_err());
        // A symlinked parent pointing outside is rejected by resolve_path.
        std::os::unix::fs::symlink(dir.path(), workdir.join("outside")).unwrap();
        assert!(create_file(&workdir, "outside/evil.txt", "x").is_err());
    }

    #[test]
    fn remove_file_removes_and_refuses_dirs_and_symlinks() {
        let (dir, workdir) = setup();
        fs::write(workdir.join("tmp.txt"), "x").unwrap();
        assert!(
            remove_file(&workdir, "tmp.txt")
                .unwrap()
                .contains("removed")
        );
        assert!(!workdir.join("tmp.txt").exists());
        assert!(remove_file(&workdir, "tmp.txt").is_err()); // already gone

        // Refuses to remove a directory.
        fs::create_dir_all(workdir.join("subdir")).unwrap();
        let err = remove_file(&workdir, "subdir").unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");

        // Refuses to remove a symlink (even one pointing inside the workdir).
        fs::write(workdir.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(workdir.join("real.txt"), workdir.join("link.txt")).unwrap();
        let err = remove_file(&workdir, "link.txt").unwrap_err();
        assert!(err.contains("refusing to remove symlink"), "got: {err}");
        assert!(workdir.join("real.txt").exists());

        // Escapes are rejected.
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        assert!(remove_file(&workdir, "../secret.txt").is_err());
        let outside = dir.path().join("secret.txt").display().to_string();
        assert!(remove_file(&workdir, &outside).is_err());
    }
}
