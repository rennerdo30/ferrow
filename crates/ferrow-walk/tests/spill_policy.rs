//! A scan read that falls back to the heap is counted
//! (`ferrow_index::store::scratch_in_heap`, which the service warns about),
//! and one that must not fall back fails with `BUILD_FAILED`. One test in its
//! own process: the counter is global, and a parallel test would move it.

use ferrow_index::store::scratch_in_heap;
use ferrow_walk::BUILD_FAILED;
use ferrow_walk::ntfs::{SpillPolicy, VolumeBuilder};

#[test]
fn heap_fallbacks_of_a_scan_read_are_counted_and_large_ones_fail() {
    let dir = std::env::temp_dir().join(format!("ferrow-walk-policy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing");
    let records = 5_000;
    let policy = |heap_fallback_records, free_bytes| SpillPolicy {
        heap_fallback_records,
        free_bytes,
    };

    // Files made: nothing counted.
    let start = scratch_in_heap();
    let built = VolumeBuilder::with_spill(records, &dir, &policy(10_000, None)).unwrap();
    assert!(built.file_bytes() > 0 && built.note().is_none());
    assert_eq!(scratch_in_heap(), start);
    drop(built);

    // A small volume with no room: the heap, counted.
    let built = VolumeBuilder::with_spill(records, &dir, &policy(10_000, Some(10))).unwrap();
    assert_eq!(built.file_bytes(), 0);
    assert!(scratch_in_heap() > start, "no room is a counted fallback");
    drop(built);

    // A small volume whose directory is missing: the heap, counted again
    // (its record table and its two names files).
    let before = scratch_in_heap();
    let built = VolumeBuilder::with_spill(records, &missing, &policy(10_000, None)).unwrap();
    assert_eq!(built.file_bytes(), 0);
    assert!(scratch_in_heap() >= before + 3, "table and names files");
    drop(built);

    // A large volume never counts: it fails, with the marker the scan turns
    // into a fatal error the service retries.
    let before = scratch_in_heap();
    for (dir, free) in [(&dir, Some(10)), (&missing, None)] {
        let Err(error) = VolumeBuilder::with_spill(records, dir, &policy(100, free)) else {
            panic!("a large volume must not be read into the heap");
        };
        assert!(error.starts_with(BUILD_FAILED), "{error}");
    }
    assert_eq!(scratch_in_heap(), before, "nothing went to the heap");
    let _ = std::fs::remove_dir_all(&dir);
}
