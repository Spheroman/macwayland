//! The process's mapped regions (VMAs): the source of truth for what the guest
//! may touch. Page-table entries are filled in from these on first access.

use std::collections::BTreeMap;

/// What a region's pages are backed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Frames from the private pool, allocated on first touch.
    Private,
    /// A shared window ([`crate::shared`]); `ipa` is the guest physical
    /// address of the region's first page, and the rest follow contiguously.
    Shared { ipa: u64 },
    /// A private file mapping nobody has written: read through a read-only
    /// window onto the file, like `Shared`, until it is made writable, when
    /// it becomes private frames holding a copy (see
    /// [`crate::memory::AddressSpace::mprotect`]).
    File { ipa: u64 },
}

/// One mapped region. Its start is the key in [`Vmas`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vma {
    pub end: u64,
    /// Linux `PROT_*` bits.
    pub prot: u64,
    pub backing: Backing,
}

impl Vma {
    /// The guest physical address backing `va` (in a region starting at
    /// `start`), if the region is shared.
    pub fn shared_pa(&self, start: u64, va: u64) -> Option<u64> {
        match self.backing {
            Backing::Shared { ipa } | Backing::File { ipa } => Some(ipa + (va - start)),
            Backing::Private => None,
        }
    }
}

/// Non-overlapping regions, keyed by start address. Adjacent regions with the
/// same protection and contiguous backing are merged.
#[derive(Debug, Default, Clone)]
pub struct Vmas {
    map: BTreeMap<u64, Vma>,
}

impl Vmas {
    /// The region containing `addr`, as (start, region).
    pub fn find(&self, addr: u64) -> Option<(u64, Vma)> {
        let (&start, &vma) = self.map.range(..=addr).next_back()?;
        (addr < vma.end).then_some((start, vma))
    }

    /// Whether no region overlaps `[start, end)`.
    pub fn is_free(&self, start: u64, end: u64) -> bool {
        self.find(start).is_none() && self.map.range(start..end).next().is_none()
    }

    /// Whether `[start, end)` is entirely mapped.
    pub fn covers(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        while at < end {
            match self.find(at) {
                Some((_, vma)) => at = vma.end,
                None => return false,
            }
        }
        true
    }

    /// Add a region over `[start, end)`, which must be free.
    pub fn insert(&mut self, start: u64, end: u64, prot: u64, backing: Backing) {
        debug_assert!(self.is_free(start, end), "{start:#x}..{end:#x} overlaps");
        self.map.insert(start, Vma { end, prot, backing });
        self.merge_at(start);
        self.merge_at(end);
    }

    /// Remove `[start, end)`, trimming or splitting regions that straddle it.
    pub fn remove(&mut self, start: u64, end: u64) {
        self.split_at(start);
        self.split_at(end);
        let inside: Vec<u64> = self.map.range(start..end).map(|(&s, _)| s).collect();
        for s in inside {
            self.map.remove(&s);
        }
    }

    /// Set the protection of `[start, end)`, which must be entirely mapped.
    pub fn protect(&mut self, start: u64, end: u64, prot: u64) {
        debug_assert!(self.covers(start, end));
        self.split_at(start);
        self.split_at(end);
        let inside: Vec<u64> = self.map.range(start..end).map(|(&s, _)| s).collect();
        for &s in &inside {
            self.map.get_mut(&s).unwrap().prot = prot;
        }
        for s in inside {
            self.merge_at(s);
        }
        self.merge_at(end);
    }

    /// Regions overlapping `[start, end)`, as (start, region).
    pub fn overlapping(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, Vma)> + '_ {
        let first = self.find(start).map_or(start, |(s, _)| s);
        self.map.range(first..end).map(|(&s, &v)| (s, v))
    }

    /// The highest `len`-byte gap that lies within `[floor, ceiling)`.
    pub fn find_free_top_down(&self, len: u64, floor: u64, ceiling: u64) -> Option<u64> {
        let mut top = ceiling;
        for (&start, vma) in self.map.range(..ceiling).rev() {
            if vma.end <= top && top - vma.end >= len && top - len >= floor {
                return Some(top - len);
            }
            top = top.min(start);
            if top < floor.saturating_add(len) {
                return None;
            }
        }
        (top >= floor.saturating_add(len)).then(|| top - len)
    }

    /// Split the region containing `addr` (if any) into two at `addr`.
    fn split_at(&mut self, addr: u64) {
        if let Some((start, vma)) = self.find(addr)
            && start < addr
        {
            self.map.insert(start, Vma { end: addr, ..vma });
            let backing = match vma.backing {
                Backing::Shared { ipa } => Backing::Shared { ipa: ipa + (addr - start) },
                Backing::File { ipa } => Backing::File { ipa: ipa + (addr - start) },
                Backing::Private => Backing::Private,
            };
            self.map.insert(addr, Vma { backing, ..vma });
        }
    }

    /// Merge the region ending at `addr` with the one starting there, if they
    /// have the same protection and their backing continues across `addr`.
    fn merge_at(&mut self, addr: u64) {
        let Some(&next) = self.map.get(&addr) else { return };
        let Some((&prev_start, prev)) = self.map.range_mut(..addr).next_back() else { return };
        let contiguous = match (prev.backing, next.backing) {
            (Backing::Private, Backing::Private) => true,
            (Backing::Shared { ipa: a }, Backing::Shared { ipa: b }) | (Backing::File { ipa: a }, Backing::File { ipa: b }) => {
                a + (addr - prev_start) == b
            }
            _ => false,
        };
        if prev.end == addr && prev.prot == next.prot && contiguous {
            prev.end = next.end;
            self.map.remove(&addr);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, Vma)> + '_ {
        self.map.iter().map(|(&s, &v)| (s, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: u64 = 1;
    const RW: u64 = 3;
    const P: Backing = Backing::Private;

    fn regions(v: &Vmas) -> Vec<(u64, u64, u64)> {
        v.iter().map(|(s, vma)| (s, vma.end, vma.prot)).collect()
    }

    #[test]
    fn insert_merges_neighbours() {
        let mut v = Vmas::default();
        v.insert(0x1000, 0x2000, RW, P);
        v.insert(0x3000, 0x4000, RW, P);
        v.insert(0x2000, 0x3000, RW, P);
        assert_eq!(regions(&v), [(0x1000, 0x4000, RW)]);
        v.insert(0x4000, 0x5000, R, P);
        assert_eq!(regions(&v), [(0x1000, 0x4000, RW), (0x4000, 0x5000, R)]);
    }

    #[test]
    fn remove_splits() {
        let mut v = Vmas::default();
        v.insert(0x1000, 0x5000, RW, P);
        v.remove(0x2000, 0x3000);
        assert_eq!(regions(&v), [(0x1000, 0x2000, RW), (0x3000, 0x5000, RW)]);
        assert!(v.is_free(0x2000, 0x3000));
        assert!(!v.is_free(0x1800, 0x2800));
        v.remove(0x0, 0x10000);
        assert!(regions(&v).is_empty());
    }

    #[test]
    fn protect_splits_and_remerges() {
        let mut v = Vmas::default();
        v.insert(0x1000, 0x5000, RW, P);
        v.protect(0x2000, 0x3000, R);
        assert_eq!(regions(&v), [(0x1000, 0x2000, RW), (0x2000, 0x3000, R), (0x3000, 0x5000, RW)]);
        v.protect(0x2000, 0x3000, RW);
        assert_eq!(regions(&v), [(0x1000, 0x5000, RW)]);
    }

    #[test]
    fn covers_spans_regions() {
        let mut v = Vmas::default();
        v.insert(0x1000, 0x2000, RW, P);
        v.insert(0x2000, 0x3000, R, P);
        assert!(v.covers(0x1000, 0x3000));
        assert!(!v.covers(0x1000, 0x4000));
        assert!(!v.covers(0x0, 0x2000));
    }

    #[test]
    fn top_down_placement() {
        let mut v = Vmas::default();
        assert_eq!(v.find_free_top_down(0x1000, 0x1000, 0x10000), Some(0xf000));
        v.insert(0xe000, 0x10000, RW, P);
        assert_eq!(v.find_free_top_down(0x1000, 0x1000, 0x10000), Some(0xd000));
        v.insert(0x2000, 0xd000, RW, P);
        assert_eq!(v.find_free_top_down(0x1000, 0x1000, 0x10000), Some(0xd000));
        assert_eq!(v.find_free_top_down(0x2000, 0x1000, 0x10000), None);
        // A region straddling the ceiling.
        let mut v = Vmas::default();
        v.insert(0x8000, 0x20000, RW, P);
        assert_eq!(v.find_free_top_down(0x1000, 0x1000, 0x10000), Some(0x7000));
    }

    #[test]
    fn split_shifts_shared_backing_and_merge_needs_contiguity() {
        let mut v = Vmas::default();
        v.insert(0x10000, 0x14000, RW, Backing::Shared { ipa: 0x2_0000_0000 });
        v.remove(0x11000, 0x12000);
        assert_eq!(v.find(0x12000).unwrap().1.backing, Backing::Shared { ipa: 0x2_0000_2000 });
        assert_eq!(v.find(0x13fff).unwrap().1.shared_pa(0x12000, 0x13000), Some(0x2_0000_3000));
        // Refilling the hole with the same physical pages merges back...
        v.insert(0x11000, 0x12000, RW, Backing::Shared { ipa: 0x2_0000_1000 });
        assert_eq!(v.iter().count(), 1);
        // ...but different pages, or private memory, don't.
        v.remove(0x11000, 0x12000);
        v.insert(0x11000, 0x12000, RW, Backing::Shared { ipa: 0x3_0000_0000 });
        assert_eq!(v.iter().count(), 3);
        v.remove(0x11000, 0x12000);
        v.insert(0x11000, 0x12000, RW, Backing::Private);
        assert_eq!(v.iter().count(), 3);
    }
}
