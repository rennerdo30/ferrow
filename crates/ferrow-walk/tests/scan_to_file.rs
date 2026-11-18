//! A scan built straight into its index file is the scan the heap builds.

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;

use ferrow_index::{ColumnSet, FileTarget, Index};
use ferrow_walk::{NtfsMode, WalkOptions, scan, scan_with_maps_to_file};
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

fn tree(fx: &Fixture) -> WalkOptions {
    let root = fx.path.join("tree");
    for d in 0..6 {
        let dir = root.join(format!("dir{d}/inner"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..40 {
            std::fs::write(dir.join(format!("file-{f}.txt")), vec![b'x'; f * 7]).unwrap();
        }
    }
    std::fs::write(root.join("Ünïcødé 文書.txt"), b"u").unwrap();
    WalkOptions {
        roots: vec![root],
        excludes: Vec::new(),
        columns: ColumnSet::DEFAULT,
        follow_symlinks: false,
        ntfs: NtfsMode::Off,
    }
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ferrow-"))
        .collect()
}

#[test]
fn a_scan_into_a_file_indexes_what_the_heap_scan_does() {
    let fx = Fixture::new("scan-to-file");
    let options = tree(&fx);
    let out = fx.path.join("state");
    std::fs::create_dir(&out).unwrap();
    let path = out.join("index.bin");

    let (heap, heap_report) = scan(&options);
    let (index, report, _) = scan_with_maps_to_file(
        &options,
        &FileTarget {
            path: &path,
            lock: None,
        },
    );
    assert!(report.fatal_error.is_none());
    assert!(report.index_file.error.is_none(), "{:?}", report.index_file);
    let timings = report.index_file.timings.expect("built into the file");
    assert_eq!(timings.entries, heap.len());
    assert_eq!(report.entries, heap_report.entries);
    assert!(index.is_mapped(), "the scan's result reads its file");
    assert_eq!(entries(&index), entries(&heap));
    // The file is the one a later start loads.
    assert_eq!(entries(&Index::load(&path).unwrap()), entries(&heap));
    assert!(leftovers(&out).is_empty());
    drop(fx);
}

#[test]
fn a_scan_that_cannot_write_its_file_builds_the_index_in_the_heap() {
    let fx = Fixture::new("scan-to-file-fail");
    let options = tree(&fx);
    let (heap, _) = scan(&options);
    // No directory to write the file or spill into.
    let path = fx.path.join("missing").join("index.bin");
    let (index, report, _) = scan_with_maps_to_file(
        &options,
        &FileTarget {
            path: &path,
            lock: None,
        },
    );
    assert!(report.fatal_error.is_none());
    assert!(report.index_file.error.is_some());
    assert!(report.index_file.timings.is_none());
    assert!(!index.is_mapped(), "a heap index: correct, only larger");
    assert_eq!(entries(&index), entries(&heap));
    assert!(!path.exists());
    drop(fx);
}
