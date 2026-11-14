//! Guest physical memory (a pool of 4 KiB frames) and the stage-1 page tables
//! we keep in it.
//!
//! Guest physical memory is one host allocation, reserved but not committed
//! ([`POOL_SIZE`], 512 GiB), from which every address space of a VM takes
//! frames ([`Pool`]): so `execve` gets a new address space without a new
//! pool, and (docs/processes.md) many processes can share one VM. The host
//! only commits pages when they're touched, so the size costs nothing up
//! front. Linux's 4 KiB pages are a stage-1 concern — our page tables use a
//! 4 KiB granule — so the host's 16 KiB page size only matters for the
//! stage-2 mapping of the pool.

use std::{
    io,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Arc, Mutex, MutexGuard,
    },
};


use crate::{
    hv::{self, HvError, Vm},
};

/// Linux's page size on arm64, which the guest sees.
pub const PAGE_SIZE: u64 = 4096;
/// The frame pool's size: guest physical `[0, POOL_SIZE)`. Shared windows
/// live above it, up to the VM's 40-bit IPA limit.
pub const POOL_SIZE: u64 = 512 << 30;
/// Frames go to an address space this many at a time (64 KiB: four host
/// pages, and the fault-around size), so its memory fills whole host pages,
/// which go back to macOS whole when it unmaps or exits.
pub const CHUNK_FRAMES: usize = 16;

// Descriptor bits (4 KiB granule, 4 levels, 48-bit VAs).
const DESC_VALID: u64 = 1 << 0;
/// A table descriptor at levels 0–2, a page descriptor at level 3.
const DESC_TABLE_OR_PAGE: u64 = 1 << 1;
const DESC_AP_EL0: u64 = 1 << 6; // AP[1]: EL0 can access
const DESC_AP_RO: u64 = 1 << 7; // AP[2]: read-only
const DESC_SH_INNER: u64 = 3 << 8;
const DESC_AF: u64 = 1 << 10;
/// Not global: the TLB tags the entry with the ASID it was used under.
const DESC_NG: u64 = 1 << 11;
const DESC_PXN: u64 = 1 << 53;
const DESC_UXN: u64 = 1 << 54;
/// Software bit (55–58 are the OS's): the page's frame is shared copy on
/// write with another address space (a fork inside the VM), so the page
/// stays read-only, whatever its protection, until it's written.
pub const DESC_COW: u64 = 1 << 55;
pub const DESC_ADDR: u64 = 0x0000_ffff_ffff_f000;

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
/// User pages are not global: each address space's TLB entries carry its
/// ASID, so switching address spaces (or reusing a VA in a new one) needs
/// no flush of the others'.
pub fn user_attrs(p: u64) -> u64 {
    let mut a = DESC_VALID | DESC_TABLE_OR_PAGE | DESC_AF | DESC_SH_INNER | DESC_PXN | DESC_NG;
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

/// Page attributes for kernel code: EL1 read-only and executable, global
/// (the same in every address space).
pub fn kernel_code_attrs() -> u64 {
    DESC_VALID | DESC_TABLE_OR_PAGE | DESC_AF | DESC_SH_INNER | DESC_AP_RO | DESC_UXN
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

/// Frames per host page.
const FRAMES_PER_HOST_PAGE: u64 = HOST_PAGE / PAGE_SIZE;
const HOST_PAGE: u64 = 16 << 10;
const RELEASED: u8 = 0x80;

/// The pool comes in segments that double: guest physical `[0, 8 GiB)`,
/// then `[8, 16)`, `[16, 32)` GiB, and so on up to 512 GiB. Each segment is a
/// host reservation of its own, made and mapped into the VM's stage 2 when
/// frames are first handed out from it, because both cost by size:
/// reserving 512 GiB of host address space at once takes 0.3 ms and giving
/// it back 0.45 ms (against 10–20 µs for 8 GiB), and `hv_vm_map` of it 1.2 ms
/// (33 µs for 8 GiB), which every process would pay while each is a VM of
/// its own. A VM costs what it did with an 8 GiB pool until a process needs
/// more.
const FIRST_SEGMENT: u64 = 8 << 30;
const SEGMENTS: usize = 7;

/// The segment holding guest physical address `pa`.
fn segment_of(pa: u64) -> usize {
    if pa < FIRST_SEGMENT { 0 } else { (64 - (pa / FIRST_SEGMENT).leading_zeros()) as usize }
}

/// Where segment `k` starts, and its length.
fn segment(k: usize) -> (u64, u64) {
    if k == 0 { (0, FIRST_SEGMENT) } else { (FIRST_SEGMENT << (k - 1), FIRST_SEGMENT << (k - 1)) }
}

/// The frames of a VM's guest physical memory, for all its address spaces.
///
/// Each frame has a reference count: how many address spaces hold it (one,
/// until copy-on-write fork shares them). Bookkeeping grows with the frames
/// handed out, not with the pool's size. A frame whose count drops to zero
/// is free again; the address space that dropped it has had its TLB entries
/// flushed first (see [`PhysMem`]). Host pages (16 KiB) whose four frames
/// are all free are handed back to macOS, so a guest's memory use can
/// shrink again.
pub struct Pool {
    /// Each segment's host memory, once reserved.
    hosts: [AtomicPtr<u8>; SEGMENTS],
    state: Mutex<PoolState>,
}

struct PoolState {
    /// Frames at or above this have never been handed out, so they're zero.
    next_fresh: u64,
    /// Free frames; they're zeroed again on reuse.
    free: Vec<u64>,
    /// Per frame below `next_fresh`: how many address spaces hold it.
    refs: Vec<u32>,
    /// Per host page below `next_fresh`: how many of its frames are free,
    /// and (bit 7) whether it has been handed back to macOS.
    host_pages: Vec<u8>,
    /// Segments `[0, reserved)` have host memory.
    reserved: usize,
    /// Segments `[0, mapped)` are in the VM's stage 2.
    mapped: usize,
    /// A VM is attached: segments reserved from now on are mapped into it.
    vm_live: bool,
}

impl Pool {
    pub fn new() -> io::Result<Arc<Pool>> {
        let state = PoolState { next_fresh: 0, free: Vec::new(), refs: Vec::new(), host_pages: Vec::new(), reserved: 0, mapped: 0, vm_live: false };
        let pool = Pool { hosts: Default::default(), state: Mutex::new(state) };
        pool.reserve(&mut pool.state())?;
        Ok(Arc::new(pool))
    }

    fn state(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserve the next segment's host memory (and map it into the VM, if
    /// one is attached).
    fn reserve(&self, s: &mut PoolState) -> io::Result<()> {
        let k = s.reserved;
        let (start, len) = segment(k);
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.hosts[k].store(p.cast(), Ordering::Release);
        s.reserved += 1;
        if s.vm_live {
            self.map_segment(s).map_err(io::Error::other)?;
        }
        debug_assert_eq!(start + len, segment(k + 1).0);
        Ok(())
    }

    /// Map the next reserved segment into the VM.
    fn map_segment(&self, s: &mut PoolState) -> Result<(), HvError> {
        let k = s.mapped;
        let (start, len) = segment(k);
        // Safety: the host memory lives as long as the pool, which the
        // runtime keeps until after the VM is destroyed.
        unsafe { hv::map(self.ptr(start), start, len as usize, hv::MEMORY_READ | hv::MEMORY_WRITE | hv::MEMORY_EXEC)? };
        s.mapped += 1;
        Ok(())
    }

    /// Host pointer to guest physical address `pa`.
    pub fn ptr(&self, pa: u64) -> *mut u8 {
        assert!(pa < POOL_SIZE, "physical address {pa:#x} out of range");
        let k = segment_of(pa);
        let host = self.hosts[k].load(Ordering::Acquire);
        debug_assert!(!host.is_null(), "physical address {pa:#x} in a segment not reserved");
        unsafe { host.add((pa - segment(k).0) as usize) }
    }

    /// A chunk of zeroed frames, each held once, into `out`: free frames if
    /// there are enough, else a fresh 64 KiB.
    fn take(&self, out: &mut Vec<u64>) -> Result<(), OutOfMemory> {
        let mut s = self.state();
        if s.free.len() >= CHUNK_FRAMES {
            for _ in 0..CHUNK_FRAMES {
                let pa = s.free.pop().unwrap();
                let page = &mut s.host_pages[(pa / HOST_PAGE) as usize];
                if *page & RELEASED != 0 {
                    let host = self.ptr(pa & !(HOST_PAGE - 1));
                    unsafe { libc::madvise(host.cast(), HOST_PAGE as usize, libc::MADV_FREE_REUSE) };
                    *page &= !RELEASED;
                }
                *page -= 1;
                s.refs[(pa / PAGE_SIZE) as usize] = 1;
                unsafe { std::ptr::write_bytes(self.ptr(pa), 0, PAGE_SIZE as usize) };
                out.push(pa);
            }
            return Ok(());
        }
        let start = s.next_fresh;
        let end = start + CHUNK_FRAMES as u64 * PAGE_SIZE;
        if end > POOL_SIZE {
            return Err(OutOfMemory);
        }
        while s.reserved <= segment_of(end - 1) {
            self.reserve(&mut s).map_err(|e| {
                eprintln!("macwl-runtime: growing guest memory: {e}");
                OutOfMemory
            })?;
        }
        s.next_fresh = end;
        s.refs.resize((end / PAGE_SIZE) as usize, 0);
        s.host_pages.resize((end / HOST_PAGE) as usize, 0);
        // Highest first: the cache hands them out from the end, lowest first.
        for i in (start / PAGE_SIZE..end / PAGE_SIZE).rev() {
            s.refs[i as usize] = 1;
            out.push(i * PAGE_SIZE);
        }
        Ok(())
    }

    /// Drop a hold on each of `frames` (whose TLB entries are gone): those
    /// no one holds any more are free, and host pages left entirely free go
    /// back to macOS.
    pub fn release(&self, frames: &[u64]) {
        let mut s = self.state();
        let mut emptied = Vec::new();
        for &pa in frames {
            let r = &mut s.refs[(pa / PAGE_SIZE) as usize];
            assert!(*r > 0, "frame {pa:#x} released twice");
            *r -= 1;
            if *r > 0 {
                continue;
            }
            s.free.push(pa);
            let i = (pa / HOST_PAGE) as usize;
            s.host_pages[i] += 1;
            if s.host_pages[i] == FRAMES_PER_HOST_PAGE as u8 {
                s.host_pages[i] |= RELEASED;
                emptied.push(i as u64);
            }
        }
        // Their contents can go (they're zeroed again before reuse): one
        // call per run of neighbouring host pages in a segment, as a whole
        // address space goes at once when a process execs.
        emptied.sort_unstable();
        let mut i = 0;
        while i < emptied.len() {
            let first = emptied[i] * HOST_PAGE;
            let mut j = i + 1;
            while j < emptied.len() && emptied[j] == emptied[j - 1] + 1 && segment_of(emptied[j] * HOST_PAGE) == segment_of(first) {
                j += 1;
            }
            unsafe { libc::madvise(self.ptr(first).cast(), (j - i) * HOST_PAGE as usize, libc::MADV_FREE_REUSABLE) };
            i = j;
        }
    }

    /// Map the pool into `vm` (the segments reserved so far, and later ones
    /// as they are).
    pub fn map_into(&self, _vm: &Vm) -> Result<(), HvError> {
        let mut s = self.state();
        while s.mapped < s.reserved {
            self.map_segment(&mut s)?;
        }
        s.vm_live = true;
        Ok(())
    }

    /// Another hold on each of `frames` (a fork shares them).
    pub fn hold(&self, frames: &[u64]) {
        let mut s = self.state();
        for &pa in frames {
            let r = &mut s.refs[(pa / PAGE_SIZE) as usize];
            debug_assert!(*r > 0, "frame {pa:#x} held while free");
            *r += 1;
        }
    }

    /// How many address spaces hold frame `pa`.
    pub fn holders(&self, pa: u64) -> u32 {
        self.state().refs[(pa / PAGE_SIZE) as usize]
    }

    /// Frames held by anyone (for tests and diagnostics).
    pub fn frames_held(&self) -> u64 {
        let s = self.state();
        s.next_fresh / PAGE_SIZE - s.free.len() as u64
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for (k, host) in self.hosts.iter().enumerate() {
            let host = host.load(Ordering::Acquire);
            if !host.is_null() {
                unsafe { libc::munmap(host.cast(), segment(k).1 as usize) };
            }
        }
    }
}


/// An address space's frames: what it takes from the [`Pool`] and gives
/// back.
///
/// Frames come a chunk at a time and wait in a small cache, so a page fault
/// rarely takes the pool's lock. A frame that's unmapped isn't reusable at
/// once: a vCPU may still hold a stale TLB entry for it. It waits in
/// quarantine until the address space's TLB entries have been flushed
/// ([`PhysMem::release_quarantine`]), and then goes back to the pool.
pub struct PhysMem {
    pool: Arc<Pool>,
    /// Zeroed frames taken from the pool, not used yet.
    cache: Vec<u64>,
    /// Unmapped frames waiting for a TLB flush.
    quarantine: Vec<u64>,
    /// Frames in use: mapped (or page tables), or quarantined.
    in_use: u64,
}

impl PhysMem {
    pub fn new(pool: &Arc<Pool>) -> Self {
        PhysMem { pool: pool.clone(), cache: Vec::new(), quarantine: Vec::new(), in_use: 0 }
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// Host pointer to guest physical address `pa`.
    pub fn ptr(&self, pa: u64) -> *mut u8 {
        self.pool.ptr(pa)
    }

    /// A zeroed frame.
    pub fn alloc(&mut self) -> Result<u64, OutOfMemory> {
        if self.cache.is_empty() {
            self.pool.take(&mut self.cache)?;
        }
        self.in_use += 1;
        Ok(self.cache.pop().unwrap())
    }

    /// An unmapped frame: free once the TLB has been flushed.
    pub fn free(&mut self, pa: u64) {
        self.quarantine.push(pa);
    }

    /// Whether frames are waiting for a TLB flush.
    pub fn has_quarantine(&self) -> bool {
        !self.quarantine.is_empty()
    }

    /// The address space's TLB entries have been flushed everywhere:
    /// quarantined frames go back to the pool.
    pub fn release_quarantine(&mut self) {
        if self.quarantine.is_empty() {
            return;
        }
        self.pool.release(&self.quarantine);
        self.in_use -= self.quarantine.len() as u64;
        self.quarantine.clear();
    }

    /// The quarantined frames, taken out (an address space being retired:
    /// they go back once its ASID is flushed).
    pub fn take_quarantine(&mut self) -> Vec<u64> {
        self.in_use -= self.quarantine.len() as u64;
        std::mem::take(&mut self.quarantine)
    }

    pub fn read_u64(&self, pa: u64) -> u64 {
        unsafe { self.ptr(pa).cast::<u64>().read() }
    }

    pub fn write_u64(&mut self, pa: u64, v: u64) {
        unsafe { self.ptr(pa).cast::<u64>().write(v) }
    }

    /// Frames in use, quarantined ones included (for tests and diagnostics).
    pub fn frames_in_use(&self) -> u64 {
        self.in_use
    }
}

impl Drop for PhysMem {
    fn drop(&mut self) {
        // Never mapped: no TLB holds them.
        self.pool.release(&self.cache);
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

    /// The level-3 entry for `va`, if the page is mapped: where it is, and
    /// the descriptor.
    pub fn entry_of(&self, phys: &PhysMem, va: u64) -> Option<(u64, u64)> {
        let mut table = self.root;
        for level in 0..3 {
            let desc = phys.read_u64(table + index(va, level) * 8);
            if desc & DESC_VALID == 0 {
                return None;
            }
            table = desc & DESC_ADDR;
        }
        let slot = table + index(va, 3) * 8;
        let desc = phys.read_u64(slot);
        (desc & DESC_VALID != 0).then_some((slot, desc))
    }

    /// A fork's copy of these tables, made in `child_phys` into `child` (a
    /// new, empty table): the same pages, every pool frame shared copy on
    /// write (read-only and [`DESC_COW`] in both) and held once more. Shared
    /// windows' pages are mapped as they are. The caller flushes this
    /// table's address space before it runs again: its pages became
    /// read-only.
    pub fn fork_into(&self, phys: &mut PhysMem, child: &PageTable, child_phys: &mut PhysMem) -> Result<(), OutOfMemory> {
        fn copy(phys: &mut PhysMem, from: u64, child_phys: &mut PhysMem, to: u64, level: u32, held: &mut Vec<u64>) -> Result<(), OutOfMemory> {
            for i in 0..512 {
                let slot = from + i * 8;
                let desc = phys.read_u64(slot);
                if desc & DESC_VALID == 0 {
                    continue;
                }
                if level < 3 {
                    let next = child_phys.alloc()?;
                    child_phys.write_u64(to + i * 8, next | DESC_TABLE_OR_PAGE | DESC_VALID);
                    copy(phys, desc & DESC_ADDR, child_phys, next, level + 1, held)?;
                } else if desc & DESC_ADDR < POOL_SIZE {
                    let shared = desc | DESC_COW | DESC_AP_RO;
                    if shared != desc {
                        phys.write_u64(slot, shared);
                    }
                    child_phys.write_u64(to + i * 8, shared);
                    held.push(desc & DESC_ADDR);
                } else {
                    child_phys.write_u64(to + i * 8, desc);
                }
            }
            Ok(())
        }
        let mut held = Vec::new();
        let copied = copy(phys, self.root, child_phys, child.root, 0, &mut held);
        // Even if it ran out of frames part way: the child's tables hold
        // what they map, and give it back when they go.
        phys.pool().hold(&held);
        child_phys.in_use += held.len() as u64;
        copied
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

    /// Every pool frame the table holds: the pages it maps (not those in
    /// shared windows, which belong to the window) and its own tables.
    pub fn frames(&self, pool: &Pool) -> Vec<u64> {
        fn walk(pool: &Pool, table: u64, level: u32, out: &mut Vec<u64>) {
            out.push(table);
            for i in 0..512 {
                let desc = unsafe { pool.ptr(table + i * 8).cast::<u64>().read() };
                if desc & DESC_VALID == 0 {
                    continue;
                }
                let next = desc & DESC_ADDR;
                if level < 3 {
                    walk(pool, next, level + 1, out);
                } else if next < POOL_SIZE {
                    out.push(next);
                }
            }
        }
        let mut out = Vec::new();
        walk(pool, self.root, 0, &mut out);
        out
    }

    /// Unmap every page in `[start, end)`, returning pool frames to `phys`
    /// (pages in shared windows belong to the window). Returns whether
    /// anything was mapped (and so the TLB needs flushing).
    pub fn unmap_range(&self, phys: &mut PhysMem, start: u64, end: u64) -> bool {
        let mut any = false;
        self.for_each_mapped(phys, start, end, &mut |phys, _va, slot| {
            let frame = phys.read_u64(slot) & DESC_ADDR;
            phys.write_u64(slot, 0);
            if frame < POOL_SIZE {
                phys.free(frame);
            }
            any = true;
        });
        any
    }

    /// Change the attributes of every mapped page in `[start, end)`. Returns
    /// whether anything was mapped (and so the TLB needs flushing). Pages
    /// shared copy on write stay read-only (and so marked).
    pub fn protect_range(&self, phys: &mut PhysMem, start: u64, end: u64, attrs: u64) -> bool {
        let mut any = false;
        self.for_each_mapped(phys, start, end, &mut |phys, _va, slot| {
            let desc = phys.read_u64(slot);
            let cow = if desc & DESC_COW != 0 { DESC_COW | DESC_AP_RO } else { 0 };
            phys.write_u64(slot, (desc & DESC_ADDR) | attrs | cow);
            any = true;
        });
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phys() -> PhysMem {
        PhysMem::new(&Pool::new().unwrap())
    }

    #[test]
    fn map_lookup_unmap() {
        let mut phys = phys();
        let pt = PageTable::new(&mut phys).unwrap();
        let va = 0x7fff_1234_5000;
        assert_eq!(pt.lookup(&phys, va), None);
        let frame = phys.alloc().unwrap();
        pt.map(&mut phys, va, frame, user_attrs(prot::READ)).unwrap();
        assert_eq!(pt.lookup(&phys, va), Some(frame));
        assert_eq!(pt.lookup(&phys, va + PAGE_SIZE), None);
        // The root, three intermediate tables and the page.
        let mut frames = pt.frames(phys.pool());
        frames.sort();
        assert_eq!(frames.len(), 5);
        assert!(frames.contains(&frame) && frames.contains(&pt.root));
        assert!(pt.unmap_range(&mut phys, va, va + PAGE_SIZE));
        assert_eq!(pt.lookup(&phys, va), None);
        assert!(!pt.unmap_range(&mut phys, va, va + PAGE_SIZE));
    }

    #[test]
    fn recycled_frames_are_zeroed() {
        let pool = Pool::new().unwrap();
        let mut phys = PhysMem::new(&pool);
        // A whole chunk, so that it can come back as one.
        let frames: Vec<u64> = (0..CHUNK_FRAMES).map(|_| phys.alloc().unwrap()).collect();
        for &f in &frames {
            unsafe { phys.ptr(f).write_bytes(0xaa, PAGE_SIZE as usize) };
            phys.free(f);
        }
        let mut other = PhysMem::new(&pool);
        assert!(!frames.contains(&other.alloc().unwrap()), "not reused while quarantined");
        phys.release_quarantine();
        let mut third = PhysMem::new(&pool);
        let b = third.alloc().unwrap();
        assert!(frames.contains(&b), "reused once released");
        assert!(unsafe { std::slice::from_raw_parts(third.ptr(b), PAGE_SIZE as usize) }.iter().all(|&x| x == 0));
    }

    #[test]
    fn address_spaces_take_whole_chunks_and_give_them_back() {
        let pool = Pool::new().unwrap();
        let (mut a, mut b) = (PhysMem::new(&pool), PhysMem::new(&pool));
        let fa = a.alloc().unwrap();
        let fb = b.alloc().unwrap();
        let chunk = CHUNK_FRAMES as u64 * PAGE_SIZE;
        assert_ne!(fa / chunk, fb / chunk, "each takes a chunk of its own");
        assert_eq!(pool.frames_held(), 2 * CHUNK_FRAMES as u64);
        a.free(fa);
        a.release_quarantine();
        assert_eq!(a.frames_in_use(), 0);
        b.free(fb);
        b.release_quarantine();
        drop(a);
        drop(b);
        assert_eq!(pool.frames_held(), 0, "every frame back");
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
        let pool = Pool::new().unwrap();
        let mut phys = PhysMem::new(&pool);
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
        assert_ne!(rw & DESC_NG, 0, "user pages are per ASID");
        assert_eq!(kernel_code_attrs() & DESC_NG, 0, "the kernel half is global");
    }
}
