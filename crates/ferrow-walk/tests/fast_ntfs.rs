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
use std::path::Path;

use ferrow_index::{ColumnSet, FileTarget, Index};
use ferrow_walk::{NtfsMode, WalkOptions, scan, scan_with_maps, scan_with_maps_to_file};
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

/// A tree with nested folders, a big file, a hard link, a hidden file, a
/// junction, a Unicode name and two excludes (a folder and a file).
fn populate(fx: &mut Fixture) {
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
}

fn options(root: &Path, ntfs: NtfsMode) -> WalkOptions {
    WalkOptions {
        roots: vec![root.to_path_buf()],
        excludes: vec![root.join("excluded"), root.join("skip-me.txt")],
        columns: ColumnSet::DEFAULT,
        follow_symlinks: false,
        ntfs,
    }
}

#[test]
fn fast_ntfs_matches_the_walk() {
    let mut fx = Fixture::new("fast-ntfs");
    let root = fx.path.clone();
    populate(&mut fx);

    let (walked, walk_report) = scan(&options(&root, NtfsMode::Off));
    let (fast, fast_report, maps) = scan_with_maps(&options(&root, NtfsMode::AllRoots));
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
                .map(|(r, _)| r)
                .unwrap()
        };
        assert_eq!(record_of("report.txt"), record_of("report-link.txt"));
        assert_ne!(record_of("report.txt"), record_of("tiny.bin"));
        assert_eq!(map.excluded.len(), 2);
        // The heap read keeps nothing in temporary files.
        assert_eq!(fast_report.index_file.read_file_bytes, 0);
        assert_eq!(map.file_bytes(), 0);
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

/// The scan the service makes (ADR 0007 steps 8 and 9): the MFT read keeps its
/// tables and the record map in temporary files beside the index, and the
/// result is the heap scan's entries and record map.
#[test]
fn fast_ntfs_into_a_file_matches_the_heap_scan() {
    let mut fx = Fixture::new("fast-ntfs-file");
    let root = fx.path.clone();
    populate(&mut fx);
    let state = std::env::temp_dir().join(format!("ferrow-fast-ntfs-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let path = state.join("index.bin");

    let (heap, heap_report, heap_maps) = scan_with_maps(&options(&root, NtfsMode::AllRoots));
    let (index, report, maps) = scan_with_maps_to_file(
        &options(&root, NtfsMode::AllRoots),
        &FileTarget {
            path: &path,
            lock: None,
        },
    );
    assert_eq!(report.errors, 0, "{report:?}");
    assert!(report.fatal_error.is_none());
    assert!(report.index_file.error.is_none(), "{:?}", report.index_file);
    eprintln!("file scan: {}", report.mode);
    assert_eq!(entries(&index), entries(&heap));
    assert_eq!(report.entries, heap_report.entries);
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        assert!(report.mode.starts_with("mft "), "{}", report.mode);
        assert!(
            report.mode.contains("tables in temporary files"),
            "the read used files: {}",
            report.mode
        );
        assert!(report.index_file.read_file_bytes > 0);
        assert_eq!(report.index_file.read_spill_failure, None);
        assert_eq!((maps.len(), heap_maps.len()), (1, 1));
        let (map, heap_map) = (&maps[0], &heap_maps[0]);
        assert!(map.file_bytes() > 0, "the record map is in a file");
        // Same pairs in the same order as the heap scan's (the ids follow the
        // emission order, which the file-backed tables do not change).
        assert_eq!(map.entries, heap_map.entries);
        assert_eq!(map.roots, heap_map.roots);
        assert_eq!(map.excluded, heap_map.excluded);
        assert_eq!(map.serial, heap_map.serial);
        assert!(map.journal.is_some());
    } else {
        assert!(maps.is_empty());
    }
    drop((index, maps));
    let left: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ferrow-"))
        .collect();
    assert!(left.is_empty(), "temporary files left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&state);
}
