//! Bounded builds: the file-backed freeze and the spilling shards (ADR 0007).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::tests::{Spec, assert_matches, awkward_specs, build};
use crate::{ColumnSet, EntryMetadata, FileTarget, Index, IndexBuilder, ROOT_PARENT, flags};

fn scratch(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ferrow-bounded-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// File names in `dir`, sorted, but for the spill files of a live builder
/// (Windows lists a delete-on-close file while it is open; see `leftovers`).
fn listing(dir: &Path) -> Vec<String> {
    let mut names = all_names(dir);
    names.retain(|n| !n.starts_with(".ferrow-spill-"));
    names
}

fn all_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Temporary files of any kind in `dir`: none once every builder is gone.
fn leftovers(dir: &Path) -> Vec<String> {
    let mut names = all_names(dir);
    names.retain(|n| n.starts_with(".ferrow-"));
    names
}

fn fill(b: &IndexBuilder, specs: &[Spec]) {
    for (i, s) in specs.iter().enumerate() {
        let id = b.add_with_metadata(s.parent, &s.name, s.flags, s.meta);
        assert_eq!(id as usize, i, "ids are handed out in order");
    }
}

/// What `Index::save` writes for a heap build of `specs`.
fn saved_bytes(specs: &[Spec], set: ColumnSet, dir: &Path) -> Vec<u8> {
    let path = dir.join("expected.bin");
    build(specs, set, None).save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    bytes
}

const SETS: [ColumnSet; 4] = [
    ColumnSet::NONE,
    ColumnSet::DEFAULT,
    ColumnSet::ALL,
    ColumnSet::ATTRIBUTES,
];

/// The arena length is odd for some counts and even for others: the padding
/// before `sorted` is 2 bytes or none.
fn arena_len(specs: &[Spec]) -> usize {
    specs.iter().map(|s| s.name.len().min(65_535)).sum()
}

#[test]
fn a_file_freeze_writes_the_bytes_save_writes_for_every_column_set() {
    let dir = scratch("equal");
    let mut pads = std::collections::HashSet::new();
    for count in [1usize, 2, 3, 700, 701, 4_000, 4_001] {
        let specs = awkward_specs(count);
        pads.insert(arena_len(&specs) % 2);
        for set in SETS {
            let want = saved_bytes(&specs, set, &dir);
            let path = dir.join("index.bin");
            let b = IndexBuilder::with_columns(set);
            fill(&b, &specs);
            let index = b.freeze_to_file(&path).unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                want,
                "{count} entries, columns {}",
                set.bits()
            );
            assert!(index.is_mapped(), "the result reads the file");
            assert!(
                index.heap_bytes() - index.metadata_heap_bytes() < 1024,
                "no column is in the heap"
            );
            assert_matches(&index, &specs, set);
            // Loading the file validates everything the build skipped.
            assert_matches(&Index::load(&path).unwrap(), &specs, set);
        }
    }
    assert_eq!(pads.len(), 2, "both an odd and an even arena are covered");
    assert_eq!(
        listing(&dir),
        ["index.bin"],
        "no temporary file is left beside the index"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spilled_shards_build_the_same_file_and_the_same_heap_index() {
    let dir = scratch("spill");
    let specs = awkward_specs(6_000);
    for set in SETS {
        let want = saved_bytes(&specs, set, &dir);
        // A budget far below the data: every shard spills many runs.
        let b = IndexBuilder::with_spill(set, &dir, 8 * 1024);
        b.reserve(specs.len(), arena_len(&specs));
        fill(&b, &specs);
        assert!(b.spilled_bytes() > 0, "the shards spilled");
        assert!(b.spill_failure().is_none());
        let path = dir.join("index.bin");
        let (index, timings) = b
            .freeze_to_file_with(&FileTarget {
                path: &path,
                lock: None,
            })
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            want,
            "columns {}",
            set.bits()
        );
        assert_eq!(timings.entries, specs.len());
        assert_eq!(timings.file_bytes, want.len() as u64);
        assert!(timings.spilled_bytes > 0);
        assert_matches(&index, &specs, set);
        drop(index);
        std::fs::remove_file(&path).unwrap();

        // The same builder frozen into the heap reads every run back.
        let b = IndexBuilder::with_spill(set, &dir, 8 * 1024);
        fill(&b, &specs);
        assert!(b.spilled_bytes() > 0);
        let (heap, _) = b.freeze_checked().unwrap();
        assert!(!heap.is_mapped());
        assert_matches(&heap, &specs, set);
    }
    assert!(
        leftovers(&dir).is_empty(),
        "no spill file survives its builder"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_spilling_builder_holds_a_bounded_amount_while_it_is_fed() {
    let dir = scratch("bounded");
    let specs = awkward_specs(20_000);
    let budget = 64 * 1024;
    let b = IndexBuilder::with_spill(ColumnSet::DEFAULT, &dir, budget);
    let mut worst = 0usize;
    for (i, s) in specs.iter().enumerate() {
        b.add_with_metadata(s.parent, &s.name, s.flags, s.meta);
        if i % 997 == 0 {
            let held: usize = b.memory_parts().iter().map(|(_, bytes)| bytes).sum();
            worst = worst.max(held);
        }
    }
    let total = specs.len() * 21 + arena_len(&specs);
    assert!(b.spilled_bytes() as usize > total / 2, "most of it spilled");
    // Capacity, not length: doubling can leave a shard up to twice its share,
    // and a few shards wait to be reused. Far below the data either way.
    assert!(
        worst < 8 * budget,
        "peak {worst} bytes held for {total} bytes of entries"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn entries_added_from_many_threads_to_a_spilling_builder_land_at_their_ids() {
    let dir = scratch("threads");
    let b = IndexBuilder::with_spill(ColumnSet::ALL, &dir, 16 * 1024);
    let root = b.add_root(b"r", flags::DIR);
    let mut all: Vec<(u32, Spec)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|t| {
                let b = &b;
                s.spawn(move || {
                    (0..900)
                        .map(|i| {
                            let spec = Spec {
                                parent: root,
                                name: format!("t{t}-{i}-Ä").into_bytes(),
                                flags: 0,
                                meta: EntryMetadata {
                                    size: Some((t * 1000 + i) as u64),
                                    modified: Some(((t * 1000 + i) as i64) * 10_000_000),
                                    created: None,
                                    accessed: (i % 2 == 0).then_some(i64::MAX),
                                    attributes: Some(i as u32),
                                },
                            };
                            let id =
                                b.add_with_metadata(spec.parent, &spec.name, spec.flags, spec.meta);
                            (id, spec)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(b.spilled_bytes() > 0);
    let path = dir.join("index.bin");
    let index = b.freeze_to_file(&path).unwrap();
    all.sort_by_key(|(id, _)| *id);
    let mut specs = vec![Spec {
        parent: ROOT_PARENT,
        name: b"r".to_vec(),
        flags: flags::DIR,
        meta: EntryMetadata::default(),
    }];
    for (i, (id, spec)) in all.into_iter().enumerate() {
        assert_eq!(id as usize, i + 1, "ids are dense");
        specs.push(spec);
    }
    assert_matches(&index, &specs, ColumnSet::ALL);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        saved_bytes(&specs, ColumnSet::ALL, &dir)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_file_freeze_leaves_the_destination_and_the_directory_alone() {
    let dir = scratch("fail");
    let specs = awkward_specs(2_500);
    let path = dir.join("index.bin");
    std::fs::write(&path, b"the previous index").unwrap();

    // Interrupted after the data is placed, before the file replaces the
    // destination: with spilled runs, so their files are cleaned up too.
    let b = IndexBuilder::with_spill(ColumnSet::ALL, &dir, 8 * 1024);
    fill(&b, &specs);
    assert!(b.spilled_bytes() > 0);
    b.fail_before_replace.store(true, Ordering::Relaxed);
    let failed = b
        .freeze_to_file_with(&FileTarget {
            path: &path,
            lock: None,
        })
        .unwrap_err();
    assert!(failed.to_string().contains("interrupted"));
    assert_eq!(std::fs::read(&path).unwrap(), b"the previous index");
    assert_eq!(listing(&dir), ["index.bin"], "no temporary index is left");

    // The builder comes back intact: the heap freeze of it is correct.
    failed
        .builder
        .fail_before_replace
        .store(false, Ordering::Relaxed);
    let (heap, _) = failed.builder.freeze_checked().unwrap();
    assert_matches(&heap, &specs, ColumnSet::ALL);
    assert!(
        leftovers(&dir).is_empty(),
        "spill files went with the builder"
    );

    // The destination cannot be replaced (a directory is there): the rename
    // fails after the whole file was written; the temporary file is removed.
    let blocked = dir.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    fill(&b, &specs);
    assert!(b.freeze_to_file(&blocked).is_err());
    assert_eq!(listing(&dir), ["blocked", "index.bin"]);
    assert!(blocked.is_dir());
    assert!(leftovers(&dir).is_empty());

    // No directory for the temporary file at all.
    let b = IndexBuilder::new();
    b.add_root(b"x", flags::DIR);
    assert!(
        b.freeze_to_file(&dir.join("missing").join("index.bin"))
            .is_err()
    );
    assert_eq!(listing(&dir), ["blocked", "index.bin"]);
    assert!(leftovers(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_freeze_replaces_a_destination_that_is_mapped() {
    let dir = scratch("replace");
    let path = dir.join("index.bin");
    let first = awkward_specs(1_500);
    let second = awkward_specs(2_200);

    let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    fill(&b, &first);
    let old = b.freeze_to_file(&path).unwrap();
    assert!(old.is_mapped());
    // The old file is read through its map while the new one replaces it,
    // under the caller's lock.
    let lock = std::sync::Mutex::new(());
    let b = IndexBuilder::with_spill(ColumnSet::DEFAULT, &dir, 8 * 1024);
    fill(&b, &second);
    let (new, _) = b
        .freeze_to_file_with(&FileTarget {
            path: &path,
            lock: Some(&lock),
        })
        .unwrap();
    assert!(lock.try_lock().is_ok(), "the lock is released");
    assert_matches(&old, &first, ColumnSet::DEFAULT);
    assert_matches(&new, &second, ColumnSet::DEFAULT);
    assert_matches(&Index::load(&path).unwrap(), &second, ColumnSet::DEFAULT);
    drop((old, new));
    assert_eq!(all_names(&dir), ["index.bin"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_spill_directory_that_cannot_be_written_keeps_the_shards_in_memory() {
    let dir = scratch("nospill");
    let specs = awkward_specs(3_000);
    let b = IndexBuilder::with_spill(ColumnSet::ALL, &dir.join("missing"), 8 * 1024);
    fill(&b, &specs);
    assert_eq!(b.spilled_bytes(), 0);
    assert!(b.spill_failure().is_some(), "the failure is reported");
    let path = dir.join("index.bin");
    let index = b.freeze_to_file(&path).unwrap();
    assert_matches(&index, &specs, ColumnSet::ALL);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        saved_bytes(&specs, ColumnSet::ALL, &dir)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_and_single_entry_builders_freeze_to_a_file() {
    let dir = scratch("tiny");
    for set in SETS {
        let path = dir.join("empty.bin");
        let empty = IndexBuilder::with_columns(set)
            .freeze_to_file(&path)
            .unwrap();
        assert!(empty.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            saved_bytes(&[], set, &dir),
            "columns {}",
            set.bits()
        );
        let one = [Spec {
            parent: ROOT_PARENT,
            name: b"x".to_vec(),
            flags: flags::DIR,
            meta: EntryMetadata::default(),
        }];
        let b = IndexBuilder::with_columns(set);
        fill(&b, &one);
        let path = dir.join("one.bin");
        assert_matches(&b.freeze_to_file(&path).unwrap(), &one, set);
        assert_eq!(std::fs::read(&path).unwrap(), saved_bytes(&one, set, &dir));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_parallel_checksum_equals_the_plain_one_over_several_chunks() {
    // More than two 8 MiB chunks and a ragged end: the combined hashers
    // must give the file's plain CRC-32.
    let data: Vec<u8> = (0..(17 << 20) + 12_345u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    assert_eq!(crate::build::crc_parallel(&data), crc32fast::hash(&data));
    assert_eq!(
        crate::build::crc_parallel(&data[..100]),
        crc32fast::hash(&data[..100])
    );
}
