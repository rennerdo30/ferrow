use ferrow_index::{
    ColumnSet, EntryMetadata, Index, IndexBuilder, ROOT_PARENT, SortKey, timestamp_ticks,
};
use std::time::{Duration, UNIX_EPOCH};

fn order(index: &Index, key: SortKey, desc: bool) -> Option<Vec<u32>> {
    if index.request_sort(key) == ferrow_index::SortStatus::Unavailable {
        return None;
    }
    assert_eq!(
        index
            .sort_ticket(key)
            .unwrap()
            .wait_timeout(Duration::from_secs(2)),
        ferrow_index::SortStatus::Ready
    );
    index
        .metadata_order(key, desc)
        .map(|o| o.range(0, o.len()).collect())
}

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
        let fixed = IndexBuilder::with_columns(set)
            .freeze()
            .metadata_heap_bytes();
        let expected = fixed
            + usize::from(set.contains(ColumnSet::SIZE)) * 33
            + (bits & 14).count_ones() as usize * 29
            + usize::from(set.contains(ColumnSet::ATTRIBUTES)) * 17;
        assert_eq!(index.metadata_heap_bytes(), expected);
        let cold = index.heap_bytes();
        for (key, column) in [
            (SortKey::Size, ColumnSet::SIZE),
            (SortKey::Modified, ColumnSet::MODIFIED),
            (SortKey::Created, ColumnSet::CREATED),
            (SortKey::Accessed, ColumnSet::ACCESSED),
        ] {
            for desc in [false, true] {
                assert_eq!(order(&loaded, key, desc), order(&index, key, desc));
                assert_eq!(order(&loaded, key, desc).is_some(), set.contains(column));
            }
        }
        assert_eq!(
            index.heap_bytes() - cold,
            (bits & 15).count_ones() as usize * 12
        );
    }
    let index = fixture(ColumnSet::ALL);
    assert_eq!(index.metadata(0).size, Some(u64::MAX));
    assert_eq!(index.metadata(0).modified, Some(i64::MIN));
    assert_eq!(index.metadata(0).attributes, Some(u32::MAX));
    assert_eq!(order(&index, SortKey::Size, false).unwrap(), &[2, 1, 0, 3]);
    assert_eq!(order(&index, SortKey::Size, true).unwrap(), &[0, 2, 1, 3]);
    assert_eq!(
        order(&index, SortKey::Modified, false).unwrap(),
        &[0, 2, 1, 3]
    );
    assert_eq!(
        order(&index, SortKey::Modified, true).unwrap(),
        &[2, 1, 0, 3]
    );
}

#[test]
fn hostile_metadata_is_rejected_even_with_valid_checksum() {
    let b = IndexBuilder::with_columns(ColumnSet::SIZE);
    b.add_with_metadata(ROOT_PARENT, b"a", 0, EntryMetadata::default());
    let bytes = save_bytes(&b.freeze());
    for (offset, value) in [(24, 128), (28, 255), (50, 2), (54, 1), (59, 128)] {
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
    let b = IndexBuilder::with_columns(ColumnSet::SIZE);
    for name in [b"a", b"b"] {
        b.add_with_metadata(
            ROOT_PARENT,
            name,
            0,
            EntryMetadata {
                size: Some(u64::MAX),
                ..Default::default()
            },
        );
    }
    let bytes = save_bytes(&b.freeze());
    for duplicate in [false, true] {
        let mut bad = bytes.clone();
        if duplicate {
            bad.copy_within(83..87, 87);
        } else {
            bad[83..87].copy_from_slice(&2u32.to_le_bytes());
        }
        checksum(&mut bad);
        assert!(ferrow_index::persist::from_bytes(&bad).is_err());
    }
}

#[test]
fn legacy_core_loads_v2_requests_rebuild_and_time_conversion_handles_pre_epoch() {
    let mut old = b"FERROWID".to_vec();
    for word in [1u32, 0, 0, 0] {
        old.extend(word.to_le_bytes());
    }
    let core = ferrow_index::persist::from_bytes(&old).unwrap();
    assert!(core.is_empty());
    assert_eq!(core.columns_present(), ColumnSet::NONE);
    let mut bytes = save_bytes(&fixture(ColumnSet::NONE));
    bytes.drain(24..32);
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    checksum(&mut bytes);
    let core = ferrow_index::persist::from_bytes(&bytes).unwrap();
    assert_eq!(core.len(), 4);
    assert_eq!(core.columns_present(), ColumnSet::NONE);
    old[8..12].copy_from_slice(&2u32.to_le_bytes());
    let error = ferrow_index::persist::from_bytes(&old)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("version 2") && error.contains("rebuild"),
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
