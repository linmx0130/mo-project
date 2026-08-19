//! Unit tests for the `tools::fs` module — production code lives in
//! `mo_worker/src/tools/fs.rs`. Wired from there with `#[cfg(test)] #[path = "../tests/tools/fs_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

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
        read_file(&workdir, "notes.txt", &[], &[]).unwrap(),
        "line one\nline two\n"
    );
    assert_eq!(
        read_file(
            &workdir,
            &workdir.join("notes.txt").display().to_string(),
            &[],
            &[],
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
        read_file(&workdir, &abs, std::slice::from_ref(&root), &[]).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    // Without the extra root the same file is rejected.
    assert!(read_file(&workdir, &abs, &[], &[]).is_err());
    // Files outside the root (e.g. a sibling dir) are still rejected.
    let other = dir.path().join("other").join("x.txt");
    fs::create_dir_all(dir.path().join("other")).unwrap();
    fs::write(&other, "x").unwrap();
    assert!(
        read_file(
            &workdir,
            &other.display().to_string(),
            std::slice::from_ref(&root),
            &[],
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
            std::slice::from_ref(&root),
            &[],
        )
        .is_err()
    );
}

#[test]
fn rejects_escapes() {
    let (dir, workdir) = setup();
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    // Traversal outside the workdir.
    assert!(read_file(&workdir, "../secret.txt", &[], &[]).is_err());
    assert!(read_file(&workdir, "sub/../../secret.txt", &[], &[]).is_err());
    // Absolute path outside.
    let outside = dir.path().join("secret.txt").display().to_string();
    assert!(read_file(&workdir, &outside, &[], &[]).is_err());
    // Nonexistent parent.
    assert!(read_file(&workdir, "no/such/dir/file.txt", &[], &[]).is_err());
}

#[test]
fn rejects_symlink_escape() {
    let (dir, workdir) = setup();
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(dir.path().join("secret.txt"), workdir.join("link.txt")).unwrap();
    assert!(read_file(&workdir, "link.txt", &[], &[]).is_err());
}

/// `classify_read` mirrors `resolve_readable` but never rejects an escape:
/// a file inside a root classifies `Allowed`, a file outside every root
/// classifies `Outside` (with the resolved path), and unreadable paths
/// still error.
#[test]
fn classify_read_inside_and_outside() {
    let (dir, workdir) = setup();
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    // Inside the workdir → Allowed.
    match classify_read(&workdir, "notes.txt", &[]).unwrap() {
        PathClass::Allowed(full) => {
            assert_eq!(full, workdir.join("notes.txt").canonicalize().unwrap())
        }
        other => panic!("expected Allowed, got: {other:?}"),
    }
    // Inside an extra root → Allowed.
    let skill = dir.path().join("skill-a");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("script.sh"), "hi\n").unwrap();
    let root = skill.canonicalize().unwrap();
    assert!(matches!(
        classify_read(
            &workdir,
            &skill.join("script.sh").display().to_string(),
            &[root]
        )
        .unwrap(),
        PathClass::Allowed(_)
    ));
    // Outside every root → Outside, carrying the canonical path.
    match classify_read(
        &workdir,
        &dir.path().join("secret.txt").display().to_string(),
        &[],
    )
    .unwrap()
    {
        PathClass::Outside(full) => {
            assert_eq!(full, dir.path().join("secret.txt").canonicalize().unwrap())
        }
        other => panic!("expected Outside, got: {other:?}"),
    }
    // Missing file / missing parent still error (nothing to ask about).
    assert!(classify_read(&workdir, "no-such-file.txt", &[]).is_err());
    assert!(classify_read(&workdir, "no/such/dir/f.txt", &[]).is_err());
}

/// `classify_write` mirrors `resolve_path` (the file itself need not
/// exist) but never rejects an escape.
#[test]
fn classify_write_inside_and_outside() {
    let (dir, workdir) = setup();
    // Existing and new paths inside the workdir → Allowed.
    assert!(matches!(
        classify_write(&workdir, "notes.txt").unwrap(),
        PathClass::Allowed(_)
    ));
    fs::create_dir_all(workdir.join("brand")).unwrap();
    assert!(matches!(
        classify_write(&workdir, "brand/new.txt").unwrap(),
        PathClass::Allowed(_)
    ));
    // A path outside → Outside (even when the file exists there).
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    assert!(matches!(
        classify_write(
            &workdir,
            &dir.path().join("secret.txt").display().to_string()
        )
        .unwrap(),
        PathClass::Outside(_)
    ));
    // Missing parent outside still errors (nothing to write to).
    assert!(classify_write(&workdir, "/no/such/dir/f.txt").is_err());
}

/// A user-approved resolved path bypasses the containment check for that
/// one path (the tool layer asks the user before approving), while
/// everything else stays sandboxed.
#[test]
fn approved_paths_bypass_sandbox_for_exact_path_only() {
    let (dir, workdir) = setup();
    fs::write(dir.path().join("secret.txt"), "line one\n").unwrap();
    let outside = dir.path().join("secret.txt");
    let approved = vec![outside.canonicalize().unwrap()];

    // read_file: the approved absolute path is readable...
    let out = read_file(&workdir, &outside.display().to_string(), &[], &approved).unwrap();
    assert!(out.contains("line one"));
    // ...but an unapproved sibling in the same directory is not.
    fs::write(dir.path().join("other.txt"), "x").unwrap();
    assert!(
        read_file(
            &workdir,
            &dir.path().join("other.txt").display().to_string(),
            &[],
            &approved,
        )
        .is_err()
    );

    // create_file: a new file at the approved path (a path that does not
    // exist yet but whose parent is outside) is created. The approved list
    // carries the parent-canonical resolved path, exactly as the tool
    // layer's classification produces it.
    let new_file = dir.path().join("new-outside.txt");
    let approved_new = vec![dir.path().canonicalize().unwrap().join("new-outside.txt")];
    create_file(
        &workdir,
        &new_file.display().to_string(),
        "hello\n",
        &approved_new,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&new_file).unwrap(), "hello\n");
    // ...and create_file still refuses a relative path that escapes via
    // `..` (not in the approved list).
    assert!(create_file(&workdir, "../new-outside.txt", "x", &approved_new).is_err());

    // edit_file: an approved outside file can be edited (the tool returns
    // a confirmation, not the content)...
    let edited = edit_file(
        &workdir,
        &outside.display().to_string(),
        "line one",
        "LINE ONE",
        false,
        &approved,
    )
    .unwrap();
    assert!(edited.contains("edit applied"), "got: {edited}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "LINE ONE\n");
    // ...and an unapproved outside file still cannot.
    assert!(
        edit_file(
            &workdir,
            &dir.path().join("other.txt").display().to_string(),
            "x",
            "y",
            false,
            &approved,
        )
        .is_err()
    );

    // remove_file: an approved outside file is removed.
    remove_file(&workdir, &new_file.display().to_string(), &approved_new).unwrap();
    assert!(!new_file.exists());
    assert!(remove_file(&workdir, &outside.display().to_string(), &[]).is_err());
}

#[test]
fn edit_unique_match_returns_confirmation_and_applies() {
    let (_dir, workdir) = setup();
    let confirmation = edit_file(&workdir, "notes.txt", "line one", "ONE", false, &[]).unwrap();
    assert!(confirmation.contains("edit applied"), "got: {confirmation}");
    assert!(
        !confirmation.contains("line two"),
        "the tool must not return the file content: {confirmation}"
    );
    assert_eq!(
        read_file(&workdir, "notes.txt", &[], &[]).unwrap(),
        "ONE\nline two\n"
    );
}

#[test]
fn edit_requires_unique_match() {
    let (_dir, workdir) = setup();
    fs::write(workdir.join("dup.txt"), "same\nsame\n").unwrap();
    let err = edit_file(&workdir, "dup.txt", "same", "x", false, &[]).unwrap_err();
    assert!(err.contains("exactly once"), "got: {err}");
    // replace_all handles it (returning a confirmation, not the content).
    let confirmation = edit_file(&workdir, "dup.txt", "same", "x", true, &[]).unwrap();
    assert!(confirmation.contains("edit applied"), "got: {confirmation}");
    assert_eq!(
        fs::read_to_string(workdir.join("dup.txt")).unwrap(),
        "x\nx\n"
    );
}

#[test]
fn edit_missing_old_string_errors() {
    let (_dir, workdir) = setup();
    let err = edit_file(&workdir, "notes.txt", "nope", "x", false, &[]).unwrap_err();
    assert!(err.contains("found 0 matches"), "got: {err}");
}

#[test]
fn create_file_writes_content_and_refuses_existing() {
    let (_dir, workdir) = setup();
    let written = create_file(&workdir, "new.txt", "hello\nworld\n", &[]).unwrap();
    assert_eq!(written, "hello\nworld\n");
    assert_eq!(
        read_file(&workdir, "new.txt", &[], &[]).unwrap(),
        "hello\nworld\n"
    );
    // Refuses to overwrite an existing file.
    let err = create_file(&workdir, "new.txt", "again", &[]).unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");
    let err = create_file(&workdir, "notes.txt", "again", &[]).unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");
    // Parent directory must exist.
    assert!(create_file(&workdir, "no/such/dir/f.txt", "x", &[]).is_err());
}

#[test]
fn create_file_rejects_escapes() {
    let (dir, workdir) = setup();
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    let outside = dir.path().join("secret.txt").display().to_string();
    assert!(create_file(&workdir, &outside, "x", &[]).is_err());
    assert!(create_file(&workdir, "../evil.txt", "x", &[]).is_err());
    // A symlinked parent pointing outside is rejected by resolve_path.
    std::os::unix::fs::symlink(dir.path(), workdir.join("outside")).unwrap();
    assert!(create_file(&workdir, "outside/evil.txt", "x", &[]).is_err());
}

#[test]
fn remove_file_removes_and_refuses_dirs_and_symlinks() {
    let (dir, workdir) = setup();
    fs::write(workdir.join("tmp.txt"), "x").unwrap();
    assert!(
        remove_file(&workdir, "tmp.txt", &[])
            .unwrap()
            .contains("removed")
    );
    assert!(!workdir.join("tmp.txt").exists());
    assert!(remove_file(&workdir, "tmp.txt", &[]).is_err()); // already gone

    // Refuses to remove a directory.
    fs::create_dir_all(workdir.join("subdir")).unwrap();
    let err = remove_file(&workdir, "subdir", &[]).unwrap_err();
    assert!(err.contains("not a regular file"), "got: {err}");

    // Refuses to remove a symlink (even one pointing inside the workdir).
    fs::write(workdir.join("real.txt"), "x").unwrap();
    std::os::unix::fs::symlink(workdir.join("real.txt"), workdir.join("link.txt")).unwrap();
    let err = remove_file(&workdir, "link.txt", &[]).unwrap_err();
    assert!(err.contains("refusing to remove symlink"), "got: {err}");
    assert!(workdir.join("real.txt").exists());

    // Escapes are rejected.
    fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    assert!(remove_file(&workdir, "../secret.txt", &[]).is_err());
    let outside = dir.path().join("secret.txt").display().to_string();
    assert!(remove_file(&workdir, &outside, &[]).is_err());
}
