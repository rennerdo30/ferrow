//! Every temporary buffer that should have been a file and went to the heap
//! is counted (`scratch_in_heap`), so the service can warn. One test in its
//! own process: the counter is global, and a parallel test would move it.

use ferrow_index::store::{Appender, DEGRADE_LIMIT, Scratch, scratch_in_heap};

#[test]
fn every_heap_fallback_of_a_temporary_buffer_is_counted() {
    let dir = std::env::temp_dir().join(format!("ferrow-counter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing");

    // Files that can be made are not counted, nor is a buffer that was never
    // meant to be a file.
    let start = scratch_in_heap();
    let file = Scratch::<u32>::zeroed(1000, Some(&dir));
    assert!(file.is_mapped());
    let _heap = Scratch::<u32>::zeroed(1000, None);
    let appender = Appender::<u32>::or_heap(&dir);
    assert_eq!(scratch_in_heap(), start);
    drop((file, appender));

    // A table that could not get its file: counted once.
    let table = Scratch::<u32>::zeroed(1000, Some(&missing));
    assert!(!table.is_mapped());
    assert_eq!(scratch_in_heap(), start + 1);
    // A strict request fails instead and is not a fallback.
    assert!(Scratch::<u32>::try_zeroed(1000, &missing).is_err());
    assert_eq!(scratch_in_heap(), start + 1);

    // An appender whose file cannot be made: counted.
    let appender = Appender::<u32>::or_heap(&missing);
    assert!(appender.fallback().is_some());
    assert_eq!(scratch_in_heap(), start + 2);

    // An appender that moves to the heap when its file fails: counted. (The
    // failure is made with a file that is too large to read back; a plain
    // write error cannot be forced here, so the in-crate tests cover it.)
    let mut appender = Appender::<u8>::new(&dir).unwrap().allow_heap(DEGRADE_LIMIT);
    appender.extend_from_slice(&[1, 2, 3]);
    let column = appender.finish().unwrap();
    assert!(column.is_mapped(), "a healthy file stays a file");
    assert_eq!(scratch_in_heap(), start + 2);
    let _ = std::fs::remove_dir_all(&dir);
}
