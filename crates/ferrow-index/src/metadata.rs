//! Optional dense metadata columns. See ADR 0003 for range and cost policies.
use crate::{LoadError, RawEntry};
use rayon::prelude::*;
use std::cmp::Ordering;

/// Optional column presence; unknown bits are refused when loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSet(u32);
impl ColumnSet {
    pub const NONE: Self = Self(0);
    pub const SIZE: Self = Self(1);
    pub const MODIFIED: Self = Self(2);
    pub const CREATED: Self = Self(4);
    pub const ACCESSED: Self = Self(8);
    pub const ATTRIBUTES: Self = Self(16);
    pub const ALL: Self = Self(31);
    pub const DEFAULT: Self = Self(3);
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
    pub(crate) fn payload_bytes(self, n: u32) -> u64 {
        let n = n as u64;
        let sortable = (self.0 & 15).count_ones() as u64;
        let attributes = u64::from(self.contains(Self::ATTRIBUTES));
        sortable * (n * 16 + n.div_ceil(8)) + attributes * (n * 4 + n.div_ceil(8))
    }
}
impl Default for ColumnSet {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Timestamp values are signed 100 ns ticks since 1970-01-01 UTC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryMetadata {
    pub size: Option<u64>,
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub accessed: Option<i64>,
    pub attributes: Option<u32>,
}

/// Primary sort column. Equal values always use ascending name order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Created,
    Accessed,
}

#[derive(Debug, Default)]
pub(crate) struct MetadataColumns {
    size: Option<Column<u64>>,
    modified: Option<Column<i64>>,
    created: Option<Column<i64>>,
    accessed: Option<Column<i64>>,
    attributes: Option<Column<u32>>,
}

#[derive(Debug)]
struct Column<T> {
    values: Vec<T>,
    valid: Vec<u8>,
    ascending: Vec<u32>,
    descending: Vec<u32>,
}

trait Value: Copy + Default + Ord + Send + Sync {
    const WIDTH: usize;
    fn write(self, out: &mut Vec<u8>);
    fn read(bytes: &[u8]) -> Self;
}
macro_rules! value {
    ($t:ty, $n:expr) => {
        impl Value for $t {
            const WIDTH: usize = $n;
            fn write(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn read(bytes: &[u8]) -> Self {
                Self::from_le_bytes(bytes.try_into().expect("checked width"))
            }
        }
    };
}
value!(u64, 8);
value!(i64, 8);
value!(u32, 4);

impl<T: Value> Column<T> {
    fn build(raw: &[RawEntry], get: impl Fn(&EntryMetadata) -> Option<T>) -> Self {
        let mut values = Vec::with_capacity(raw.len());
        let mut valid = vec![0; raw.len().div_ceil(8)];
        for (i, e) in raw.iter().enumerate() {
            let value = get(&e.metadata);
            if value.is_some() {
                valid[i / 8] |= 1 << (i % 8);
            }
            values.push(value.unwrap_or_default());
        }
        Self {
            values,
            valid,
            ascending: Vec::new(),
            descending: Vec::new(),
        }
    }
    fn get(&self, id: u32) -> Option<T> {
        let i = id as usize;
        (self.valid[i / 8] & (1 << (i % 8)) != 0).then(|| self.values[i])
    }
    fn cmp(&self, a: u32, b: u32, desc: bool, rank: &[u32]) -> Ordering {
        let primary = match (self.get(a), self.get(b)) {
            (Some(a), Some(b)) => {
                if desc {
                    b.cmp(&a)
                } else {
                    a.cmp(&b)
                }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        primary.then_with(|| rank[a as usize].cmp(&rank[b as usize]))
    }
    fn sort(&mut self, rank: &[u32]) {
        let mut asc: Vec<_> = (0..self.values.len() as u32).collect();
        asc.par_sort_unstable_by(|&a, &b| self.cmp(a, b, false, rank));
        let mut desc = asc.clone();
        // Reverse value groups while retaining ascending names within ties.
        let known = asc.partition_point(|&id| self.get(id).is_some());
        desc[..known].reverse();
        let mut start = 0;
        while start < known {
            let value = self.get(desc[start]);
            let mut end = start + 1;
            while end < known && self.get(desc[end]) == value {
                end += 1;
            }
            desc[start..end].reverse();
            start = end;
        }
        self.ascending = asc;
        self.descending = desc;
    }
    fn heap_bytes(&self) -> usize {
        self.values.capacity() * T::WIDTH
            + self.valid.capacity()
            + (self.ascending.capacity() + self.descending.capacity()) * 4
    }
    fn write(&self, out: &mut Vec<u8>) {
        for &v in &self.values {
            v.write(out);
        }
        out.extend_from_slice(&self.valid);
        for ids in [&self.ascending, &self.descending] {
            for &id in ids {
                id.write(out);
            }
        }
    }
    fn read(data: &[u8], at: &mut usize, n: usize, sortable: bool) -> Result<Self, LoadError> {
        let mut take = |len: usize| -> Result<&[u8], LoadError> {
            let end = at
                .checked_add(len)
                .ok_or(LoadError::Corrupt("column size overflow"))?;
            let result = data.get(*at..end).ok_or(LoadError::Truncated)?;
            *at = end;
            Ok(result)
        };
        let values = take(
            n.checked_mul(T::WIDTH)
                .ok_or(LoadError::Corrupt("column size overflow"))?,
        )?
        .chunks_exact(T::WIDTH)
        .map(T::read)
        .collect();
        let valid = take(n.div_ceil(8))?.to_vec();
        if !n.is_multiple_of(8) && valid.last().is_some_and(|v| *v >> (n % 8) != 0) {
            return Err(LoadError::Corrupt("noncanonical validity bitmap"));
        }
        let mut ids = || -> Result<Vec<u32>, LoadError> {
            Ok(take(n * 4)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect())
        };
        let column = Self {
            values,
            valid,
            ascending: if sortable { ids()? } else { Vec::new() },
            descending: if sortable { ids()? } else { Vec::new() },
        };
        for i in 0..n {
            if column.get(i as u32).is_none() && column.values[i] != T::default() {
                return Err(LoadError::Corrupt("noncanonical unknown metadata"));
            }
        }
        Ok(column)
    }
    fn validate(&self, rank: &[u32]) -> Result<(), LoadError> {
        for (ids, desc) in [(&self.ascending, false), (&self.descending, true)] {
            let mut seen = vec![false; rank.len()];
            for &id in ids {
                let slot = seen
                    .get_mut(id as usize)
                    .ok_or(LoadError::Corrupt("metadata sort id out of range"))?;
                if *slot {
                    return Err(LoadError::Corrupt("duplicate metadata sort id"));
                }
                *slot = true;
            }
            if ids
                .par_windows(2)
                .any(|w| self.cmp(w[0], w[1], desc, rank).is_gt())
            {
                return Err(LoadError::Corrupt("metadata sort order"));
            }
        }
        Ok(())
    }
}

impl MetadataColumns {
    pub(crate) fn build(raw: &[RawEntry], set: ColumnSet) -> Self {
        Self {
            size: set
                .contains(ColumnSet::SIZE)
                .then(|| Column::build(raw, |m| m.size)),
            modified: set
                .contains(ColumnSet::MODIFIED)
                .then(|| Column::build(raw, |m| m.modified)),
            created: set
                .contains(ColumnSet::CREATED)
                .then(|| Column::build(raw, |m| m.created)),
            accessed: set
                .contains(ColumnSet::ACCESSED)
                .then(|| Column::build(raw, |m| m.accessed)),
            attributes: set
                .contains(ColumnSet::ATTRIBUTES)
                .then(|| Column::build(raw, |m| m.attributes)),
        }
    }
    pub(crate) fn present(&self) -> ColumnSet {
        ColumnSet(
            u32::from(self.size.is_some())
                | (u32::from(self.modified.is_some()) << 1)
                | (u32::from(self.created.is_some()) << 2)
                | (u32::from(self.accessed.is_some()) << 3)
                | (u32::from(self.attributes.is_some()) << 4),
        )
    }
    pub(crate) fn get(&self, id: u32) -> EntryMetadata {
        EntryMetadata {
            size: self.size.as_ref().and_then(|c| c.get(id)),
            modified: self.modified.as_ref().and_then(|c| c.get(id)),
            created: self.created.as_ref().and_then(|c| c.get(id)),
            accessed: self.accessed.as_ref().and_then(|c| c.get(id)),
            attributes: self.attributes.as_ref().and_then(|c| c.get(id)),
        }
    }
    pub(crate) fn order(&self, key: SortKey, desc: bool) -> Option<&[u32]> {
        macro_rules! order {
            ($c:expr) => {
                $c.as_ref().map(|c| {
                    if desc {
                        c.descending.as_slice()
                    } else {
                        c.ascending.as_slice()
                    }
                })
            };
        }
        match key {
            SortKey::Name => None,
            SortKey::Size => order!(self.size),
            SortKey::Modified => order!(self.modified),
            SortKey::Created => order!(self.created),
            SortKey::Accessed => order!(self.accessed),
        }
    }
    fn rank(sorted: &[u32]) -> Vec<u32> {
        let mut rank = vec![0; sorted.len()];
        for (i, &id) in sorted.iter().enumerate() {
            rank[id as usize] = i as u32;
        }
        rank
    }
    pub(crate) fn sort(&mut self, sorted: &[u32]) {
        if self.present().bits() & 15 == 0 {
            return;
        }
        let rank = Self::rank(sorted);
        if let Some(c) = &mut self.size {
            c.sort(&rank);
        }
        for c in [&mut self.modified, &mut self.created, &mut self.accessed]
            .into_iter()
            .flatten()
        {
            c.sort(&rank);
        }
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        self.size.as_ref().map_or(0, Column::heap_bytes)
            + [&self.modified, &self.created, &self.accessed]
                .into_iter()
                .flatten()
                .map(Column::heap_bytes)
                .sum::<usize>()
            + self.attributes.as_ref().map_or(0, Column::heap_bytes)
    }
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        if let Some(c) = &self.size {
            c.write(out);
        }
        for c in [&self.modified, &self.created, &self.accessed]
            .into_iter()
            .flatten()
        {
            c.write(out);
        }
        if let Some(c) = &self.attributes {
            c.write(out);
        }
    }
    pub(crate) fn read(
        data: &[u8],
        at: &mut usize,
        n: usize,
        set: ColumnSet,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            size: if set.contains(ColumnSet::SIZE) {
                Some(Column::read(data, at, n, true)?)
            } else {
                None
            },
            modified: if set.contains(ColumnSet::MODIFIED) {
                Some(Column::read(data, at, n, true)?)
            } else {
                None
            },
            created: if set.contains(ColumnSet::CREATED) {
                Some(Column::read(data, at, n, true)?)
            } else {
                None
            },
            accessed: if set.contains(ColumnSet::ACCESSED) {
                Some(Column::read(data, at, n, true)?)
            } else {
                None
            },
            attributes: if set.contains(ColumnSet::ATTRIBUTES) {
                Some(Column::read(data, at, n, false)?)
            } else {
                None
            },
        })
    }
    pub(crate) fn validate(&self, sorted: &[u32]) -> Result<(), LoadError> {
        if self.present().bits() & 15 == 0 {
            return Ok(());
        }
        let rank = Self::rank(sorted);
        if let Some(c) = &self.size {
            c.validate(&rank)?;
        }
        for c in [&self.modified, &self.created, &self.accessed]
            .into_iter()
            .flatten()
        {
            c.validate(&rank)?;
        }
        Ok(())
    }
}

/// Convert system timestamps without losing seconds or overflowing.
pub fn timestamp_ticks(time: std::time::SystemTime) -> Option<i64> {
    use std::time::UNIX_EPOCH;
    let (duration, negative) = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d, false),
        Err(e) => (e.duration(), true),
    };
    let nanos = i128::try_from(duration.as_nanos()).ok()?;
    let ticks = if negative {
        (-nanos).div_euclid(100)
    } else {
        nanos / 100
    };
    i64::try_from(ticks).ok()
}
