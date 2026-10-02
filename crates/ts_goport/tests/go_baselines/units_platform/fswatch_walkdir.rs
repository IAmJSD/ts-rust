//! Go: `internal/fswatch/walkdir_test.go`. Each Go test runs on the native
//! walk (`walkdir_unix::walk_dir`) and on `walk_dir_generic`, as Go
//! `runWalkDirTest` does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use ts_goport::fswatch::walk_dir_generic;
use ts_goport::fswatch::walkdir_unix::walk_dir;
use ts_goport::gostd::{GoError, errors};

use super::fswatch_watcher::new_tmp_dir;

type WalkFn<'a> = Option<&'a mut dyn FnMut(&str, bool) -> Result<(), GoError>>;

// Go: walkdir_test.go:11 walkDirFunc
type Walk = fn(&str, bool, WalkFn<'_>) -> Result<(), GoError>;

fn native(dir: &str, recursive: bool, f: WalkFn<'_>) -> Result<(), GoError> {
    walk_dir(dir, recursive, f)
}

fn generic(dir: &str, recursive: bool, f: WalkFn<'_>) -> Result<(), GoError> {
    match f {
        Some(f) => walk_dir_generic(dir, recursive, Some(f)),
        None => walk_dir_generic(dir, recursive, None),
    }
}

// Go: walkdir_test.go:13 runWalkDirTest
fn run_walk_dir_test(test: fn(&str, Walk)) {
    test("native", native);
    test("generic", generic);
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

/// Walks `root` recursively and returns every path with its isDir flag.
fn walk_all(walk: Walk, root: &str) -> HashMap<String, bool> {
    let found = RefCell::new(HashMap::new());
    let mut f = |path: &str, is_dir: bool| -> Result<(), GoError> {
        found.borrow_mut().insert(path.to_string(), is_dir);
        Ok(())
    };
    walk(root, true, Some(&mut f)).unwrap_or_else(|e| panic!("walk: {}", e.error()));
    found.into_inner()
}

// Go: walkdir_test.go:30 TestWalkDirDoesNotFollowSymlinkedDir
#[test]
fn test_walk_dir_does_not_follow_symlinked_dir() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, root) = new_tmp_dir();
        let (_other, other) = new_tmp_dir();
        let target = other.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("child"), "hidden").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let found = walk_all(walk, &s(&root));
        let is_dir = found
            .get(&s(&link))
            .unwrap_or_else(|| panic!("{name}: symlink {link:?} missing from walk"));
        assert!(
            !is_dir,
            "{name}: symlink {link:?} was treated as a directory"
        );
        assert!(
            !found.contains_key(&s(&link.join("child"))),
            "{name}: walkDir followed symlinked directory"
        );
    });
}

fn geteuid() -> u32 {
    // The owner of a file this process makes is its effective uid.
    let (_other, other) = new_tmp_dir();
    let p = other.join("uid");
    std::fs::write(&p, "").unwrap();
    std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&p).unwrap())
}

// Go: walkdir_test.go:85 TestWalkDirIgnoresUnreadableSubdir
#[test]
fn test_walk_dir_ignores_unreadable_subdir() {
    if geteuid() == 0 {
        println!("SKIP: root can read directories regardless of mode bits");
        return;
    }
    run_walk_dir_test(|name, walk| {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = new_tmp_dir();
        let denied = root.join("denied");
        std::fs::create_dir(&denied).unwrap();
        let child = denied.join("child");
        std::fs::write(&child, "hidden").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0)).unwrap();

        let found = walk_all(walk, &s(&root));
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            !found.contains_key(&s(&denied)),
            "{name}: unreadable directory should be ignored, found {denied:?}"
        );
        assert!(
            !found.contains_key(&s(&child)),
            "{name}: unreadable child should be ignored, found {child:?}"
        );
    });
}

// Go: walkdir_test.go:126 TestWalkDirMissingDir
#[test]
fn test_walk_dir_missing_dir() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, tmp) = new_tmp_dir();
        let dir = tmp.join("nonexistent");
        assert!(
            walk(&s(&dir), true, None).is_err(),
            "{name}: expected error for missing directory"
        );
    });
}

// Go: walkdir_test.go:134 TestWalkDirNotADir
#[test]
fn test_walk_dir_not_a_dir() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, tmp) = new_tmp_dir();
        let f = tmp.join("file");
        std::fs::write(&f, "x").unwrap();
        assert!(
            walk(&s(&f), true, None).is_err(),
            "{name}: expected error for non-directory"
        );
    });
}

// Go: walkdir_test.go:145 TestWalkDirEntries
#[test]
fn test_walk_dir_entries() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, root) = new_tmp_dir();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.txt"), "b").unwrap();

        let found = walk_all(walk, &s(&root));
        assert!(
            found.contains_key(&s(&root.join("a.txt"))),
            "{name}: missing a.txt"
        );
        assert!(found.contains_key(&s(&sub)), "{name}: missing sub/");
        assert!(
            found.contains_key(&s(&sub.join("b.txt"))),
            "{name}: missing sub/b.txt"
        );
    });
}

// Go: walkdir_test.go:177 TestWalkDirCallback
#[test]
fn test_walk_dir_callback() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, root) = new_tmp_dir();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), "f").unwrap();

        let found = walk_all(walk, &s(&root));
        let dirs: Vec<&String> = found.iter().filter(|(_, d)| **d).map(|(p, _)| p).collect();
        let files: Vec<&String> = found.iter().filter(|(_, d)| !**d).map(|(p, _)| p).collect();
        assert!(
            dirs.len() >= 2,
            "{name}: expected at least 2 dirs (root + sub), got {dirs:?}"
        );
        assert!(!files.is_empty(), "{name}: expected at least 1 file");
    });
}

// Go: walkdir_test.go:208 TestWalkDirCallbackError
#[test]
fn test_walk_dir_callback_error() {
    run_walk_dir_test(|name, walk| {
        let (_tmp, root) = new_tmp_dir();
        std::fs::write(root.join("a.txt"), "a").unwrap();

        let sentinel = errors::new("stop");
        let mut f = |_: &str, _: bool| -> Result<(), GoError> { Err(sentinel.clone()) };
        let err = walk(&s(&root), true, Some(&mut f));
        match err {
            Err(err) => assert!(
                errors::is(&err, &sentinel),
                "{name}: expected sentinel error, got {}",
                err.error()
            ),
            Ok(()) => panic!("{name}: expected sentinel error, got nil"),
        }
    });
}
