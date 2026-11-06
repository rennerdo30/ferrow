//! Loading must not allocate per entry (M1 spec: "one sequential read
//! with no per-entry allocation"). A counting global allocator measures
//! `persist::from_bytes` for two entry counts; the allocation counts
//! must match (a small slack covers harness noise in this binary, which
//! contains exactly one test).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwards the caller's unchanged allocation contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwards the pointer and layout to its original allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwards the caller's unchanged reallocation contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

use ferrow_index::{IndexBuilder, flags, persist};

fn saved_bytes(entries: usize) -> Vec<u8> {
    let builder = IndexBuilder::new();
    let root = builder.add_root(b"root", flags::DIR);
    for _ in 1..entries {
        builder.add(root, b"file", 0);
    }
    let index = builder.freeze();
    let path =
        std::env::temp_dir().join(format!("ferrow-alloc-{}-{entries}.bin", std::process::id()));
    index.save(&path).unwrap();
    let data = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    data
}

#[test]
fn load_allocation_count_is_independent_of_entry_count() {
    let mut counts = Vec::new();
    for entries in [100usize, 1000] {
        let data = saved_bytes(entries);
        ALLOCS.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
        let loaded = persist::from_bytes(&data);
        COUNTING.store(false, Ordering::Relaxed);
        counts.push(ALLOCS.load(Ordering::Relaxed));
        assert_eq!(loaded.unwrap().len(), entries);
    }
    assert!(
        counts[1] <= counts[0] + 2 && counts[0] < 32,
        "allocations for 100 and 1000 entries: {counts:?} (per-entry allocation on load)"
    );
}
