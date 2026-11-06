//! End-to-end CLI test: index a temporary tree, then search it in every
//! output mode.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestTree {
    root: PathBuf,
    index: PathBuf,
}

impl TestTree {
    fn new() -> TestTree {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "ferrow-cli-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("tree");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for name in ["Report 2024.txt", "report draft.txt", "Budget.xlsx"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        std::fs::write(root.join("sub").join("deep report.log"), b"x").unwrap();
        TestTree {
            index: base.join("index.bin"),
            root,
        }
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn ferrow(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferrow"))
        .args(args)
        .output()
        .expect("run ferrow")
}

#[test]
fn configurable_columns_sorting_json_and_missing_sort() {
    let tree = TestTree::new();
    let b = ferrow_index::IndexBuilder::with_columns(ferrow_index::ColumnSet::DEFAULT);
    for (name, size, modified) in [(b"a".as_slice(), 3, 10), (b"b", 1, 30), (b"c", 2, 20)] {
        b.add_with_metadata(
            ferrow_index::ROOT_PARENT,
            name,
            0,
            ferrow_index::EntryMetadata {
                size: Some(size),
                modified: Some(modified),
                ..Default::default()
            },
        );
    }
    b.freeze().save(&tree.index).unwrap();
    let path = tree.index.to_str().unwrap();
    let out = ferrow(&[
        "search",
        "",
        "--index",
        path,
        "--sort",
        "modified",
        "--desc",
        "--columns",
        "path,size,modified",
    ]);
    assert_eq!(stdout(&out), "b\t1\t30\nc\t2\t20\na\t3\t10\n");
    let out = ferrow(&["search", "", "--index", path, "--sort", "size", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(rows[0]["path"], "b");
    assert_eq!(rows[0]["size"], 1);
    assert_eq!(rows[0]["modified"], 30);
    assert!(rows[0]["created"].is_null());
    let absent = ferrow(&["search", "", "--index", path, "--sort", "created"]);
    assert!(!absent.status.success());
    assert!(String::from_utf8_lossy(&absent.stderr).contains("rebuild"));
    let out = ferrow(&[
        "index",
        tree.root.to_str().unwrap(),
        "--out",
        path,
        "--index-columns",
        "none",
    ]);
    stdout(&out);
    assert_eq!(
        ferrow_index::Index::load(&tree.index)
            .unwrap()
            .columns_present(),
        ferrow_index::ColumnSet::NONE
    );
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[cfg(windows)]
#[test]
fn cli_accepts_non_unicode_root_argument() {
    use std::os::windows::ffi::OsStringExt;
    static NEXT: AtomicUsize = AtomicUsize::new(100);
    let base = std::env::temp_dir().join(format!(
        "ferrow-cli-nonunicode-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join(std::ffi::OsString::from_wide(&[b'r' as u16, 0xd800]));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("needle.txt"), b"x").unwrap();
    let index = base.join("index.bin");
    let out = Command::new(env!("CARGO_BIN_EXE_ferrow"))
        .arg("index")
        .arg(&root)
        .arg("--out")
        .arg(&index)
        .output()
        .expect("run ferrow");
    assert!(
        out.status.success(),
        "exit={:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_ferrow"))
        .args(["search", "needle", "--index"])
        .arg(&index)
        .arg("--count")
        .output()
        .expect("run ferrow");
    assert_eq!(stdout(&out).trim(), "1");
    let _ = std::fs::remove_dir_all(&base);
}

// APFS rejects non-UTF-8 names outright, so on macOS this is a real
// ignored test in CI output, not a silent pass. Everywhere else a
// creation error fails the test.
#[cfg(unix)]
#[cfg_attr(target_os = "macos", ignore = "APFS rejects non-UTF-8 file names")]
#[test]
fn cli_accepts_non_unicode_root_argument() {
    use std::os::unix::ffi::OsStringExt;
    static NEXT: AtomicUsize = AtomicUsize::new(100);
    let base = std::env::temp_dir().join(format!(
        "ferrow-cli-nonunicode-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join(std::ffi::OsString::from_vec(vec![b'r', 0xff, b'x']));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("needle.txt"), b"x").unwrap();
    let index = base.join("index.bin");
    let out = Command::new(env!("CARGO_BIN_EXE_ferrow"))
        .arg("index")
        .arg(&root)
        .arg("--out")
        .arg(&index)
        .output()
        .expect("run ferrow");
    assert!(
        out.status.success(),
        "exit={:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_ferrow"))
        .args(["search", "needle", "--index"])
        .arg(&index)
        .arg("--count")
        .output()
        .expect("run ferrow");
    assert_eq!(stdout(&out).trim(), "1");
    let _ = std::fs::remove_dir_all(&base);
}

/// JSON escaping and NUL output must reproduce tricky names exactly.
/// Parsed with serde_json; expected values are the real paths.
#[test]
fn json_and_nul_escape_tricky_names_exactly() {
    static NEXT: AtomicUsize = AtomicUsize::new(200);
    let base = std::env::temp_dir().join(format!(
        "ferrow-cli-escape-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("tree");
    std::fs::create_dir_all(&root).unwrap();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut names: Vec<String> = vec!["O'Brien & [x].txt".to_string(), "Grüße ß.txt".to_string()];
    #[cfg(unix)]
    names.push("Quote \"Q\" back\\slash.txt".to_string());
    for name in &names {
        std::fs::write(root.join(name), b"x").unwrap();
    }
    let index = base.join("index.bin");
    let out = ferrow(&[
        "index",
        root.to_str().unwrap(),
        "--out",
        index.to_str().unwrap(),
    ]);
    stdout(&out);

    let out = ferrow(&[
        "search",
        "txt",
        "--index",
        index.to_str().unwrap(),
        "--json",
    ]);
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&out)).expect("valid JSON array");
    let parsed: Vec<String> = rows
        .iter()
        .map(|row| row["path"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        rows.iter()
            .all(|row| row["size"] == 1 && row["modified"].is_i64() && row["created"].is_null())
    );
    let mut expected: Vec<String> = names
        .iter()
        .map(|n| root.join(n).to_string_lossy().into_owned())
        .collect();
    let mut sorted_parsed = parsed.clone();
    sorted_parsed.sort();
    expected.sort();
    assert_eq!(sorted_parsed, expected);

    let out = ferrow(&[
        "search",
        "txt",
        "--index",
        index.to_str().unwrap(),
        "--null",
    ]);
    assert!(out.status.success());
    // Byte-for-byte: exactly the JSON output's paths (same search, same
    // order), each followed by one NUL. Extra or missing delimiters fail.
    let mut expected_nul: Vec<u8> = Vec::new();
    for path in &parsed {
        expected_nul.extend_from_slice(path.as_bytes());
        expected_nul.push(0);
    }
    assert_eq!(out.stdout, expected_nul);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn index_then_search_all_modes() {
    let tree = TestTree::new();
    let root = tree.root.to_str().unwrap();
    let index = tree.index.to_str().unwrap();

    let out = ferrow(&["index", root, "--out", index]);
    let text = stdout(&out);
    assert!(text.contains("parallel read_dir"), "{text}");
    assert!(text.contains("entries"), "{text}");

    // Plain search: all three report hits, full paths, one per line.
    let out = ferrow(&["search", "report", "--index", index]);
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    let plain_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines.iter().all(|l| l.starts_with(root)), "{text}");
    assert!(text.contains("Report 2024.txt"));
    assert!(text.contains("deep report.log"));

    // AND with a second term narrows to one hit.
    let out = ferrow(&["search", "report 2024", "--index", index]);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("Report 2024.txt"));

    // JSON output, parsed with a real JSON parser.
    let out = ferrow(&["search", "budget", "--index", index, "--json"]);
    let text = stdout(&out);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text).expect("valid JSON array");
    let parsed: Vec<String> = rows
        .iter()
        .map(|row| row["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        parsed,
        vec![tree.root.join("Budget.xlsx").to_string_lossy().into_owned()]
    );

    // NUL-separated output, byte-for-byte: exactly the plain output's
    // paths (same search, same order), each followed by one NUL. Extra
    // or missing delimiters fail.
    let out = ferrow(&["search", "report", "--index", index, "--null"]);
    assert!(out.status.success());
    let mut expected_nul: Vec<u8> = Vec::new();
    for line in &plain_lines {
        expected_nul.extend_from_slice(line.as_bytes());
        expected_nul.push(0);
    }
    assert_eq!(out.stdout, expected_nul);

    // Count only.
    let out = ferrow(&["search", "report", "--index", index, "--count"]);
    assert_eq!(stdout(&out).trim(), "3");

    // Limit.
    let out = ferrow(&["search", "report", "--index", index, "--limit", "1"]);
    assert_eq!(stdout(&out).lines().count(), 1);

    // Stats.
    let out = ferrow(&["stats", "--index", index]);
    let text = stdout(&out);
    assert!(text.contains("entries: 6"), "{text}"); // root + sub + 4 files
    assert!(text.contains("bytes/entry"), "{text}");
    assert!(text.contains("load time"), "{text}");

    // Bad query and missing index fail cleanly (non-zero, message on stderr).
    let out = ferrow(&["search", "\"unclosed", "--index", index]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unclosed quote"));

    let out = ferrow(&["search", "x", "--index", "ferrow-no-such-index.bin"]);
    assert!(!out.status.success());

    // Exclude a subtree on a fresh index.
    let index2 = tree.root.parent().unwrap().join("index2.bin");
    let sub = tree.root.join("sub");
    let out = ferrow(&[
        "index",
        root,
        "--exclude",
        sub.to_str().unwrap(),
        "--out",
        index2.to_str().unwrap(),
    ]);
    stdout(&out);
    let out = ferrow(&[
        "search",
        "report",
        "--index",
        index2.to_str().unwrap(),
        "--count",
    ]);
    assert_eq!(stdout(&out).trim(), "2");
}
