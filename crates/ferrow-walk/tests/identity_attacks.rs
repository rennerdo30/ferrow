mod support;
use support::{Fixture, contains, run};

#[test]
fn alias_root_cannot_reinclude_excluded_ancestor() {
    let mut f = Fixture::new("alias-root");
    let private = f.path.join("private");
    let sub = private.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("private.txt"), b"x").unwrap();
    let alias = f.path.join("alias");
    f.link(&sub, &alias);
    for follow in [true, false] {
        assert!(run(&alias, vec![private.clone()], follow).is_empty());
    }
}

#[test]
fn alias_child_cannot_reinclude_excluded_ancestor() {
    let mut f = Fixture::new("alias-child");
    let private = f.path.join("private");
    let sub = private.join("sub");
    let root = f.path.join("root");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(sub.join("private.txt"), b"x").unwrap();
    f.link(&sub, &root.join("alias"));
    let index = run(&root, vec![private], true);
    assert!(!contains(&index, b"private.txt"));
    assert!(!contains(&index, b"alias"));
}

#[test]
fn directory_alias_excludes_target_with_following_on() {
    let mut f = Fixture::new("alias-direct");
    let target = f.path.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("private.txt"), b"x").unwrap();
    let alias = f.path.join("alias");
    f.link(&target, &alias);
    for exclude in [alias, target] {
        assert_eq!(run(&f.path, vec![exclude], true).len(), 1);
    }
}

#[test]
fn file_hard_links_follow_documented_parent_scope() {
    let f = Fixture::new("hardlinks");
    let a = f.path.join("a");
    let b = f.path.join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let secret = a.join("secret");
    std::fs::write(&secret, b"x").unwrap();
    std::fs::hard_link(&secret, a.join("same-dir")).unwrap();
    std::fs::hard_link(&secret, b.join("other-dir")).unwrap();
    let index = run(&f.path, vec![secret], false);
    assert!(!contains(&index, b"secret"));
    assert!(!contains(&index, b"same-dir"));
    assert!(contains(&index, b"other-dir"));
}

#[test]
fn file_exclude_alias_of_parent_and_file_root() {
    let mut f = Fixture::new("file-parent");
    let target = f.path.join("target");
    std::fs::create_dir(&target).unwrap();
    let file = target.join("secret");
    std::fs::write(&file, b"x").unwrap();
    let alias = f.path.join("alias");
    f.link(&target, &alias);
    assert_eq!(run(&alias, vec![file.clone()], true).len(), 1);
    assert!(run(&file, vec![file.clone()], false).is_empty());
}

#[cfg(windows)]
#[test]
fn junction_exclude_resolves_target() {
    let mut f = Fixture::new("junction-direct");
    let target = f.path.join("private");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("private.txt"), b"x").unwrap();
    let alias = f.path.join("alias");
    f.junction(&target, &alias);
    for exclude in [alias, target.clone()] {
        assert!(run(&target, vec![exclude], true).is_empty());
    }
}

#[cfg(windows)]
#[test]
fn junction_descendant_cannot_reinclude_excluded_ancestor() {
    let mut f = Fixture::new("junction-descendant");
    let private = f.path.join("private");
    let sub = private.join("sub");
    let root = f.path.join("root");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(sub.join("private.txt"), b"x").unwrap();
    let alias = root.join("alias");
    f.junction(&sub, &alias);
    for follow in [true, false] {
        assert!(run(&alias, vec![private.clone()], follow).is_empty());
    }
    assert_eq!(run(&root, vec![private], true).len(), 1);
}

#[cfg(unix)]
#[test]
fn dotdot_detour_is_not_an_excluded_ancestor() {
    let f = Fixture::new("detour");
    let private = f.path.join("private");
    let root = f.path.join("root");
    std::fs::create_dir(&private).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("keep.txt"), b"x").unwrap();
    let detour = private.join("..").join("root");
    let index = run(&detour, vec![private], false);
    assert!(contains(&index, b"keep.txt"));
    assert_eq!(index.path(0), detour);
}

#[cfg(windows)]
#[test]
fn excludes_support_long_paths_that_std_can_scan() {
    use ferrow_walk::{WalkOptions, scan};
    let mut f = Fixture::new("long-path");
    let root = f.path.join("a".repeat(130)).join("b".repeat(130));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("private.txt"), b"x").unwrap();
    assert!(root.as_os_str().len() > 260);
    assert_eq!(run(&root, vec![], false).len(), 2, "std can scan this path");
    let verbatim = std::fs::canonicalize(&root).unwrap();
    assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
    let mut failures = Vec::new();
    for scan_root in [&root, &verbatim] {
        for exclude in [&root, &verbatim] {
            for follow in [false, true] {
                let (index, report) = scan(&WalkOptions {
                    roots: vec![scan_root.clone()],
                    excludes: vec![exclude.clone()],
                    follow_symlinks: follow,
                });
                if !index.is_empty() || report.errors != 0 {
                    failures.push(format!("root={scan_root:?}, exclude={exclude:?}, follow={follow}: {} entries, {report:?}", index.len()));
                }
            }
        }
    }
    // A followed child alias reaches the long target, then a second alias
    // loops back to it. Long-path identities must also bound that cycle.
    let scan_root = f.path.join("scan");
    std::fs::create_dir(&scan_root).unwrap();
    f.link(&verbatim, &scan_root.join("alias"));
    f.link(&verbatim, &verbatim.join("loop"));
    for (scan_root, expected) in [(&scan_root, 4), (&root, 3), (&verbatim, 3)] {
        let (index, report) = scan(&WalkOptions {
            roots: vec![scan_root.clone()],
            excludes: vec![],
            follow_symlinks: true,
        });
        if index.len() != expected
            || report.errors != 1
            || !report
                .error_samples
                .iter()
                .any(|s| s.contains("symlink cycle"))
        {
            failures.push(format!(
                "long-path cycle at {scan_root:?}: {} entries, {report:?}",
                index.len()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
