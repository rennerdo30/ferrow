use ferrow_index::{
    ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, SortKey, timestamp_ticks,
};
use std::time::{Duration, UNIX_EPOCH};

fn fixture(set: ColumnSet) -> Index {
    let builder = IndexBuilder::with_columns(set);
    for (name, size, ticks) in [
        (b"z".as_slice(), Some(u64::MAX), Some(i64::MIN)),
        (b"b", Some(0), Some(7)),
        (b"A", Some(0), Some(7)),
        (b"unknown", None, None),
    ] {
        builder.add_with_metadata(
            ROOT_PARENT,
            name,
            0,
            EntryMetadata {
                size,
                modified: ticks,
                created: ticks,
                accessed: ticks,
                attributes: Some(u32::MAX),
            },
        );
    }
    builder.freeze()
}

fn save_bytes(index: &Index) -> Vec<u8> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "ferrow-metadata-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    index.save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    bytes
}

fn checksum(bytes: &mut [u8]) {
    let checksum = crc32fast::hash(&bytes[24..]);
    bytes[20..24].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn optional_columns_and_both_directions_round_trip() {
    for bits in 0..=ColumnSet::ALL.bits() {
        let set = ColumnSet::from_bits(bits).unwrap();
        let index = fixture(set);
        let loaded = ferrow_index::persist::from_bytes(&save_bytes(&index)).unwrap();
        assert_eq!(loaded.columns_present(), set);
        assert_eq!(loaded.heap_bytes(), index.heap_bytes());
        for id in 0..index.len() as u32 {
            assert_eq!(loaded.metadata(id), index.metadata(id));
        }
        assert_eq!(
            index.metadata_heap_bytes(),
            (bits & 15).count_ones() as usize * 65
                + usize::from(set.contains(ColumnSet::ATTRIBUTES)) * 17
        );
        for (key, column) in [
            (SortKey::Size, ColumnSet::SIZE),
            (SortKey::Modified, ColumnSet::MODIFIED),
            (SortKey::Created, ColumnSet::CREATED),
            (SortKey::Accessed, ColumnSet::ACCESSED),
        ] {
            for desc in [false, true] {
                assert_eq!(
                    loaded.metadata_order(key, desc),
                    index.metadata_order(key, desc)
                );
                assert_eq!(
                    loaded.metadata_order(key, desc).is_some(),
                    set.contains(column)
                );
            }
        }
    }
    let index = fixture(ColumnSet::ALL);
    assert_eq!(index.metadata(0).size, Some(u64::MAX));
    assert_eq!(index.metadata(0).modified, Some(i64::MIN));
    assert_eq!(index.metadata(0).attributes, Some(u32::MAX));
    assert_eq!(
        index.metadata_order(SortKey::Size, false).unwrap(),
        &[2, 1, 0, 3]
    );
    assert_eq!(
        index.metadata_order(SortKey::Size, true).unwrap(),
        &[0, 2, 1, 3]
    );
    assert_eq!(
        index.metadata_order(SortKey::Modified, false).unwrap(),
        &[0, 2, 1, 3]
    );
    assert_eq!(
        index.metadata_order(SortKey::Modified, true).unwrap(),
        &[2, 1, 0, 3]
    );
}

#[test]
fn hostile_metadata_is_rejected_even_with_valid_checksum() {
    // One entry, one sortable column: header + core + value + bitmap + two ids.
    let b = IndexBuilder::with_columns(ColumnSet::SIZE);
    b.add_with_metadata(ROOT_PARENT, b"a", 0, EntryMetadata::default());
    let bytes = save_bytes(&b.freeze());
    for (offset, value) in [(24, 128), (46, 1), (54, 128), (55, 1), (59, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        checksum(&mut bad);
        assert!(
            ferrow_index::persist::from_bytes(&bad).is_err(),
            "accepted offset {offset}"
        );
    }
    for len in 0..bytes.len() {
        assert!(ferrow_index::persist::from_bytes(&bytes[..len]).is_err());
    }
    let bytes = save_bytes(&fixture(ColumnSet::ALL));
    let arena_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let order = 28 + 4 * 16 + 2 * arena_len + 4 * 8 + 1;
    for desc in [false, true] {
        let at = order + usize::from(desc) * 16;
        let mut duplicate = bytes.clone();
        duplicate.copy_within(at..at + 4, at + 4);
        checksum(&mut duplicate);
        assert!(ferrow_index::persist::from_bytes(&duplicate).is_err());
        let mut unsorted = bytes.clone();
        for i in 0..4 {
            unsorted.swap(at + i, at + 4 + i);
        }
        checksum(&mut unsorted);
        assert!(ferrow_index::persist::from_bytes(&unsorted).is_err());
    }
}

#[test]
fn v1_refusal_requests_rebuild_and_time_conversion_handles_pre_epoch() {
    let mut old = b"FERROWID".to_vec();
    for word in [1u32, 0, 0, 0] {
        old.extend(word.to_le_bytes());
    }
    let error = ferrow_index::persist::from_bytes(&old)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("version 1") && error.contains("rebuild"),
        "{error}"
    );
    assert_eq!(timestamp_ticks(UNIX_EPOCH), Some(0));
    assert_eq!(
        timestamp_ticks(UNIX_EPOCH + Duration::from_nanos(199)),
        Some(1)
    );
    assert_eq!(
        timestamp_ticks(UNIX_EPOCH - Duration::from_nanos(100)),
        Some(-1)
    );
    assert_eq!(
        timestamp_ticks(UNIX_EPOCH - Duration::from_secs(1)),
        Some(-10_000_000)
    );
}
