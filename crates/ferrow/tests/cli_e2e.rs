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

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
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
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines.iter().all(|l| l.starts_with(root)), "{text}");
    assert!(text.contains("Report 2024.txt"));
    assert!(text.contains("deep report.log"));

    // AND with a second term narrows to one hit.
    let out = ferrow(&["search", "report 2024", "--index", index]);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("Report 2024.txt"));

    // JSON output.
    let out = ferrow(&["search", "budget", "--index", index, "--json"]);
    let text = stdout(&out);
    assert!(
        text.starts_with('[') && text.contains("Budget.xlsx"),
        "{text}"
    );
    assert!(text.trim_end().ends_with(']'), "{text}");

    // NUL-separated output.
    let out = ferrow(&["search", "report", "--index", index, "--null"]);
    assert!(out.status.success());
    let parts: Vec<&[u8]> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    assert_eq!(parts.len(), 3);

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
