//! The NTFS fast path must index exactly what the directory walk does.
//!
//! Reading the MFT needs administrator rights, so locally (not elevated)
//! the fast path falls back to the walk and this test only checks the
//! fallback. GitHub's Windows runners are elevated: there the fast path
//! must be taken and match the walk entry for entry.
#![cfg(windows)]

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;

use ferrow_index::{ColumnSet, Index};
use ferrow_walk::{NtfsMode, WalkOptions, scan, scan_with_maps};
use support::Fixture;

fn entries(index: &Index) -> BTreeMap<String, (u16, Option<u64>, Option<i64>)> {
    (0..index.len() as u32)
        .map(|id| {
            let md = index.metadata(id);
            (
                index.path(id).to_string_lossy().into_owned(),
                (index.flags(id), md.size, md.modified),
            )
        })
        .collect()
}

#[test]
fn fast_ntfs_matches_the_walk() {
    let mut fx = Fixture::new("fast-ntfs");
    let root = fx.path.clone();
    std::fs::create_dir_all(root.join("Docs/Deep/Deeper")).unwrap();
    std::fs::create_dir_all(root.join("excluded/inner")).unwrap();
    std::fs::write(root.join("Docs/report.txt"), vec![b'x'; 5000]).unwrap();
    std::fs::write(root.join("Docs/Deep/Deeper/tiny.bin"), b"1").unwrap();
    std::fs::write(root.join("excluded/inner/secret.txt"), b"s").unwrap();
    std::fs::write(root.join("Docs/Ünïcødé 文書.txt"), b"u").unwrap();
    std::fs::write(root.join("big.dat"), vec![0u8; 3 << 20]).unwrap();
    std::fs::write(root.join("skip-me.txt"), b"x").unwrap();
    std::fs::hard_link(root.join("Docs/report.txt"), root.join("report-link.txt")).unwrap();
    let hidden = root.join("hidden.txt");
    std::fs::write(&hidden, b"h").unwrap();
    let status = std::process::Command::new("attrib")
        .arg("+h")
        .arg(&hidden)
        .status()
        .unwrap();
    assert!(status.success());
    fx.junction(&root.join("Docs"), &root.join("docs-junction"));

    let options = |ntfs| WalkOptions {
        roots: vec![root.clone()],
        excludes: vec![root.join("excluded"), root.join("skip-me.txt")],
        columns: ColumnSet::DEFAULT,
        follow_symlinks: false,
        ntfs,
    };
    let (walked, walk_report) = scan(&options(NtfsMode::Off));
    let (fast, fast_report, maps) = scan_with_maps(&options(NtfsMode::AllRoots));
    assert_eq!(walk_report.errors, 0, "{walk_report:?}");
    assert_eq!(fast_report.errors, 0, "{fast_report:?}");
    eprintln!("fast path: {}", fast_report.mode);
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        assert!(
            fast_report.mode.starts_with("mft "),
            "the elevated CI runner must take the MFT path: {}",
            fast_report.mode
        );
        // Every entry is mapped to its MFT record; both hard links share one.
        assert_eq!(maps.len(), 1);
        let map = &maps[0];
        assert_eq!(map.entries.len(), fast.len());
        assert_eq!(map.roots.len(), 1);
        assert!(
            map.journal.is_some(),
            "NTFS system volumes keep a change journal"
        );
        let record_of = |name: &str| {
            map.entries
                .iter()
                .find(|(_, id)| fast.path(*id).ends_with(name))
                .map(|(r, _)| *r)
                .unwrap()
        };
        assert_eq!(record_of("report.txt"), record_of("report-link.txt"));
        assert_ne!(record_of("report.txt"), record_of("tiny.bin"));
        assert_eq!(map.excluded.len(), 2);
    } else {
        assert!(maps.is_empty());
    }
    let walked = entries(&walked);
    let fast = entries(&fast);
    assert!(walked.keys().any(|p| p.ends_with("tiny.bin")));
    assert!(
        !walked
            .keys()
            .any(|p| p.contains("secret") || p.ends_with("skip-me.txt"))
    );
    assert_eq!(fast, walked);
}
