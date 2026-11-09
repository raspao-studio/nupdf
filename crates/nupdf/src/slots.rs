// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Storage for the effective cross-reference table.
//!
//! Most files have a table close to the number of objects they contain, and
//! a plain array is best. A hostile file can declare millions of slots in a
//! few bytes (a huge object number, or an xref stream with zero-width
//! fields), and MuPDF allocates every one. nupdf stores such tables sparsely:
//! explicit entries in a map, and one default entry for a range, so memory
//! follows what the file actually contains.

use std::collections::{BTreeMap, HashMap};

use crate::xref::{Entry, Kind};

/// Tables up to this many slots are stored as arrays.
pub const DENSE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Slots {
    len: usize,
    repr: Repr,
}

#[derive(Debug, Clone)]
enum Repr {
    Dense(Vec<Entry>),
    /// `map` first, then the range in `ranges` (start to end and entry,
    /// disjoint) that holds the number, otherwise `Entry::NONE`.
    Sparse {
        ranges: BTreeMap<i32, (i32, Entry)>,
        map: HashMap<i32, Entry>,
    },
}

impl Slots {
    /// A dense table from its entries.
    pub fn from_vec(entries: Vec<Entry>) -> Slots {
        Slots {
            len: entries.len(),
            repr: Repr::Dense(entries),
        }
    }

    /// `len` slots of `fill`, stored densely when small.
    pub fn filled(len: usize, fill: Entry) -> Slots {
        if len <= DENSE_LIMIT {
            return Slots::from_vec(vec![fill; len]);
        }
        let mut slots = Slots::sparse(len);
        slots.fill_range(0, i32::try_from(len).unwrap_or(i32::MAX), fill);
        slots
    }

    /// `len` unset slots, stored sparsely.
    pub fn sparse(len: usize) -> Slots {
        Slots {
            len,
            repr: Repr::Sparse {
                ranges: BTreeMap::new(),
                map: HashMap::new(),
            },
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, num: i32) -> Entry {
        let Some(i) = usize::try_from(num).ok().filter(|&i| i < self.len) else {
            return Entry::NONE;
        };
        match &self.repr {
            Repr::Dense(v) => v[i],
            Repr::Sparse { ranges, map } => match map.get(&num) {
                Some(e) => *e,
                None => ranges
                    .range(..=num)
                    .next_back()
                    .filter(|(_, (end, _))| num < *end)
                    .map_or(Entry::NONE, |(_, (_, e))| *e),
            },
        }
    }

    /// Sets one slot, growing the table if `num` is past its end.
    pub fn set(&mut self, num: i32, entry: Entry) {
        let Ok(i) = usize::try_from(num) else {
            return;
        };
        if i >= self.len {
            self.len = i + 1;
        }
        match &mut self.repr {
            Repr::Dense(v) => {
                if i >= v.len() {
                    v.resize(i + 1, Entry::NONE);
                }
                v[i] = entry;
            }
            Repr::Sparse { map, .. } => {
                map.insert(num, entry);
            }
        }
    }

    /// Sets `start..end` to `entry`. In a sparse table the range must not
    /// overlap one set before, and costs the same whatever its length.
    pub fn fill_range(&mut self, start: i32, end: i32, entry: Entry) {
        let (Ok(s), Ok(e)) = (usize::try_from(start), usize::try_from(end)) else {
            return;
        };
        if s >= e {
            return;
        }
        self.len = self.len.max(e);
        match &mut self.repr {
            Repr::Dense(v) => {
                if e > v.len() {
                    v.resize(e, Entry::NONE);
                }
                v[s..e].fill(entry);
            }
            Repr::Sparse { ranges, .. } => {
                // Join a range that ends here with the same entry.
                if let Some((&prev, &(prev_end, prev_entry))) = ranges.range(..start).next_back()
                    && prev_end == start
                    && prev_entry == entry
                {
                    ranges.insert(prev, (end, entry));
                } else {
                    ranges.insert(start, (end, entry));
                }
            }
        }
    }

    /// Numbers whose entry is neither free nor unset, highest first.
    pub fn in_use_descending(&self) -> Vec<i32> {
        let in_use = |e: &Entry| !matches!(e.kind, Kind::None | Kind::Free);
        let mut nums: Vec<i32> = match &self.repr {
            Repr::Dense(v) => v
                .iter()
                .enumerate()
                .filter(|(_, e)| in_use(e))
                .map(|(i, _)| i as i32)
                .collect(),
            Repr::Sparse { ranges, map } => {
                let mut nums: Vec<i32> = map
                    .iter()
                    .filter(|(_, e)| in_use(e))
                    .map(|(&n, _)| n)
                    .collect();
                for (&start, (end, e)) in ranges {
                    if in_use(e) {
                        nums.extend((start..*end).filter(|n| !map.contains_key(n)));
                    }
                }
                nums
            }
        };
        nums.sort_unstable_by(|a, b| b.cmp(a));
        nums
    }

    /// Entries that were set one by one (every slot, when dense).
    pub fn explicit(&self) -> Vec<(i32, Entry)> {
        match &self.repr {
            Repr::Dense(v) => v.iter().enumerate().map(|(i, e)| (i as i32, *e)).collect(),
            Repr::Sparse { map, .. } => map.iter().map(|(&n, &e)| (n, e)).collect(),
        }
    }

    /// Approximate heap use, for tests and limits.
    pub fn heap_bytes(&self) -> usize {
        let entry = std::mem::size_of::<Entry>();
        match &self.repr {
            Repr::Dense(v) => v.capacity() * entry,
            Repr::Sparse { ranges, map } => {
                map.capacity() * (entry + 8) + ranges.len() * (entry + 16)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ofs: i64) -> Entry {
        Entry {
            kind: Kind::InFile,
            ofs,
            generation: 0,
        }
    }

    #[test]
    fn dense_and_sparse_agree() {
        let mut small = Slots::filled(10, Entry::free(0));
        let mut big = Slots::filled(DENSE_LIMIT * 4, Entry::free(0));
        for s in [&mut small, &mut big] {
            s.set(3, at(100));
            s.set(0, Entry::free(65535));
            assert_eq!(s.get(3), at(100));
            assert_eq!(s.get(5), Entry::free(0));
            assert_eq!(s.get(0), Entry::free(65535));
            assert_eq!(s.get(-1), Entry::NONE);
            assert_eq!(s.in_use_descending(), vec![3]);
        }
        assert!(big.heap_bytes() < 1 << 16, "{}", big.heap_bytes());
    }

    #[test]
    fn setting_past_the_end_grows_with_unset_slots() {
        let mut sparse = Slots::sparse(4);
        sparse.fill_range(1, 4, Entry::free(0));
        for mut s in [Slots::filled(4, Entry::free(0)), sparse] {
            s.set(9, at(7));
            assert_eq!(s.len(), 10);
            assert_eq!(s.get(6), Entry::NONE);
            assert_eq!(s.get(9), at(7));
        }
    }

    #[test]
    fn sparse_ranges_cost_the_same_at_any_length() {
        let mut s = Slots::sparse(0);
        s.fill_range(10, 5_000_000, Entry::free(0));
        s.fill_range(5_000_000, 8_000_000, Entry::free(0));
        s.fill_range(8_000_000, 8_000_001, at(9));
        s.set(20, at(1));
        assert_eq!(s.len(), 8_000_001);
        assert_eq!(s.get(9), Entry::NONE);
        assert_eq!(s.get(10), Entry::free(0));
        assert_eq!(s.get(20), at(1));
        assert_eq!(s.get(7_999_999), Entry::free(0));
        assert_eq!(s.get(8_000_000), at(9));
        assert_eq!(s.get(8_000_001), Entry::NONE);
        assert!(s.heap_bytes() < 1 << 12, "{}", s.heap_bytes());
    }

    #[test]
    fn entries_are_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<Entry>(), 16);
    }
}
