//! Guest physical memory (a pool of 4 KiB frames) and the stage-1 page tables
//! we keep in it.
//!
//! Guest physical memory is one host allocation mapped into the VM once, at
//! IPA 0. The host only commits its pages when they're touched, so making it
//! large costs nothing up front. Linux's 4 KiB pages are a stage-1 concern —
//! our page tables use a 4 KiB granule — so the host's 16 KiB page size only
//! matters for that single stage-2 mapping.

use std::{io, ptr::NonNull};

use linux_abi::wire::{Decoder, Encoder, Truncated};

use crate::{
    hv::{self, HvError, Vm},
    mach::MemoryEntry,
};

/// Linux's page size on arm64, which the guest sees.
pub const PAGE_SIZE: u64 = 4096;
/// Guest physical memory size. Must fit the VM's IPA size (36 bits by default).
pub const PHYS_SIZE: u64 = 8 << 30;

// Descriptor bits (4 KiB granule, 4 levels, 48-bit VAs).
const DESC_VALID: u64 = 1 << 0;
/// A table descriptor at levels 0–2, a page descriptor at level 3.
const DESC_TABLE_OR_PAGE: u64 = 1 << 1;
const DESC_AP_EL0: u64 = 1 << 6; // AP[1]: EL0 can access
const DESC_AP_RO: u64 = 1 << 7; // AP[2]: read-only
const DESC_SH_INNER: u64 = 3 << 8;
const DESC_AF: u64 = 1 << 10;
const DESC_PXN: u64 = 1 << 53;
const DESC_UXN: u64 = 1 << 54;
const DESC_ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Linux `PROT_*` bits, as stored in VMAs.
pub mod prot {
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;
}

/// Page attributes for user memory with Linux protection `prot`. arm64 can't
/// express write-only or execute-only for EL0, so (like Linux) write implies
/// read and exec implies read. PROT_NONE pages stay mapped (so mprotect can
/// restore them) but EL1-only, so any EL0 access is a permission fault.
pub fn user_attrs(p: u64) -> u64 {
    let mut a = DESC_VALID | DESC_TABLE_OR_PAGE | DESC_AF | DESC_SH_INNER | DESC_PXN;
    if p & (prot::READ | prot::WRITE | prot::EXEC) != 0 {
        a |= DESC_AP_EL0;
        if p & prot::WRITE == 0 {
            a |= DESC_AP_RO;
        }
    }
    if p & prot::EXEC == 0 {
        a |= DESC_UXN;
    }
    a
}

/// Page attributes for kernel code: EL1 read-only and executable.
pub fn kernel_code_attrs() -> u64 {
    DESC_VALID | DESC_TABLE_OR_PAGE | DESC_AF | DESC_SH_INNER | DESC_AP_RO | DESC_UXN
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

/// Guest physical memory and its frame allocator.
///
/// A frame that's unmapped isn't reusable at once: a vCPU may still hold a
/// stale TLB entry for it. It waits in quarantine until the TLB has been
/// flushed ([`PhysMem::release_quarantine`]). Host pages (16 KiB) whose four
/// frames are all free are handed back to macOS, so a guest's memory use can
/// shrink again.
pub struct PhysMem {
    host: NonNull<u8>,
    /// Frames at or above this have never been handed out, so they're zero.
    next_fresh: u64,
    /// Free frames; they're zeroed again on reuse.
    free: Vec<u64>,
    /// Unmapped frames waiting for a TLB flush.
    quarantine: Vec<u64>,
    /// Per host page: how many of its frames are free, and (bit 7) whether
    /// it has been handed back to macOS.
    host_pages: Vec<u8>,
}

/// Frames per host page.
const FRAMES_PER_HOST_PAGE: u64 = HOST_PAGE / PAGE_SIZE;
const HOST_PAGE: u64 = 16 << 10;
const RELEASED: u8 = 0x80;

impl PhysMem {
    pub fn new() -> io::Result<Self> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PHYS_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(PhysMem {
            host: NonNull::new(p.cast()).unwrap(),
            next_fresh: 0,
            free: Vec::new(),
            quarantine: Vec::new(),
            host_pages: vec![0; (PHYS_SIZE / HOST_PAGE) as usize],
        })
    }

    /// A copy-on-write snapshot of all of guest physical memory, for a forked
    /// child. The VM can keep running (other threads may be): macOS write-
    /// protects the pages for the VM too, so its later writes take copies
    /// (checked: a guest writing through HVF after the snapshot doesn't
    /// change it).
    pub fn snapshot(&self) -> io::Result<MemoryEntry> {
        MemoryEntry::snapshot(self.host.as_ptr(), PHYS_SIZE)
    }

    /// The allocator's state, which goes with a snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        // A child's fresh vCPU has no stale TLB entries: quarantined frames
        // are simply free there.
        e.u64(self.next_fresh).u64((self.free.len() + self.quarantine.len()) as u64);
        for &f in self.free.iter().chain(&self.quarantine) {
            e.u64(f);
        }
    }

    /// Guest physical memory from a parent's snapshot and allocator state.
    pub fn restore(entry: &MemoryEntry, d: &mut Decoder) -> Result<Self, Truncated> {
        let mut p = PhysMem::new().map_err(|_| Truncated)?;
        entry.map_at(p.host.as_ptr(), PHYS_SIZE).map_err(|_| Truncated)?;
        p.next_fresh = d.u64()?;
        let n = d.count(8)?;
        p.quarantine = (0..n).map(|_| d.u64()).collect::<Result<_, _>>()?;
        p.release_quarantine();
        Ok(p)
    }

    /// Map all of guest physical memory into `vm` at IPA 0.
    pub fn map_into(&self, vm: &Vm) -> Result<(), HvError> {
        // Safety: the host memory lives as long as `self`, which `crate::run`
        // keeps alive until after the VM is destroyed.
        unsafe {
            vm.map(
                self.host.as_ptr(),
                0,
                PHYS_SIZE as usize,
                hv::MEMORY_READ | hv::MEMORY_WRITE | hv::MEMORY_EXEC,
            )
        }
    }

    /// Host pointer to guest physical address `pa`.
    pub fn ptr(&self, pa: u64) -> *mut u8 {
        assert!(pa < PHYS_SIZE, "physical address {pa:#x} out of range");
        unsafe { self.host.as_ptr().add(pa as usize) }
    }

    /// A zeroed frame.
    pub fn alloc(&mut self) -> Result<u64, OutOfMemory> {
        if let Some(pa) = self.free.pop() {
            let page = &mut self.host_pages[(pa / HOST_PAGE) as usize];
            if *page & RELEASED != 0 {
                let host = unsafe { self.host.as_ptr().add((pa & !(HOST_PAGE - 1)) as usize) };
                unsafe { libc::madvise(host.cast(), HOST_PAGE as usize, libc::MADV_FREE_REUSE) };
                *page &= !RELEASED;
            }
            *page -= 1;
            unsafe { std::ptr::write_bytes(self.ptr(pa), 0, PAGE_SIZE as usize) };
            return Ok(pa);
        }
        if self.next_fresh >= PHYS_SIZE {
            return Err(OutOfMemory);
        }
        let pa = self.next_fresh;
        self.next_fresh += PAGE_SIZE;
        Ok(pa)
    }

    /// An unmapped frame: free once the TLB has been flushed.
    pub fn free(&mut self, pa: u64) {
        self.quarantine.push(pa);
    }

    /// Whether frames are waiting for a TLB flush.
    pub fn has_quarantine(&self) -> bool {
        !self.quarantine.is_empty()
    }

    /// The TLB has been flushed everywhere: quarantined frames are free, and
    /// host pages left entirely free go back to macOS.
    pub fn release_quarantine(&mut self) {
        for pa in std::mem::take(&mut self.quarantine) {
            self.free.push(pa);
            let i = (pa / HOST_PAGE) as usize;
            self.host_pages[i] += 1;
            if self.host_pages[i] == FRAMES_PER_HOST_PAGE as u8 {
                let host = unsafe { self.host.as_ptr().add(i * HOST_PAGE as usize) };
                // Its contents can go; it's zeroed again before reuse.
                unsafe { libc::madvise(host.cast(), HOST_PAGE as usize, libc::MADV_FREE_REUSABLE) };
                self.host_pages[i] |= RELEASED;
            }
        }
    }

    pub fn read_u64(&self, pa: u64) -> u64 {
        unsafe { self.ptr(pa).cast::<u64>().read() }
    }

    pub fn write_u64(&mut self, pa: u64, v: u64) {
        unsafe { self.ptr(pa).cast::<u64>().write(v) }
    }

    /// Frames in use, quarantined ones included (for tests and diagnostics).
    pub fn frames_in_use(&self) -> u64 {
        self.next_fresh / PAGE_SIZE - self.free.len() as u64
    }
}

impl Drop for PhysMem {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.host.as_ptr().cast(), PHYS_SIZE as usize) };
    }
}

fn index(va: u64, level: u32) -> u64 {
    (va >> (39 - 9 * level)) & 511
}

/// A 4-level translation table (what TTBR0 or TTBR1 points at).
pub struct PageTable {
    pub root: u64,
}

impl PageTable {
    pub fn new(phys: &mut PhysMem) -> Result<Self, OutOfMemory> {
        Ok(PageTable { root: phys.alloc()? })
    }

    /// Physical address of the level-3 entry for `va`, creating intermediate
    /// tables as needed.
    fn entry(&self, phys: &mut PhysMem, va: u64) -> Result<u64, OutOfMemory> {
        let mut table = self.root;
        for level in 0..3 {
            let slot = table + index(va, level) * 8;
            let desc = phys.read_u64(slot);
            table = if desc & DESC_VALID != 0 {
                desc & DESC_ADDR
            } else {
                let next = phys.alloc()?;
                phys.write_u64(slot, next | DESC_TABLE_OR_PAGE | DESC_VALID);
                next
            };
        }
        Ok(table + index(va, 3) * 8)
    }

    /// The physical frame `va` maps to, if any.
    pub fn lookup(&self, phys: &PhysMem, va: u64) -> Option<u64> {
        let mut table = self.root;
        for level in 0..4 {
            let desc = phys.read_u64(table + index(va, level) * 8);
            if desc & DESC_VALID == 0 {
                return None;
            }
            table = desc & DESC_ADDR;
        }
        Some(table)
    }

    /// Map the page at `va` to frame `pa`. The page must not be mapped.
    pub fn map(&self, phys: &mut PhysMem, va: u64, pa: u64, attrs: u64) -> Result<(), OutOfMemory> {
        let slot = self.entry(phys, va)?;
        debug_assert_eq!(phys.read_u64(slot) & DESC_VALID, 0, "{va:#x} already mapped");
        phys.write_u64(slot, pa | attrs);
        Ok(())
    }

    /// Visit the level-3 entry of every mapped page in `[start, end)`, skipping
    /// absent subtrees, as `f(phys, va, entry_pa)`.
    pub fn for_each_mapped(&self, phys: &mut PhysMem, start: u64, end: u64, f: &mut dyn FnMut(&mut PhysMem, u64, u64)) {
        fn walk(phys: &mut PhysMem, table: u64, level: u32, base: u64, start: u64, end: u64, f: &mut dyn FnMut(&mut PhysMem, u64, u64)) {
            let span = 1u64 << (39 - 9 * level);
            for i in 0..512 {
                let lo = base + i * span;
                let hi = lo + span;
                if hi <= start || lo >= end {
                    continue;
                }
                let slot = table + i * 8;
                let desc = phys.read_u64(slot);
                if desc & DESC_VALID == 0 {
                    continue;
                }
                if level == 3 {
                    f(phys, lo, slot);
                } else {
                    walk(phys, desc & DESC_ADDR, level + 1, lo, start, end, f);
                }
            }
        }
        walk(phys, self.root, 0, 0, start & ((1 << 48) - 1), end.min(1 << 48), f);
    }

    /// Unmap every page in `[start, end)`, returning pool frames to `phys`
    /// (pages in shared windows belong to the window). Returns whether
    /// anything was mapped (and so the TLB needs flushing).
    pub fn unmap_range(&self, phys: &mut PhysMem, start: u64, end: u64) -> bool {
        let mut any = false;
        self.for_each_mapped(phys, start, end, &mut |phys, _va, slot| {
            let frame = phys.read_u64(slot) & DESC_ADDR;
            phys.write_u64(slot, 0);
            if frame < PHYS_SIZE {
                phys.free(frame);
            }
            any = true;
        });
        any
    }

    /// Change the attributes of every mapped page in `[start, end)`. Returns
    /// whether anything was mapped (and so the TLB needs flushing).
    pub fn protect_range(&self, phys: &mut PhysMem, start: u64, end: u64, attrs: u64) -> bool {
        let mut any = false;
        self.for_each_mapped(phys, start, end, &mut |phys, _va, slot| {
            let frame = phys.read_u64(slot) & DESC_ADDR;
            phys.write_u64(slot, frame | attrs);
            any = true;
        });
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_lookup_unmap() {
        let mut phys = PhysMem::new().unwrap();
        let pt = PageTable::new(&mut phys).unwrap();
        let va = 0x7fff_1234_5000;
        assert_eq!(pt.lookup(&phys, va), None);
        let frame = phys.alloc().unwrap();
        pt.map(&mut phys, va, frame, user_attrs(prot::READ)).unwrap();
        assert_eq!(pt.lookup(&phys, va), Some(frame));
        assert_eq!(pt.lookup(&phys, va + PAGE_SIZE), None);
        assert!(pt.unmap_range(&mut phys, va, va + PAGE_SIZE));
        assert_eq!(pt.lookup(&phys, va), None);
        assert!(!pt.unmap_range(&mut phys, va, va + PAGE_SIZE));
    }

    #[test]
    fn recycled_frames_are_zeroed() {
        let mut phys = PhysMem::new().unwrap();
        let a = phys.alloc().unwrap();
        unsafe { phys.ptr(a).write_bytes(0xaa, PAGE_SIZE as usize) };
        phys.free(a);
        assert_ne!(phys.alloc().unwrap(), a, "not reused while quarantined");
        phys.release_quarantine();
        let b = phys.alloc().unwrap();
        assert_eq!(a, b);
        assert!(unsafe { std::slice::from_raw_parts(phys.ptr(b), PAGE_SIZE as usize) }.iter().all(|&x| x == 0));
    }

    /// A host page whose frames are all free goes back to macOS, and comes
    /// back zeroed when reused.
    #[test]
    fn free_host_pages_are_released() {
        fn footprint() -> u64 {
            let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
            let pid = std::process::id() as i32;
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&mut ri as *mut libc::rusage_info_v4).cast()) };
            ri.ri_phys_footprint
        }
        let mut phys = PhysMem::new().unwrap();
        // 64 MiB of frames, touched.
        let frames: Vec<u64> = (0..(64 << 20) / PAGE_SIZE).map(|_| phys.alloc().unwrap()).collect();
        for &f in &frames {
            unsafe { phys.ptr(f).write_bytes(1, PAGE_SIZE as usize) };
        }
        let before = footprint();
        for &f in &frames {
            phys.free(f);
        }
        phys.release_quarantine();
        let after = footprint();
        assert!(before - after > 60 << 20, "footprint {} MiB -> {} MiB", before >> 20, after >> 20);
        let f = phys.alloc().unwrap();
        assert!(unsafe { std::slice::from_raw_parts(phys.ptr(f), PAGE_SIZE as usize) }.iter().all(|&x| x == 0));
    }

    #[test]
    fn attrs() {
        let none = user_attrs(0);
        assert_eq!(none & DESC_AP_EL0, 0);
        assert_ne!(none & DESC_UXN, 0);
        let rx = user_attrs(prot::READ | prot::EXEC);
        assert_ne!(rx & DESC_AP_EL0, 0);
        assert_ne!(rx & DESC_AP_RO, 0);
        assert_eq!(rx & DESC_UXN, 0);
        let rw = user_attrs(prot::READ | prot::WRITE);
        assert_eq!(rw & DESC_AP_RO, 0);
        assert_ne!(rw & DESC_UXN, 0);
        assert_ne!(rw & DESC_PXN, 0);
    }
}
