//! CI-only alternative: persist compact ids and validate instead of sorting.
use ferrow_index::{Index, SortKey};
use std::path::Path;

pub fn load_order() -> impl Iterator<Item = (bool, usize)> {
    (0..3)
        .map(|mode| (true, mode))
        .chain((0..3).flat_map(|run| (0..3).map(move |slot| (false, (run + slot) % 3))))
}

fn width(n: usize) -> usize {
    if n <= 1 << 23 {
        3
    } else if n <= 1 << 31 {
        4
    } else {
        5
    }
}

pub fn encode(index: &Index) -> Vec<u8> {
    let n = index.len();
    let width = width(n);
    let flag = 1u64 << (width * 8 - 1);
    let mut bytes = Vec::with_capacity(2 * n * width);
    for key in [SortKey::Size, SortKey::Modified] {
        let order = index.metadata_order(key, false).expect("prepared cache");
        let mut previous = None;
        for (rank, id) in order.range(0, n).enumerate() {
            let value = index.sort_value_for_benchmark(key, id);
            let code = u64::from(id)
                | if rank == 0 || value != previous {
                    flag
                } else {
                    0
                };
            bytes.extend_from_slice(&code.to_le_bytes()[..width]);
            previous = value;
        }
    }
    bytes
}

pub fn validate(index: &Index, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    validate_bytes(index, &std::fs::read(path)?)
}

fn validate_bytes(index: &Index, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let n = index.len();
    let width = width(n);
    if bytes.len() != 2 * n * width {
        return Err("invalid persisted cache length".into());
    }
    let flag = 1u64 << (width * 8 - 1);
    let mut rank = vec![0u32; n];
    for (position, &id) in index.sorted().iter().enumerate() {
        rank[id as usize] = position as u32;
    }
    for (column, key) in [SortKey::Size, SortKey::Modified].into_iter().enumerate() {
        let mut seen = vec![false; n];
        let mut previous = None;
        let mut previous_id = 0;
        for (position, raw) in bytes[column * n * width..(column + 1) * n * width]
            .chunks_exact(width)
            .enumerate()
        {
            let mut encoded = [0; 8];
            encoded[..width].copy_from_slice(raw);
            let code = u64::from_le_bytes(encoded);
            let id = code & !flag;
            if id >= n as u64 || seen[id as usize] {
                return Err("invalid persisted cache permutation".into());
            }
            let id = id as u32;
            seen[id as usize] = true;
            let value = index.sort_value_for_benchmark(key, id);
            if (code & flag != 0) != (position == 0 || value != previous) {
                return Err("invalid persisted group boundary".into());
            }
            if position > 0 {
                let comparison = match (previous, value) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                };
                if comparison.is_gt()
                    || (comparison.is_eq() && rank[previous_id as usize] >= rank[id as usize])
                {
                    return Err("invalid persisted cache order".into());
                }
            }
            previous = value;
            previous_id = id;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strategy_samples_balance_each_arm_in_each_position_after_warmup() {
        let order: Vec<_> = load_order().collect();
        assert_eq!(&order[..3], &[(true, 0), (true, 1), (true, 2)]);
        let samples = &order[3..];
        assert!(samples.iter().all(|s| !s.0));
        for row in samples.chunks_exact(3) {
            let mut modes: Vec<_> = row.iter().map(|s| s.1).collect();
            modes.sort();
            assert_eq!(modes, vec![0, 1, 2]);
        }
        for column in 0..3 {
            let mut modes: Vec<_> = samples
                .iter()
                .skip(column)
                .step_by(3)
                .map(|s| s.1)
                .collect();
            modes.sort();
            assert_eq!(modes, vec![0, 1, 2]);
        }
    }
    #[test]
    fn prototype_validates_values_ties_permutation_and_group_flags() {
        let b = ferrow_index::IndexBuilder::with_columns(ferrow_index::ColumnSet::DEFAULT);
        for (i, name) in [b"d", b"C", b"b", b"A"].into_iter().enumerate() {
            b.add_with_metadata(
                ferrow_index::ROOT_PARENT,
                name,
                0,
                ferrow_index::EntryMetadata {
                    size: Some((i / 2) as u64),
                    modified: Some((i / 2) as i64 * 10_000_000),
                    ..Default::default()
                },
            );
        }
        let index = b.freeze();
        for key in [SortKey::Size, SortKey::Modified] {
            index.request_sort(key);
            assert_eq!(
                index
                    .sort_ticket(key)
                    .unwrap()
                    .wait_timeout(std::time::Duration::from_secs(2)),
                ferrow_index::SortStatus::Ready
            );
        }
        let bytes = encode(&index);
        validate_bytes(&index, &bytes).unwrap();
        let mut duplicate = bytes.clone();
        duplicate.copy_within(0..3, 3);
        assert!(validate_bytes(&index, &duplicate).is_err());
        let mut tied = bytes.clone();
        for i in 0..3 {
            tied.swap(i, i + 3);
        }
        tied[2] |= 128;
        tied[5] &= 127;
        assert!(
            validate_bytes(&index, &tied)
                .unwrap_err()
                .to_string()
                .contains("order")
        );
        let mut flag = bytes.clone();
        flag[2] ^= 128;
        assert!(validate_bytes(&index, &flag).is_err());
        let mut invalid = bytes.clone();
        invalid[0] = 255;
        assert!(validate_bytes(&index, &invalid).is_err());
        assert!(validate_bytes(&index, &bytes[..bytes.len() - 1]).is_err());
    }
}
