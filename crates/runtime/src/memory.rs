//! A Linux process's address space: mapped regions ([`Vmas`]), the page
//! tables that implement them, its ASID, and the program break — plus what
//! every address space of a VM shares ([`Machine`]): the frame pool, and the
//! kernel half, which holds only the exception vectors.
//!
//! ```text
//! 0   64 KiB          brk ↑        ↓ mmap   ↓ stack   2^48       kernel half (TTBR1)
//! | ✗ | ELF image | heap |  ...  | mappings | stack | ✗ ... | vectors (EL1 only) |
//! ```
//!
//! Regions are the source of truth; page-table entries are filled in lazily,
//! when the guest first touches a page (a translation fault exits to the host,
//! see [`AddressSpace::handle_fault`]), a few pages at a time. Private regions
//! get frames from the pool; shared ones (`MAP_SHARED`) point into a host
//! shared mapping placed in guest physical memory ([`crate::shared`]).
//!
//! Each address space has its own ASID (`TTBR0_EL1[63:48]`, with
//! `TCR_EL1.AS`), and user pages are not global, so the TLB keeps every
//! address space's entries apart: a vCPU can move between address spaces by
//! writing `TTBR0_EL1`, and flushes after a change are by ASID
//! (docs/processes.md, section 2).

use std::{
    collections::BTreeSet,
    os::fd::{OwnedFd, RawFd},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use linux_abi::{
    abi::mm,
    errno,
    wire::{Decoder, Encoder, Truncated},
    Fault, GuestMemory, SharedKey,
};

use crate::{
    hv::{self, HvError, Vcpu, Vm},
    mach::MemoryEntry,
    paging::{self, prot, OutOfMemory, PageTable, PhysMem, Pool, PAGE_SIZE, POOL_SIZE},
    shared::SharedWindows,
    vma::{Backing, Vma, Vmas},
};

/// User addresses are 48 bits, as on Linux arm64.
pub const USER_TOP: u64 = 1 << 48;
/// Linux's `mmap_min_addr`: nothing maps below this, so null dereferences fault.
pub const MIN_ADDR: u64 = 0x10000;
pub const STACK_TOP: u64 = USER_TOP - 0x10000;
pub const STACK_SIZE: u64 = 8 << 20;
/// Mappings without an address are placed top-down from here, leaving room
/// for the stack (Linux uses the same 128 MiB minimum gap).
const MMAP_BASE: u64 = STACK_TOP - (128 << 20);
/// How much to map on one fault (in the same region, aligned to this size), to
/// cut down on VM exits when touching memory sequentially.
const FAULT_AROUND: u64 = 16 * PAGE_SIZE;

/// The exception vectors, mapped EL1-only in the kernel half.
pub const VECTORS_VA: u64 = 0xffff_ffff_fff0_0000;
/// The vector table index for a synchronous exception from EL0 (AArch64).
pub const VECTOR_EL0_SYNC: u64 = 8;
/// In that vector, the path that flushes the running address space's TLB
/// entries (its ASID's, on every vCPU) before returning to EL0. The host
/// resumes the guest here instead of at the plain `eret` after changing
/// page-table entries the guest may have cached.
pub const VECTOR_FLUSH_PATH: u64 = VECTORS_VA + VECTOR_EL0_SYNC * 0x80 + 8;
/// After the vector table (16 entries of 0x80 bytes), in the same page: flush
/// the TLB entries of the ASID in x0 (bits 63:48, as `TLBI ASIDE1IS` takes
/// it) on every vCPU, then `eret`. Run from EL1, for an address space that's
/// gone.
pub const FLUSH_ASID_PATH: u64 = VECTORS_VA + 0x800;
/// ASIDs are 16 bits (`TCR_EL1.AS`); 0 is never an address space's.
const ASID_LIMIT: u64 = 1 << 16;

/// What every address space of a VM shares: the frame pool, the kernel half
/// (one `TTBR1_EL1` per VM: the vectors, global), the ASIDs, and the shared
/// windows.
pub struct Machine {
    pool: Arc<Pool>,
    kernel: PageTable,
    /// Holds the kernel half's frames, for as long as the VM.
    _kernel_frames: Mutex<PhysMem>,
    asids: Mutex<Asids>,
    reclaim: Arc<Reclaim>,
    /// Shared memory placed above the pool, counted by the address spaces
    /// using each window.
    windows: Mutex<SharedWindows>,
}

/// Address spaces whose ASID has been flushed, waiting for a thread of
/// their own to give their frames back. Finding the frames means walking
/// the page tables (0.14 ms for 200 MB, most of it the host faulting in
/// table pages a forked child never touched) and giving them back as much
/// again: on the `execve` path, that would make an exec from a big forked
/// process (a shell's `posix_spawn`) 0.3 ms slower than when the old address
/// space was a host mapping dropped whole.
#[derive(Default)]
struct Reclaim {
    state: Mutex<ReclaimState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct ReclaimState {
    queue: Vec<Retired>,
    /// The thread is giving some back now.
    busy: bool,
    /// It has been started.
    running: bool,
    /// The machine has gone: the thread ends.
    closed: bool,
}

impl Reclaim {
    fn state(&self) -> MutexGuard<'_, ReclaimState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The thread: give back what's queued, until the machine goes.
    fn run(&self, pool: &Pool) {
        let mut s = self.state();
        loop {
            if let Some(retired) = s.queue.pop() {
                s.busy = true;
                drop(s);
                pool.release(&retired.frames(pool));
                s = self.state();
                s.busy = false;
                self.changed.notify_all();
            } else if s.closed {
                return;
            } else {
                s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.reclaim.state().closed = true;
        self.reclaim.changed.notify_all();
    }
}

struct Asids {
    /// ASIDs at or above this have never been handed out.
    next: u64,
    /// Free ASIDs, flushed from every TLB.
    free: Vec<u64>,
    /// Address spaces gone whose ASID hasn't been flushed yet: neither it
    /// nor their frames can be reused until it has.
    retired: Vec<Retired>,
}

impl Default for Asids {
    fn default() -> Self {
        Asids { next: 1, free: Vec::new(), retired: Vec::new() }
    }
}

/// An address space that has gone, waiting for its ASID to be flushed.
struct Retired {
    asid: u64,
    /// Its page tables, as they were: walked for its frames only when they
    /// can go back (not at all if the VM goes first, as when a process
    /// exits).
    user: PageTable,
    quarantine: Vec<u64>,
}

impl Retired {
    fn frames(&self, pool: &Pool) -> Vec<u64> {
        let mut frames = self.user.frames(pool);
        frames.extend_from_slice(&self.quarantine);
        frames
    }
}

impl Machine {
    pub fn new() -> std::io::Result<Arc<Machine>> {
        let pool = Pool::new()?;
        let oom = |_| std::io::Error::from(std::io::ErrorKind::OutOfMemory);
        let mut phys = PhysMem::new(&pool);
        let kernel = PageTable::new(&mut phys).map_err(oom)?;
        let vectors = phys.alloc().map_err(oom)?;
        write_vectors(&mut phys, vectors);
        kernel.map(&mut phys, VECTORS_VA, vectors, paging::kernel_code_attrs()).map_err(oom)?;
        Ok(Arc::new(Machine {
            pool,
            kernel,
            _kernel_frames: Mutex::new(phys),
            asids: Mutex::default(),
            reclaim: Arc::default(),
            windows: Mutex::default(),
        }))
    }

    /// A forked child's, from the parent's snapshot (see
    /// [`AddressSpace::encode_for_fork`]): the same pool contents and kernel
    /// half, and no ASIDs in use.
    fn restore(entries: &[MemoryEntry], d: &mut Decoder) -> Result<Arc<Machine>, Truncated> {
        let pool = Pool::restore(entries, d)?;
        let kernel = PageTable { root: d.u64()? };
        let phys = PhysMem::new(&pool);
        Ok(Arc::new(Machine {
            pool,
            kernel,
            _kernel_frames: Mutex::new(phys),
            asids: Mutex::default(),
            reclaim: Arc::default(),
            windows: Mutex::default(),
        }))
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// Map guest physical memory into `vm`: the pool, and shared windows (now
    /// and as they're created). A host serving clients does this before it
    /// has any process.
    pub fn map_into(&self, vm: &Vm) -> Result<(), HvError> {
        self.pool.map_into(vm)?;
        self.windows().attach_vm()
    }

    fn asids(&self) -> MutexGuard<'_, Asids> {
        self.asids.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn windows(&self) -> MutexGuard<'_, SharedWindows> {
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The ASIDs of address spaces that ran and have gone, still to be
    /// flushed (by any vCPU: `tlbi aside1is` reaches them all), after which
    /// [`Machine::asid_flushed`] gives them and their frames back. With many
    /// processes in one VM, CPUs look here between tasks.
    pub fn unflushed_asids(&self) -> Vec<u64> {
        self.asids().retired.iter().map(|r| r.asid).collect()
    }

    /// Whether any address space waits for its ASID's flush (cheap enough
    /// to ask between tasks).
    pub fn any_unflushed(&self) -> bool {
        !self.asids().retired.is_empty()
    }

    fn alloc_asid(&self) -> Result<u64, OutOfMemory> {
        let mut a = self.asids();
        if let Some(asid) = a.free.pop() {
            return Ok(asid);
        }
        // 65535 address spaces alive at once, before Linux's way past that
        // (a new generation, every TLB flushed, ASIDs handed out again) is
        // needed.
        if a.next >= ASID_LIMIT {
            return Err(OutOfMemory);
        }
        a.next += 1;
        Ok(a.next - 1)
    }

    /// An address space has gone: its ASID and frames wait until the ASID
    /// is flushed ([`Machine::asid_flushed`]), or are free at once if it
    /// never ran.
    fn retire(&self, retired: Retired, ran: bool) {
        let mut a = self.asids();
        if ran {
            a.retired.push(retired);
        } else {
            self.pool.release(&retired.frames(&self.pool));
            a.free.push(retired.asid);
        }
    }

    /// The ASID `asid` has been flushed from every TLB (`tlbi aside1is`): it
    /// can be used again at once, and the frames of the address space that
    /// had it go back to the pool on the reclaiming thread.
    pub fn asid_flushed(&self, asid: u64) {
        let retired = {
            let mut a = self.asids();
            let Some(i) = a.retired.iter().position(|r| r.asid == asid) else { return };
            a.free.push(asid);
            a.retired.swap_remove(i)
        };
        let mut s = self.reclaim.state();
        s.queue.push(retired);
        if !s.running {
            s.running = true;
            let (reclaim, pool) = (self.reclaim.clone(), self.pool.clone());
            if linux_abi::host_threads::spawn("reclaim", move || reclaim.run(&pool)).is_err() {
                // No thread to spare: give the frames back here instead.
                s.running = false;
                let queued = std::mem::take(&mut s.queue);
                drop(s);
                for retired in queued {
                    self.pool.release(&retired.frames(&self.pool));
                }
                self.reclaim.changed.notify_all();
                return;
            }
        }
        self.reclaim.changed.notify_all();
    }

    /// Wait until the reclaiming thread has given back everything queued,
    /// so the pool's free list is whole (a fork's snapshot of it).
    fn reclaimed(&self) {
        let mut s = self.reclaim.state();
        while !s.queue.is_empty() || s.busy {
            s = self.reclaim.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Frames of address spaces gone whose ASIDs haven't been flushed.
    fn retired_frames(&self) -> Vec<u64> {
        self.asids().retired.iter().flat_map(|r| r.frames(&self.pool)).collect()
    }
}

/// Whether a vCPU about to execute `pc` is inside the flush path's stretch
/// that keeps the guest's x0 in `TPIDR_EL1` (from after the `msr` that saves
/// it to the `mrs` that restores it, see [`write_vectors`]).
pub fn flush_path_borrows_x0(pc: u64) -> bool {
    (VECTOR_FLUSH_PATH + 2 * 4..=VECTOR_FLUSH_PATH + 8 * 4).contains(&pc)
}

/// Whether a vCPU about to execute `pc` is at the start of a vector entry:
/// it has taken the exception, and the entry's `hvc` to the host is next.
pub fn at_vector_entry(pc: u64) -> bool {
    (VECTORS_VA..VECTORS_VA + 16 * 0x80).contains(&pc) && (pc - VECTORS_VA).is_multiple_of(0x80)
}

/// The ASID size this CPU supports, in bits, once a vCPU has said: 8 or 16.
static ASID_BITS: AtomicU8 = AtomicU8::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
}

fn allowed(p: u64, access: Access) -> bool {
    match access {
        // arm64 can't express write-only or execute-only for EL0.
        Access::Read => p & (prot::READ | prot::WRITE | prot::EXEC) != 0,
        Access::Write => p & prot::WRITE != 0,
        Access::Exec => p & prot::EXEC != 0,
    }
}

fn page_down(a: u64) -> u64 {
    a & !(PAGE_SIZE - 1)
}

fn page_up(a: u64) -> Option<u64> {
    a.checked_next_multiple_of(PAGE_SIZE)
}

/// Address spaces made so far: each gets the next number, which vCPUs keep
/// to know which one they're set up for (an ASID or a page-table root can
/// come back, once its address space has gone; this never does).
static SERIAL: AtomicU64 = AtomicU64::new(1);

pub struct AddressSpace {
    machine: Arc<Machine>,
    phys: PhysMem,
    /// The machine's shared windows it uses (by start).
    windows: BTreeSet<u64>,
    user: PageTable,
    asid: u64,
    serial: u64,
    /// A vCPU has run with it, so the TLB may hold its entries: when it
    /// goes, its ASID must be flushed before its frames are reused.
    ran: bool,
    vmas: Vmas,
    brk_start: u64,
    brk: u64,
    /// Page-table entries the guest may have cached changed; flush the TLB
    /// before it runs again.
    tlb_stale: bool,
    /// Pages shared copy on write that it has made its own, so far.
    cow_breaks: u64,
}

impl AddressSpace {
    pub fn new(machine: &Arc<Machine>) -> std::io::Result<Self> {
        let oom = |_| std::io::Error::from(std::io::ErrorKind::OutOfMemory);
        let mut phys = PhysMem::new(machine.pool());
        let user = PageTable::new(&mut phys).map_err(oom)?;
        let asid = machine.alloc_asid().map_err(oom)?;
        let mut space = AddressSpace {
            machine: machine.clone(),
            phys,
            windows: BTreeSet::new(),
            user,
            asid,
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            ran: false,
            vmas: Vmas::default(),
            brk_start: MIN_ADDR,
            brk: MIN_ADDR,
            tlb_stale: false,
            cow_breaks: 0,
        };
        // Where signal handlers without their own return path return to.
        let tramp = crate::signal::TRAMPOLINE_VA;
        space.vmas.insert(tramp, tramp + PAGE_SIZE, prot::READ | prot::EXEC, Backing::Private);
        let code: Vec<u8> = crate::signal::TRAMPOLINE.iter().flat_map(|i| i.to_le_bytes()).collect();
        space.poke(tramp, &code).map_err(|_| oom(OutOfMemory))?;
        Ok(space)
    }

    /// The VM-wide memory this address space is part of.
    pub fn machine(&self) -> &Arc<Machine> {
        &self.machine
    }

    pub fn asid(&self) -> u64 {
        self.asid
    }

    /// How many pages shared copy on write it has made its own (see
    /// [`AddressSpace::handle_fault`]).
    pub fn cow_breaks(&self) -> u64 {
        self.cow_breaks
    }

    /// Its number: unique for the life of the host process.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// A copy-on-write fork of this address space in the same pool (one VM
    /// for both processes): the same regions, break and windows; its own
    /// page tables and ASID; every private frame shared until one side
    /// writes it. This one's pages become read-only, so its TLB entries
    /// must be flushed before it runs again ([`AddressSpace::needs_flush`]
    /// says so).
    pub fn fork(&mut self) -> Result<AddressSpace, OutOfMemory> {
        let mut phys = PhysMem::new(self.machine.pool());
        let user = PageTable::new(&mut phys)?;
        let asid = self.machine.alloc_asid()?;
        {
            let mut windows = self.machine.windows();
            for &w in &self.windows {
                windows.add_user(w);
            }
        }
        let mut child = AddressSpace {
            machine: self.machine.clone(),
            phys,
            windows: self.windows.clone(),
            user,
            asid,
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            ran: false,
            vmas: self.vmas.clone(),
            brk_start: self.brk_start,
            brk: self.brk,
            tlb_stale: false,
            cow_breaks: 0,
        };
        self.tlb_stale = true;
        self.user.fork_into(&mut self.phys, &child.user, &mut child.phys)?;
        Ok(child)
    }

    /// A write to a page of `vma` (mapped by the entry at `slot`, `desc`)
    /// that's shared copy on write: the page becomes this address space's
    /// own, a copy unless no one else holds the frame any more. Returns its
    /// frame.
    fn break_cow(&mut self, slot: u64, desc: u64, vma: Vma) -> Result<u64, OutOfMemory> {
        let frame = desc & paging::DESC_ADDR;
        let pa = if self.phys.pool().holders(frame) == 1 {
            frame
        } else {
            let copy = self.phys.alloc()?;
            unsafe { std::ptr::copy_nonoverlapping(self.phys.ptr(frame), self.phys.ptr(copy), PAGE_SIZE as usize) };
            // Its hold goes once this address space's TLB entries for it
            // have.
            self.phys.free(frame);
            copy
        };
        self.phys.write_u64(slot, pa | page_attrs(vma));
        self.tlb_stale = true;
        self.cow_breaks += 1;
        Ok(pa)
    }

    /// Everything a forked child needs to rebuild this address space:
    /// copy-on-write snapshots of the pool (one per segment in use), the
    /// layout (encoded into `e`), and the shared windows' fds, to map the
    /// same memory. Other threads may keep running.
    pub fn encode_for_fork(&self, e: &mut Encoder) -> std::io::Result<(Vec<MemoryEntry>, Vec<RawFd>)> {
        let pool = self.machine.pool();
        // Frames an exec left behind are free in the child only once they're
        // on the free list.
        self.machine.reclaimed();
        let entries = pool.snapshot()?;
        let also_free: Vec<u64> = self.phys.unmapped().chain(self.machine.retired_frames()).collect();
        pool.encode(e, &also_free);
        e.u64(self.machine.kernel.root);
        e.u64(self.user.root).u64(self.brk_start).u64(self.brk);
        let vmas: Vec<_> = self.vmas.iter().collect();
        e.u64(vmas.len() as u64);
        for (start, v) in vmas {
            e.u64(start).u64(v.end).u64(v.prot);
            match v.backing {
                Backing::Private => e.u64(0),
                Backing::Shared { ipa } => e.u64(1).u64(ipa),
                Backing::File { ipa } => e.u64(2).u64(ipa),
            };
        }
        let fds = self.machine.windows().encode(e, self.windows.iter());
        Ok((entries, fds))
    }

    /// The child's side of [`AddressSpace::encode_for_fork`].
    pub fn decode_forked(d: &mut Decoder, entries: &[MemoryEntry], fds: &mut impl Iterator<Item = OwnedFd>) -> Result<Self, Truncated> {
        let machine = Machine::restore(entries, d)?;
        let user = PageTable { root: d.u64()? };
        let (brk_start, brk) = (d.u64()?, d.u64()?);
        let mut vmas = Vmas::default();
        for _ in 0..d.count(32)? {
            let (start, end, prot) = (d.u64()?, d.u64()?, d.u64()?);
            let backing = match d.u64()? {
                0 => Backing::Private,
                1 => Backing::Shared { ipa: d.u64()? },
                2 => Backing::File { ipa: d.u64()? },
                _ => return Err(Truncated),
            };
            vmas.insert(start, end, prot, backing);
        }
        let windows = machine.windows().decode(d, fds)?.into_iter().collect();
        let asid = machine.alloc_asid().map_err(|_| Truncated)?;
        let phys = PhysMem::new(machine.pool());
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        Ok(AddressSpace { machine, phys, windows, user, asid, serial, ran: false, vmas, brk_start, brk, tlb_stale: false, cow_breaks: 0 })
    }

    /// Map guest physical memory into `vm`: the pool, and shared windows (now
    /// and as they're created).
    pub fn map_into(&mut self, vm: &Vm) -> Result<(), HvError> {
        self.machine.map_into(vm)
    }

    /// Take guest physical memory out of `vm`, before it goes.
    pub fn unmap_from(&mut self, _vm: &Vm) -> Result<(), HvError> {
        self.machine.pool().unmap_from_vm()?;
        self.machine.windows().detach_vm()
    }

    /// Point a vCPU set up for another address space of the same VM (by
    /// [`AddressSpace::configure_vcpu`]) at this one: only `TTBR0_EL1`
    /// differs, and with ASIDs neither's TLB entries need flushing.
    pub fn switch_vcpu(&mut self, vcpu: &mut Vcpu) -> Result<(), HvError> {
        vcpu.set_sys_reg(hv::sys_reg::TTBR0_EL1, self.user.root | self.asid << 48)?;
        self.ran = true;
        Ok(())
    }

    /// Point the vCPU at our page tables (with our ASID) and the VM's
    /// vectors, and turn the MMU on.
    pub fn configure_vcpu(&mut self, vcpu: &mut Vcpu) -> Result<(), HvError> {
        use hv::sys_reg::*;
        if ASID_BITS.load(Ordering::Relaxed) == 0 {
            // ID_AA64MMFR0_EL1.ASIDBits: 0b0010 for 16 bits.
            let bits = if (vcpu.sys_reg(ID_AA64MMFR0_EL1)? >> 4) & 0xf == 0b0010 { 16 } else { 8 };
            ASID_BITS.store(bits, Ordering::Relaxed);
        }
        assert!(self.asid < 1 << ASID_BITS.load(Ordering::Relaxed), "ASID {} too big for this CPU", self.asid);
        // Attr0: normal memory, write-back read/write-allocate.
        vcpu.set_sys_reg(MAIR_EL1, 0xff)?;
        let tcr = 16           // T0SZ: 48-bit user VAs
            | (0b01 << 8)      // IRGN0: write-back
            | (0b01 << 10)     // ORGN0: write-back
            | (0b11 << 12)     // SH0: inner shareable (TG0 = 0: 4 KiB granule)
            | (16 << 16)       // T1SZ: 48-bit kernel VAs
            | (0b01 << 24)     // IRGN1
            | (0b01 << 26)     // ORGN1
            | (0b11 << 28)     // SH1
            | (0b10 << 30)     // TG1: 4 KiB granule
            | (0b010 << 32)    // IPS: 40-bit physical addresses (the VM's IPA size)
            | (1 << 36)        // AS: 16-bit ASIDs (A1 = 0: TTBR0_EL1's)
            | (1 << 37);       // TBI0: top byte ignored for user pointers, as on Linux
        vcpu.set_sys_reg(TCR_EL1, tcr)?;
        vcpu.set_sys_reg(TTBR0_EL1, self.user.root | self.asid << 48)?;
        vcpu.set_sys_reg(TTBR1_EL1, self.machine.kernel.root)?;
        // FPEN = 0b11: FP/SIMD usable at EL0 (musl's memcpy uses NEON).
        vcpu.set_sys_reg(CPACR_EL1, 0b11 << 20)?;
        vcpu.set_sys_reg(VBAR_EL1, VECTORS_VA)?;
        // EL0VCTEN: EL0 may read the virtual counter and its frequency
        // (CNTVCT_EL0, CNTFRQ_EL0), as Linux allows for its vDSO. Windows
        // programs under Wine read them directly.
        vcpu.set_sys_reg(CNTKCTL_EL1, 1 << 1)?;
        let sctlr = 0x30d0_0800 // RES1 bits
            | (1 << 0)   // M: MMU on
            | (1 << 2)   // C: data cacheable
            | (1 << 12)  // I: instruction cacheable
            | (1 << 14)  // DZE: EL0 may use DC ZVA
            | (1 << 15)  // UCT: EL0 may read CTR_EL0
            | (1 << 26); // UCI: EL0 cache maintenance
        vcpu.set_sys_reg(SCTLR_EL1, sctlr)?;
        self.ran = true;
        Ok(())
    }

    /// Whether the TLB must be flushed before the guest runs again (and clear
    /// the flag).
    pub fn take_tlb_stale(&mut self) -> bool {
        std::mem::take(&mut self.tlb_stale)
    }

    /// Whether [`AddressSpace::take_tlb_stale`] would say so, left as it is.
    pub fn tlb_stale(&self) -> bool {
        self.tlb_stale
    }

    /// Whether a system call changed mappings such that this address
    /// space's TLB entries must be flushed (on every vCPU) before it
    /// returns (and unmapped frames can be reused).
    pub fn needs_flush(&self) -> bool {
        self.tlb_stale || self.phys.has_quarantine()
    }

    /// The address space's TLB entries have been flushed (on every vCPU:
    /// `tlbi aside1is` reaches them all).
    pub fn flushed(&mut self) {
        self.tlb_stale = false;
        self.phys.release_quarantine();
    }

    pub fn frames_in_use(&self) -> u64 {
        self.phys.frames_in_use()
    }
    /// Map an ELF segment's pages, `[start, end)` page-aligned. A first page
    /// shared with the previous segment gets both segments' permissions.
    pub fn map_segment(&mut self, start: u64, end: u64, p: u64) -> Result<(), u64> {
        if start < MIN_ADDR || end > USER_TOP || start >= end {
            return Err(start);
        }
        let mut from = start;
        if let Some((_, vma)) = self.vmas.find(start) {
            self.vmas.protect(start, start + PAGE_SIZE, vma.prot | p);
            from += PAGE_SIZE;
        }
        if from < end {
            if !self.vmas.is_free(from, end) {
                return Err(from);
            }
            self.vmas.insert(from, end, p, Backing::Private);
        }
        Ok(())
    }

    /// Where an `len`-byte mapping with no address would go (the loader puts
    /// the dynamic linker here, as Linux does).
    pub fn find_free(&self, len: u64) -> Option<u64> {
        self.vmas.find_free_top_down(len, MIN_ADDR, MMAP_BASE)
    }

    /// Map the initial stack, below [`STACK_TOP`].
    pub fn map_stack(&mut self) {
        self.vmas.insert(STACK_TOP - STACK_SIZE, STACK_TOP, prot::READ | prot::WRITE, Backing::Private);
    }

    /// Put the program break right after the loaded image.
    pub fn set_brk_start(&mut self, addr: u64) {
        self.brk_start = page_up(addr).unwrap();
        self.brk = self.brk_start;
    }

    /// Copy `data` into mapped memory regardless of its protection (for the
    /// loader, which fills read-only segments).
    pub fn poke(&mut self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        self.copy_in(addr, data, None)
    }

    /// The guest faulted on `va` (a translation or permission fault). If the
    /// region allows `access`, map the page (and its neighbours) and return
    /// true so the guest retries. False means a real fault (SIGSEGV).
    pub fn handle_fault(&mut self, va: u64, access: Access) -> Result<bool, OutOfMemory> {
        let va = va & (USER_TOP - 1); // drop the pointer tag (TBI0)
        let Some((start, vma)) = self.vmas.find(va) else { return Ok(false) };
        if !allowed(vma.prot, access) {
            return Ok(false);
        }
        let page = page_down(va);
        if access == Access::Write && matches!(vma.backing, Backing::File { .. }) {
            // The first write to a page of a private file mapping: from now
            // on the page is this process's own copy, as on Linux.
            self.privatize(page, page + PAGE_SIZE).map_err(|_| OutOfMemory)?;
            return Ok(true);
        }
        if let Some((slot, desc)) = self.user.entry_of(&self.phys, page) {
            // A write to a page shared with a fork: now it's ours.
            if access == Access::Write && desc & paging::DESC_COW != 0 {
                self.break_cow(slot, desc, vma)?;
                return Ok(true);
            }
            // Mapped and allowed, so the guest saw a stale TLB entry.
            self.tlb_stale = true;
            return Ok(true);
        }
        let window = va & !(FAULT_AROUND - 1);
        let lo = window.max(start);
        let hi = (window + FAULT_AROUND).min(vma.end);
        for p in (lo..hi).step_by(PAGE_SIZE as usize) {
            if self.user.lookup(&self.phys, p).is_none() {
                self.populate(p, start, vma)?;
            }
        }
        Ok(true)
    }

    /// Map `page` (in `vma`, which starts at `start`) and return its
    /// physical address: a new frame, or its place in a shared window.
    fn populate(&mut self, page: u64, start: u64, vma: Vma) -> Result<u64, OutOfMemory> {
        let pa = match vma.shared_pa(start, page) {
            Some(pa) => pa,
            None => self.phys.alloc()?,
        };
        self.user.map(&mut self.phys, page, pa, page_attrs(vma))?;
        Ok(pa)
    }

    /// Host pointer to guest physical address `pa`. (A window's stays valid
    /// while this address space uses the window.)
    fn ptr(&self, pa: u64) -> *mut u8 {
        if pa < POOL_SIZE {
            self.phys.ptr(pa)
        } else {
            self.machine.windows().ptr(pa).expect("physical address outside any window")
        }
    }

    /// The physical address of the page holding `va`, to be written, as a
    /// write would leave it: populated, this address space's own if it was
    /// shared copy on write, a private copy if it was read through a
    /// file's window. Checks `access` against the region (None: the
    /// loader, which may write anything mapped).
    fn page_to_write(&mut self, va: u64, access: Option<Access>) -> Result<u64, Fault> {
        loop {
            let (start, vma) = self.vmas.find(va).ok_or(Fault { addr: va })?;
            if access.is_some_and(|a| !allowed(vma.prot, a)) {
                return Err(Fault { addr: va });
            }
            let page = page_down(va);
            if matches!(vma.backing, Backing::File { .. }) {
                // Never write through to the file: copy the page first.
                self.privatize(page, page + PAGE_SIZE)?;
                continue;
            }
            return match (vma.shared_pa(start, page), self.user.entry_of(&self.phys, page)) {
                (Some(pa), _) => Ok(pa),
                // Shared with a fork: written, it becomes ours.
                (None, Some((slot, desc))) if desc & paging::DESC_COW != 0 => self.break_cow(slot, desc, vma).map_err(|_| Fault { addr: va }),
                (None, Some((_, desc))) => Ok(desc & paging::DESC_ADDR),
                (None, None) => self.populate(page, start, vma).map_err(|_| Fault { addr: va }),
            };
        }
    }

    /// Copy into guest memory, checking `access` against the regions (None:
    /// the loader, which may write anything mapped).
    fn copy_in(&mut self, addr: u64, data: &[u8], access: Option<Access>) -> Result<(), Fault> {
        let mut done = 0;
        while done < data.len() {
            let va = addr.checked_add(done as u64).ok_or(Fault { addr })?;
            let pa = self.page_to_write(va, access)?;
            let page = page_down(va);
            let off = va - page;
            let n = ((PAGE_SIZE - off) as usize).min(data.len() - done);
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), self.ptr(pa + off), n) };
            done += n;
        }
        Ok(())
    }

    /// `madvise(DONTNEED)`: the private pages in `[addr, addr + len)` go (and
    /// read as zeros when touched again); the regions stay.
    pub fn discard(&mut self, addr: u64, len: u64) -> i64 {
        let Some(end) = addr.checked_add(len).filter(|&e| e <= USER_TOP) else { return -errno::EINVAL };
        if !self.vmas.covers(addr, end) {
            return -errno::ENOMEM;
        }
        let private: Vec<(u64, u64)> = self
            .vmas
            .overlapping(addr, end)
            .filter(|(_, v)| v.backing == Backing::Private)
            .map(|(s, v)| (s.max(addr), v.end.min(end)))
            .collect();
        for (s, e) in private {
            if self.user.unmap_range(&mut self.phys, s, e) {
                self.tlb_stale = true;
            }
        }
        0
    }

    /// Remove `[start, end)` from the regions and page tables, and release
    /// shared windows no region uses any more.
    fn unmap(&mut self, start: u64, end: u64) {
        let shown = self.windows_shown(start, end);
        self.vmas.remove(start, end);
        if self.user.unmap_range(&mut self.phys, start, end) {
            self.tlb_stale = true;
        }
        self.release_unused_windows(shown);
    }

    /// The shared windows the regions in `[start, end)` show (as (start,
    /// end)): the only ones that removing those regions can leave unused.
    fn windows_shown(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let windows = self.machine.windows();
        let mut shown: Vec<(u64, u64)> = self
            .vmas
            .overlapping(start, end)
            .filter_map(|(s, v)| match v.backing {
                Backing::Shared { ipa } | Backing::File { ipa } => windows.find(ipa).or_else(|| windows.find(ipa + (v.end - s) - 1)),
                Backing::Private => None,
            })
            .collect();
        shown.sort_unstable();
        shown.dedup();
        shown
    }

    /// Of `candidates` (windows whose regions went), release those no
    /// region shows any more. (Only those: looking at every window against
    /// every region, on each unmap and each private file page's first
    /// write, was a Wine process's busiest work in the runtime.)
    fn release_unused_windows(&mut self, candidates: Vec<(u64, u64)>) {
        for (ws, we) in candidates {
            if !self.windows.contains(&ws) {
                continue;
            }
            let used = self.vmas.iter().any(|(s, v)| match v.backing {
                Backing::Shared { ipa } | Backing::File { ipa } => ipa < we && ipa + (v.end - s) > ws,
                Backing::Private => false,
            });
            if !used {
                self.windows.remove(&ws);
                self.machine.windows().release(ws);
            }
        }
    }

    /// It uses `window`, which [`SharedWindows::create`] counted it a user
    /// of: once is enough (a second mapping of something it maps already).
    fn use_window(&mut self, window: u64) {
        if !self.windows.insert(window) {
            self.machine.windows().release(window);
        }
    }

    /// The shared windows it uses, as (start, end).
    pub fn window_ranges(&self) -> Vec<(u64, u64)> {
        let windows = self.machine.windows();
        self.windows.iter().filter_map(|&w| windows.find(w)).collect()
    }

    /// Choose where a new `len`-byte mapping goes (for MAP_FIXED, clearing
    /// whatever is there), or return a negative errno.
    fn place(&mut self, hint: u64, len: u64, flags: u64) -> Result<u64, i64> {
        if flags & (mm::MAP_FIXED | mm::MAP_FIXED_NOREPLACE) != 0 {
            if !hint.is_multiple_of(PAGE_SIZE) {
                return Err(-errno::EINVAL);
            }
            if hint < MIN_ADDR || hint.checked_add(len).is_none_or(|end| end > USER_TOP) {
                return Err(-errno::ENOMEM);
            }
            if !self.vmas.is_free(hint, hint + len) {
                if flags & mm::MAP_FIXED_NOREPLACE != 0 {
                    return Err(-errno::EEXIST);
                }
                self.unmap(hint, hint + len);
            }
            return Ok(hint);
        }
        // A hint is used if that range is free, as on Linux.
        let hint = page_down(hint);
        if hint >= MIN_ADDR && hint.checked_add(len).is_some_and(|end| end <= USER_TOP && self.vmas.is_free(hint, end)) {
            return Ok(hint);
        }
        self.vmas.find_free_top_down(len, MIN_ADDR, MMAP_BASE).ok_or(-errno::ENOMEM)
    }
}

/// The memory operations system calls use; [`Space`] offers them to the
/// dispatcher, from any thread.
impl AddressSpace {
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        let mut done = 0;
        while done < buf.len() {
            let va = addr.checked_add(done as u64).ok_or(Fault { addr })?;
            let (start, vma) = self.vmas.find(va).ok_or(Fault { addr: va })?;
            if !allowed(vma.prot, Access::Read) {
                return Err(Fault { addr: va });
            }
            let page = page_down(va);
            let off = va - page;
            let n = ((PAGE_SIZE - off) as usize).min(buf.len() - done);
            // Reading needs no mutation: a private page not populated yet
            // reads as zero.
            match vma.shared_pa(start, page).or_else(|| self.user.lookup(&self.phys, page)) {
                Some(pa) => unsafe { std::ptr::copy_nonoverlapping(self.ptr(pa + off), buf[done..].as_mut_ptr(), n) },
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        self.copy_in(addr, data, Some(Access::Write))
    }

    pub fn brk(&mut self, addr: u64) -> u64 {
        if addr < self.brk_start {
            return self.brk;
        }
        let (Some(old_end), Some(new_end)) = (page_up(self.brk), page_up(addr)) else { return self.brk };
        if new_end > old_end {
            if new_end > USER_TOP || !self.vmas.is_free(old_end, new_end) {
                return self.brk;
            }
            self.vmas.insert(old_end, new_end, prot::READ | prot::WRITE, Backing::Private);
        } else if new_end < old_end {
            self.unmap(new_end, old_end);
        }
        self.brk = addr;
        addr
    }

    pub fn mmap_anonymous(&mut self, hint: u64, len: u64, p: u64, flags: u64) -> i64 {
        let Some(len) = page_up(len).filter(|&l| l > 0 && l <= USER_TOP) else { return -errno::ENOMEM };
        let p = p & (prot::READ | prot::WRITE | prot::EXEC); // drop PROT_BTI/PROT_MTE
        match self.place(hint, len, flags) {
            Ok(addr) => {
                self.vmas.insert(addr, addr + len, p, Backing::Private);
                addr as i64
            }
            Err(e) => e,
        }
    }

    pub fn mmap_shared(&mut self, hint: u64, len: u64, p: u64, flags: u64, host_fd: Option<RawFd>, offset: u64) -> i64 {
        let Some(len) = page_up(len).filter(|&l| l > 0 && l <= USER_TOP) else { return -errno::ENOMEM };
        let p = p & (prot::READ | prot::WRITE | prot::EXEC);
        // Like Linux: the fd must be readable, and writable for PROT_WRITE.
        let writable = match host_fd {
            None => true,
            Some(fd) => match unsafe { libc::fcntl(fd, libc::F_GETFL) } {
                -1 => return -errno::EBADF,
                fl if fl & libc::O_ACCMODE == libc::O_RDWR => true,
                fl if fl & libc::O_ACCMODE == libc::O_RDONLY => false,
                _ => return -errno::EACCES,
            },
        };
        if p & prot::WRITE != 0 && !writable {
            return -errno::EACCES;
        }
        // Placed first: making room (MAP_FIXED) releases unused windows.
        let addr = match self.place(hint, len, flags) {
            Ok(addr) => addr,
            Err(e) => return e,
        };
        let created = self.machine.windows().create(host_fd, offset, len, writable);
        match created {
            Ok((window, ipa)) => {
                self.use_window(window);
                self.vmas.insert(addr, addr + len, p, Backing::Shared { ipa });
                addr as i64
            }
            Err(e) => -e,
        }
    }

    /// A private mapping of file `host_fd` from `offset`, read through a
    /// window: no copy of what may never be read (the most of a library, a
    /// font), and pages nobody has written show the file's later changes, as
    /// on Linux. A page becomes a private copy when the guest first writes
    /// it.
    pub fn mmap_file(&mut self, hint: u64, len: u64, p: u64, flags: u64, host_fd: RawFd, offset: u64) -> i64 {
        let Some(len) = page_up(len).filter(|&l| l > 0 && l <= USER_TOP) else { return -errno::ENOMEM };
        let p = p & (prot::READ | prot::WRITE | prot::EXEC);
        if unsafe { libc::fcntl(host_fd, libc::F_GETFL) } & libc::O_ACCMODE == libc::O_WRONLY {
            return -errno::EACCES;
        }
        let addr = match self.place(hint, len, flags) {
            Ok(addr) => addr,
            Err(e) => return e,
        };
        let created = self.machine.windows().create(Some(host_fd), offset, len, false);
        match created {
            Ok((window, ipa)) => {
                self.use_window(window);
                self.vmas.insert(addr, addr + len, p, Backing::File { ipa });
                addr as i64
            }
            Err(e) => -e,
        }
    }

    /// Make `[start, end)`, inside one [`Backing::File`] region, private
    /// frames holding what the window shows.
    fn privatize(&mut self, start: u64, end: u64) -> Result<(), Fault> {
        let (s, v) = self.vmas.find(start).expect("privatize outside a region");
        let ipa = v.shared_pa(s, start).expect("privatize of a region with no window");
        let mut data = vec![0u8; (end - start) as usize];
        let mut done = 0;
        while done < data.len() {
            let n = (PAGE_SIZE as usize).min(data.len() - done);
            unsafe { std::ptr::copy_nonoverlapping(self.ptr(ipa + done as u64), data[done..].as_mut_ptr(), n) };
            done += n;
        }
        self.vmas.remove(start, end);
        if self.user.unmap_range(&mut self.phys, start, end) {
            self.tlb_stale = true;
        }
        self.vmas.insert(start, end, v.prot, Backing::Private);
        let window = self.machine.windows().find(ipa).into_iter().collect();
        self.release_unused_windows(window);
        self.copy_in(start, &data, None)
    }

    /// See [`GuestMemory::shared_futex_key`]: the window's object and
    /// offset for a `MAP_SHARED` region; None for the rest (a private
    /// file mapping's pages become the process's own when written, as on
    /// Linux, where only an unwritten one is keyed by its file).
    pub fn shared_futex_key(&self, addr: u64) -> Result<Option<SharedKey>, Fault> {
        let (start, vma) = self.vmas.find(addr).ok_or(Fault { addr })?;
        if !matches!(vma.backing, Backing::Shared { .. }) {
            return Ok(None);
        }
        let page = page_down(addr);
        let pa = vma.shared_pa(start, page).expect("a shared region's window") + (addr - page);
        Ok(self.machine.windows().futex_key(pa))
    }

    /// See [`GuestMemory::update_u32`]: a compare-and-swap on the host's
    /// view of the page, which is the guest's memory itself, so it's atomic
    /// against the guest's own atomics on other CPUs.
    pub fn update_u32(&mut self, addr: u64, f: &mut dyn FnMut(u32) -> Option<u32>) -> Result<u32, Fault> {
        if !addr.is_multiple_of(4) {
            return Err(Fault { addr });
        }
        let pa = self.page_to_write(addr, Some(Access::Write))?;
        let word = unsafe { std::sync::atomic::AtomicU32::from_ptr(self.ptr(pa + (addr - page_down(addr))).cast()) };
        let mut old = word.load(Ordering::Acquire);
        loop {
            let Some(new) = f(old) else { return Ok(old) };
            match word.compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(old),
                Err(now) => old = now,
            }
        }
    }

    pub fn munmap(&mut self, addr: u64, len: u64) -> i64 {
        let Some(end) = page_up(len).and_then(|l| addr.checked_add(l)) else { return -errno::EINVAL };
        if !addr.is_multiple_of(PAGE_SIZE) || len == 0 || end > USER_TOP {
            return -errno::EINVAL;
        }
        self.unmap(addr, end);
        0
    }

    pub fn regions(&self) -> Vec<linux_abi::Region> {
        self.vmas
            .iter()
            .map(|(start, v)| linux_abi::Region {
                start,
                end: v.end,
                prot: v.prot,
                shared: matches!(v.backing, Backing::Shared { .. }),
            })
            .collect()
    }

    pub fn mprotect(&mut self, addr: u64, len: u64, p: u64) -> i64 {
        let Some(end) = page_up(len).and_then(|l| addr.checked_add(l)) else { return -errno::ENOMEM };
        if !addr.is_multiple_of(PAGE_SIZE) {
            return -errno::EINVAL;
        }
        if len == 0 {
            return 0;
        }
        if !self.vmas.covers(addr, end) {
            return -errno::ENOMEM;
        }
        let p = p & (prot::READ | prot::WRITE | prot::EXEC);
        if p & prot::WRITE != 0 {
            // Shared mappings of read-only files can't become writable.
            for (s, v) in self.vmas.overlapping(addr, end) {
                if let Backing::Shared { ipa } = v.backing
                    && !self.machine.windows().writable(ipa + (s.max(addr) - s))
                {
                    return -errno::EACCES;
                }
            }
        }
        self.vmas.protect(addr, end, p);
        // Private file pages stay read-only in the page tables whatever the
        // protection, so the first write faults and makes a copy.
        let parts: Vec<(u64, u64, u64)> =
            self.vmas.overlapping(addr, end).map(|(s, v)| (s.max(addr), v.end.min(end), page_attrs(v))).collect();
        for (s, e, attrs) in parts {
            if self.user.protect_range(&mut self.phys, s, e, attrs) {
                self.tlb_stale = true;
            }
        }
        0
    }
}

/// Page-table attributes for a page of `vma`: its protection, except that
/// pages read through a private file mapping's window are never writable.
fn page_attrs(vma: Vma) -> u64 {
    match vma.backing {
        Backing::File { .. } => paging::user_attrs(vma.prot & !prot::WRITE),
        _ => paging::user_attrs(vma.prot),
    }
}

/// An address space shared by a process's threads: what the dispatcher gets
/// as its [`GuestMemory`]. Each access takes the lock briefly, so a thread
/// blocked in a system call doesn't hold up the others.
pub struct Space(Mutex<AddressSpace>);

// Safety: the raw pointers in an address space are into host mappings it
// owns; all access goes through the mutex.
unsafe impl Send for AddressSpace {}

impl Space {
    pub fn new(space: AddressSpace) -> Self {
        Space(Mutex::new(space))
    }

    pub fn lock(&self) -> MutexGuard<'_, AddressSpace> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn into_inner(self) -> AddressSpace {
        self.0.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl GuestMemory for Space {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.lock().read(addr, buf)
    }
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        self.lock().write(addr, data)
    }
    fn brk(&self, addr: u64) -> u64 {
        self.lock().brk(addr)
    }
    fn mmap_anonymous(&self, hint: u64, len: u64, p: u64, flags: u64) -> i64 {
        self.lock().mmap_anonymous(hint, len, p, flags)
    }
    fn mmap_shared(&self, hint: u64, len: u64, p: u64, flags: u64, host_fd: Option<RawFd>, offset: u64) -> i64 {
        self.lock().mmap_shared(hint, len, p, flags, host_fd, offset)
    }
    fn mmap_file(&self, hint: u64, len: u64, p: u64, flags: u64, host_fd: RawFd, offset: u64) -> i64 {
        self.lock().mmap_file(hint, len, p, flags, host_fd, offset)
    }
    fn munmap(&self, addr: u64, len: u64) -> i64 {
        self.lock().munmap(addr, len)
    }
    fn mprotect(&self, addr: u64, len: u64, p: u64) -> i64 {
        self.lock().mprotect(addr, len, p)
    }
    fn resident_pages(&self) -> u64 {
        self.lock().frames_in_use()
    }

    fn regions(&self) -> Vec<linux_abi::Region> {
        self.lock().regions()
    }
    fn discard(&self, addr: u64, len: u64) -> i64 {
        self.lock().discard(addr, len)
    }
    fn shared_futex_key(&self, addr: u64) -> Result<Option<SharedKey>, Fault> {
        self.lock().shared_futex_key(addr)
    }
    fn update_u32(&self, addr: u64, f: &mut dyn FnMut(u32) -> Option<u32>) -> Result<u32, Fault> {
        self.lock().update_u32(addr, f)
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Its frames (and page tables) may still be in TLBs under its ASID:
        // they go back to the pool once that's flushed.
        let retired = Retired { asid: self.asid, user: PageTable { root: self.user.root }, quarantine: self.phys.take_quarantine() };
        self.machine.retire(retired, self.ran);
        // Its shared windows go too, if no other address space uses them.
        // (Their pages may still be in TLBs under its ASID, but nothing runs
        // with that ASID again until it has been flushed.)
        let mut windows = self.machine.windows();
        for &w in &self.windows {
            windows.release(w);
        }
    }
}

/// The vector table: every entry is `hvc #<index>; eret`. The hvc exits to the
/// host, which handles the exception (for a system call: puts the result in
/// x0) and resumes at the eret, returning to EL0. The EL0-synchronous entry
/// also has a return path at [`VECTOR_FLUSH_PATH`] that flushes the running
/// address space's TLB entries first: by the ASID in `TTBR0_EL1`, which it
/// reads into x0 (saved meanwhile in `TPIDR_EL1`, which nothing else uses).
/// After the table, [`FLUSH_ASID_PATH`] flushes a given ASID.
fn write_vectors(phys: &mut PhysMem, frame: u64) {
    const ERET: u32 = 0xd69f_03e0;
    const DSB_ISHST: u32 = 0xd503_3a9f;
    const DSB_ISH: u32 = 0xd503_3b9f;
    const ISB: u32 = 0xd503_3fdf;
    const MSR_TPIDR_EL1_X0: u32 = 0xd518_d080;
    const MRS_X0_TPIDR_EL1: u32 = 0xd538_d080;
    const MRS_X0_TTBR0_EL1: u32 = 0xd538_2000;
    const LSR_X0_48: u32 = 0xd370_fc00; // the ASID, alone
    const LSL_X0_48: u32 = 0xd350_3c00; // back in bits 63:48
    const TLBI_ASIDE1IS_X0: u32 = 0xd508_8340;
    let write = |phys: &mut PhysMem, at: u64, code: &[u32]| {
        for (j, insn) in code.iter().enumerate() {
            unsafe { phys.ptr(frame + at + j as u64 * 4).cast::<u32>().write(*insn) };
        }
    };
    for i in 0..16u64 {
        let hvc = 0xd400_0002 | ((i as u32) << 5);
        let mut code = vec![hvc, ERET];
        if i == VECTOR_EL0_SYNC {
            code.extend([
                DSB_ISHST,
                MSR_TPIDR_EL1_X0,
                MRS_X0_TTBR0_EL1,
                LSR_X0_48,
                LSL_X0_48,
                TLBI_ASIDE1IS_X0,
                DSB_ISH,
                ISB,
                MRS_X0_TPIDR_EL1,
                ERET,
            ]);
        }
        write(phys, i * 0x80, &code);
    }
    write(phys, FLUSH_ASID_PATH - VECTORS_VA, &[DSB_ISHST, TLBI_ASIDE1IS_X0, DSB_ISH, ISB, ERET]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RW: u64 = prot::READ | prot::WRITE;

    fn space() -> AddressSpace {
        AddressSpace::new(&Machine::new().unwrap()).unwrap()
    }

    fn mmap(s: &mut AddressSpace, len: u64, p: u64) -> u64 {
        let a = s.mmap_anonymous(0, len, p, mm::MAP_PRIVATE | mm::MAP_ANONYMOUS);
        assert!(a > 0, "mmap failed: {a}");
        a as u64
    }

    fn read_byte(s: &AddressSpace, addr: u64) -> Result<u8, Fault> {
        let mut b = [0];
        s.read(addr, &mut b).map(|_| b[0])
    }

    #[test]
    fn mmap_is_lazy_and_zeroed() {
        let mut s = space();
        let before = s.frames_in_use();
        let a = mmap(&mut s, 1 << 30, RW);
        assert_eq!(s.frames_in_use(), before, "nothing populated yet");
        assert_eq!(read_byte(&s, a + 12345).unwrap(), 0);
        s.write(a + 12345, &[7]).unwrap();
        assert_eq!(read_byte(&s, a + 12345).unwrap(), 7);
        assert!(s.frames_in_use() > before);
    }

    #[test]
    fn placement_is_top_down_and_respects_hints() {
        let mut s = space();
        let a = mmap(&mut s, 0x3000, RW);
        let b = mmap(&mut s, 0x1000, RW);
        assert_eq!(a + 0x3000, MMAP_BASE);
        assert_eq!(b + 0x1000, a);
        let hinted = s.mmap_anonymous(0x5000_0000, 0x1000, RW, mm::MAP_PRIVATE | mm::MAP_ANONYMOUS);
        assert_eq!(hinted, 0x5000_0000);
    }

    #[test]
    fn writes_cross_pages_and_respect_protection() {
        let mut s = space();
        let a = mmap(&mut s, 0x2000, RW);
        s.write(a + 0xffe, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0; 4];
        s.read(a + 0xffe, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        assert_eq!(s.mprotect(a, 0x1000, prot::READ), 0);
        assert!(s.take_tlb_stale(), "a mapped page changed");
        assert_eq!(s.write(a + 0x10, &[9]), Err(Fault { addr: a + 0x10 }));
        assert_eq!(read_byte(&s, a + 0xffe).unwrap(), 1, "contents survive mprotect");
        s.write(a + 0x1000, &[9]).unwrap();

        assert_eq!(s.mprotect(a, 0x1000, 0), 0);
        assert!(read_byte(&s, a).is_err());
        assert_eq!(s.mprotect(a, 0x1000, RW), 0);
        assert_eq!(read_byte(&s, a + 0xffe).unwrap(), 1, "contents survive PROT_NONE");
    }

    #[test]
    fn discard_zeroes_private_pages_and_keeps_the_region() {
        let mut s = space();
        let a = mmap(&mut s, 0x3000, RW);
        s.write(a, &[7; 0x3000]).unwrap();
        let used = s.frames_in_use();
        assert_eq!(s.discard(a + 0x1000, 0x1000), 0);
        s.flushed();
        assert_eq!(s.frames_in_use(), used - 1);
        assert_eq!(read_byte(&s, a + 0x1000).unwrap(), 0, "reads as zero, still mapped");
        assert_eq!(read_byte(&s, a).unwrap(), 7);
        assert_eq!(s.discard(a + 0x10_0000, 0x1000), -errno::ENOMEM, "unmapped");
    }

    #[test]
    fn munmap_frees_and_remap_is_zero() {
        let mut s = space();
        let a = mmap(&mut s, 0x4000, RW);
        s.write(a, &[0xff; 0x4000]).unwrap();
        let used = s.frames_in_use();
        assert_eq!(s.munmap(a + 0x1000, 0x1000), 0);
        assert!(s.needs_flush());
        assert_eq!(s.frames_in_use(), used, "quarantined until the TLB is flushed");
        s.flushed();
        assert!(!s.needs_flush());
        assert_eq!(s.frames_in_use(), used - 1);
        assert!(read_byte(&s, a + 0x1000).is_err());
        assert_eq!(read_byte(&s, a + 0x2000).unwrap(), 0xff);

        let fixed = s.mmap_anonymous(a, 0x4000, RW, mm::MAP_PRIVATE | mm::MAP_ANONYMOUS | mm::MAP_FIXED);
        assert_eq!(fixed, a as i64);
        assert_eq!(read_byte(&s, a + 0x2000).unwrap(), 0, "MAP_FIXED replaces contents");
        let noreplace = s.mmap_anonymous(a, 0x1000, RW, mm::MAP_PRIVATE | mm::MAP_ANONYMOUS | mm::MAP_FIXED_NOREPLACE);
        assert_eq!(noreplace, -errno::EEXIST);
    }

    #[test]
    fn mprotect_unmapped_is_enomem() {
        let mut s = space();
        let a = mmap(&mut s, 0x1000, RW);
        assert_eq!(s.mprotect(a, 0x2000, prot::READ), -errno::ENOMEM);
        assert_eq!(s.mprotect(a + 1, 0x1000, prot::READ), -errno::EINVAL);
        assert_eq!(s.munmap(a + 1, 0x1000), -errno::EINVAL);
    }

    #[test]
    fn brk_grows_and_shrinks() {
        let mut s = space();
        s.set_brk_start(0x40_1234);
        let start = s.brk(0);
        assert_eq!(start, 0x40_2000);
        assert_eq!(s.brk(start + 0x2800), start + 0x2800);
        s.write(start + 0x2000, &[5]).unwrap();
        assert_eq!(s.brk(start + 0x1000), start + 0x1000);
        assert!(s.write(start + 0x2000, &[5]).is_err(), "shrunk away");
        assert_eq!(s.brk(start + 0x3000), start + 0x3000);
        assert_eq!(read_byte(&s, start + 0x2000).unwrap(), 0, "regrown memory is zero");
        // Can't grow into another mapping.
        let blocker = s.mmap_anonymous(start + 0x10000, 0x1000, RW, mm::MAP_PRIVATE | mm::MAP_ANONYMOUS | mm::MAP_FIXED);
        assert!(blocker > 0);
        assert_eq!(s.brk(start + 0x20000), start + 0x3000);
    }

    #[test]
    fn faults_populate_only_allowed_access() {
        let mut s = space();
        let a = 0x1000_0000;
        let flags = mm::MAP_PRIVATE | mm::MAP_ANONYMOUS | mm::MAP_FIXED;
        assert_eq!(s.mmap_anonymous(a, 0x100000, prot::READ, flags), a as i64);
        assert_eq!(s.handle_fault(a + 0x5008, Access::Write), Ok(false));
        assert_eq!(s.handle_fault(a + 0x5008, Access::Read), Ok(true));
        // Fault-around mapped the aligned 64 KiB window.
        assert!(s.user.lookup(&s.phys, a).is_some());
        assert!(s.user.lookup(&s.phys, a + 0xf000).is_some());
        assert!(s.user.lookup(&s.phys, a + 0x10000).is_none());
        assert_eq!(s.handle_fault(0x1234, Access::Read), Ok(false));
    }

    /// A fork inside the pool: each side's writes are its own, whichever
    /// writes first and however many forks share a frame; frames shared by
    /// no one any more are made writable again without a copy; everything
    /// goes back when the address spaces do.
    #[test]
    fn fork_copies_on_write() {
        let machine = Machine::new().unwrap();
        let pool = machine.pool().clone();
        let base = pool.frames_held();
        let mut parent = AddressSpace::new(&machine).unwrap();
        let a = mmap(&mut parent, 0x8000, RW);
        parent.write(a, &[1; 0x8000]).unwrap();
        parent.flushed();
        let held = pool.frames_held();

        let mut child = parent.fork().unwrap();
        assert!(parent.needs_flush(), "the parent's pages became read-only");
        parent.flushed();
        assert_ne!(child.asid(), parent.asid());
        assert!(pool.frames_held() - held <= paging::CHUNK_FRAMES as u64, "only page tables are new (a chunk of frames for them)");
        assert_eq!(read_byte(&child, a + 0x10).unwrap(), 1);
        // A write fault in the child, and a system call's write in the parent.
        assert_eq!(child.handle_fault(a, Access::Write), Ok(true));
        child.write(a, &[2]).unwrap();
        parent.write(a + 0x1000, &[3]).unwrap();
        assert_eq!(read_byte(&parent, a).unwrap(), 1);
        assert_eq!(read_byte(&child, a).unwrap(), 2);
        assert_eq!(read_byte(&child, a + 0x1000).unwrap(), 1);
        assert_eq!(read_byte(&parent, a + 0x1000).unwrap(), 3);

        // A second fork shares what the first still does.
        let mut second = parent.fork().unwrap();
        parent.flushed();
        second.write(a + 0x2000, &[4]).unwrap();
        child.write(a + 0x3000, &[5]).unwrap();
        for (s, want) in [(&parent, [1, 3, 1, 1]), (&child, [2, 1, 1, 5]), (&second, [1, 3, 4, 1])] {
            for (page, &b) in want.iter().enumerate() {
                assert_eq!(read_byte(s, a + page as u64 * 0x1000).unwrap(), b);
            }
        }

        // Protection changes keep shared pages read-only.
        assert_eq!(parent.mprotect(a, 0x8000, prot::READ), 0);
        assert_eq!(parent.mprotect(a, 0x8000, RW), 0);
        parent.flushed();
        assert_eq!(parent.handle_fault(a + 0x4000, Access::Write), Ok(true));
        parent.write(a + 0x4000, &[6]).unwrap();
        assert_eq!(read_byte(&child, a + 0x4000).unwrap(), 1);
        assert_eq!(read_byte(&second, a + 0x4000).unwrap(), 1);

        // With the forks gone (they never ran, so their frames go at once),
        // the parent's pages are its own again: written in place.
        drop(child);
        drop(second);
        parent.flushed();
        let before = parent.frames_in_use();
        parent.write(a + 0x5000, &[7]).unwrap();
        parent.flushed();
        assert_eq!(parent.frames_in_use(), before, "no copy");
        let (_, desc) = parent.user.entry_of(&parent.phys, a + 0x5000).unwrap();
        assert_eq!(desc & paging::DESC_COW, 0);
        drop(parent);
        assert_eq!(pool.frames_held(), base, "every frame of the address spaces back");
    }

    #[test]
    fn fork_shares_shared_memory() {
        let mut parent = space();
        let a = parent.mmap_shared(0, 0x2000, RW, mm::MAP_SHARED | mm::MAP_ANONYMOUS, None, 0) as u64;
        parent.write(a, &[1]).unwrap();
        let mut child = parent.fork().unwrap();
        child.write(a, &[2]).unwrap();
        assert_eq!(read_byte(&parent, a).unwrap(), 2, "MAP_SHARED stays shared");
        drop(parent);
        assert_eq!(child.window_ranges().len(), 1, "the window stays while the child uses it");
        assert_eq!(read_byte(&child, a).unwrap(), 2);
    }

    mod shared {
        use std::{
            fs::File,
            io::Write,
            os::{fd::AsRawFd, unix::fs::FileExt},
        };

        use super::*;

        const SHARED: u64 = mm::MAP_SHARED;

        /// A file of 4 KiB pages filled with 'a', 'b', 'c', ...
        fn pages(n: u8) -> tempfile::NamedTempFile {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            for i in 0..n {
                f.write_all(&[b'a' + i; 4096]).unwrap();
            }
            f
        }

        fn map(s: &mut AddressSpace, file: &File, len: u64, p: u64, offset: u64) -> i64 {
            s.mmap_shared(0, len, p, SHARED, Some(file.as_raw_fd()), offset)
        }

        #[test]
        fn file_mapping_is_a_live_view() {
            let f = pages(3);
            let file = f.reopen().unwrap();
            let mut s = space();
            // Offset 4 KiB isn't host-page aligned; the window covers from 0.
            let a = map(&mut s, &file, 0x2000, RW, 0x1000) as u64;
            assert_eq!(read_byte(&s, a).unwrap(), b'b');
            assert_eq!(read_byte(&s, a + 0x1000).unwrap(), b'c');

            s.write(a + 5, b"guest").unwrap();
            let mut buf = [0; 5];
            file.read_exact_at(&mut buf, 0x1005).unwrap();
            assert_eq!(&buf, b"guest", "guest writes reach the file");

            file.write_all_at(b"host", 0x2000).unwrap();
            let mut buf = [0; 4];
            s.read(a + 0x1000, &mut buf).unwrap();
            assert_eq!(&buf, b"host", "host writes reach the guest");
        }

        #[test]
        fn two_mappings_of_one_file_alias() {
            let f = pages(1);
            let file = f.reopen().unwrap();
            let mut s = space();
            let a = map(&mut s, &file, 0x1000, RW, 0) as u64;
            let b = map(&mut s, &file, 0x1000, prot::READ, 0) as u64;
            assert_ne!(a, b);
            s.write(a + 100, &[42]).unwrap();
            assert_eq!(read_byte(&s, b + 100).unwrap(), 42);
            assert!(s.write(b, &[1]).is_err(), "second mapping is read-only");
        }

        /// Address spaces of one machine mapping the same file share its
        /// window (one host mapping, one place in guest physical memory),
        /// for shared and private read-only mappings alike, and key a
        /// futex there alike; a private mapping's first write is its own.
        #[test]
        fn one_window_per_object_and_access() {
            let f = pages(4);
            let file = f.reopen().unwrap();
            let machine = Machine::new().unwrap();
            let mut spaces: Vec<AddressSpace> = (0..3).map(|_| AddressSpace::new(&machine).unwrap()).collect();
            let shared: Vec<u64> = spaces.iter_mut().map(|s| map(s, &file, 0x4000, RW, 0) as u64).collect();
            assert_eq!(machine.windows().count(), 1, "one window for three mappings");
            spaces[0].write(shared[0] + 0x1000, b"x").unwrap();
            assert_eq!(read_byte(&spaces[2], shared[2] + 0x1000).unwrap(), b'x');
            let keys: Vec<_> = spaces.iter().zip(&shared).map(|(s, &a)| s.shared_futex_key(a + 0x1004).unwrap().unwrap()).collect();
            assert!(keys.iter().all(|k| *k == keys[0] && k.offset == 0x1004), "{keys:?}");
            // A smaller range inside it, at an offset: the same window.
            let inner = map(&mut spaces[1], &file, 0x1000, RW, 0x1000) as u64;
            assert_eq!(read_byte(&spaces[1], inner).unwrap(), b'x');
            assert_eq!(machine.windows().count(), 1);
            // Private read-only mappings (a library's): a window of their
            // own access, one for all.
            let private: Vec<u64> = spaces.iter_mut().map(|s| s.mmap_file(0, 0x4000, RW, mm::MAP_PRIVATE, file.as_raw_fd(), 0) as u64).collect();
            assert_eq!(machine.windows().count(), 2);
            assert_eq!(spaces[0].shared_futex_key(private[0]).unwrap(), None, "a private mapping's futexes are its own");
            spaces[0].write(private[0], b"mine").unwrap();
            assert_eq!(read_byte(&spaces[1], private[1]).unwrap(), b'a', "the first write stayed private");
            assert_eq!(read_byte(&spaces[0], shared[0]).unwrap(), b'a', "and out of the file");
            // Unmapping in one leaves the others' views.
            assert_eq!(spaces[1].munmap(shared[1], 0x4000), 0);
            assert_eq!(spaces[1].munmap(inner, 0x1000), 0);
            spaces[2].write(shared[2] + 2, b"y").unwrap();
            assert_eq!(read_byte(&spaces[0], shared[0] + 2).unwrap(), b'y');
            drop(spaces);
            assert_eq!(machine.windows().count(), 0, "gone with their last users");
        }

        #[test]
        fn faults_map_the_window_not_private_frames() {
            let f = pages(1);
            let file = f.reopen().unwrap();
            let mut s = space();
            let a = map(&mut s, &file, 0x1000, RW, 0) as u64;
            let before = s.frames_in_use();
            assert_eq!(s.handle_fault(a + 8, Access::Write), Ok(true));
            let pa = s.user.lookup(&s.phys, a).unwrap();
            assert!(pa >= POOL_SIZE, "maps into the window");
            assert_eq!(unsafe { *s.ptr(pa) }, b'a');
            // Only page-table pages were allocated, not a data frame.
            let tables = s.frames_in_use() - before;
            assert!(tables <= 3, "{tables} frames");
            // Unmapping must not hand the window's page to the private pool.
            assert_eq!(s.munmap(a, 0x1000), 0);
            assert_eq!(s.frames_in_use() - before, tables);
        }

        #[test]
        fn read_only_files_stay_read_only() {
            let f = pages(1);
            let file = File::open(f.path()).unwrap();
            let mut s = space();
            assert_eq!(map(&mut s, &file, 0x1000, RW, 0), -errno::EACCES);
            let a = map(&mut s, &file, 0x1000, prot::READ, 0);
            assert!(a > 0);
            assert_eq!(s.mprotect(a as u64, 0x1000, RW), -errno::EACCES);
            assert_eq!(s.mprotect(a as u64, 0x1000, 0), 0, "dropping access is fine");
        }

        #[test]
        fn windows_are_released_when_unused() {
            let f = pages(4);
            let file = f.reopen().unwrap();
            let mut s = space();
            let a = map(&mut s, &file, 0x4000, RW, 0) as u64;
            assert_eq!(s.window_ranges().len(), 1);
            assert_eq!(s.munmap(a + 0x1000, 0x2000), 0);
            assert_eq!(s.window_ranges().len(), 1, "both ends still mapped");
            assert_eq!(read_byte(&s, a + 0x3000).unwrap(), b'd', "the tail keeps its offset");
            assert_eq!(s.munmap(a, 0x1000), 0);
            assert_eq!(s.window_ranges().len(), 1);
            assert_eq!(s.munmap(a + 0x3000, 0x1000), 0);
            assert!(s.window_ranges().is_empty());
            // MAP_FIXED over a shared mapping releases it too.
            let b = map(&mut s, &file, 0x1000, RW, 0) as u64;
            let flags = mm::MAP_PRIVATE | mm::MAP_ANONYMOUS | mm::MAP_FIXED;
            assert_eq!(s.mmap_anonymous(b, 0x1000, RW, flags), b as i64);
            assert!(s.window_ranges().is_empty());
        }

        #[test]
        fn private_file_mappings_copy_only_pages_written() {
            let f = pages(3);
            let file = File::open(f.path()).unwrap();
            let mut s = space();
            let flags = mm::MAP_PRIVATE;
            let a = s.mmap_file(0, 0x2000, prot::READ, flags, file.as_raw_fd(), 0x1000) as u64;
            assert_eq!(read_byte(&s, a).unwrap(), b'b');
            assert_eq!(read_byte(&s, a + 0x1000).unwrap(), b'c');
            assert!(s.write(a, &[1]).is_err());
            assert!(s.regions().iter().all(|r| !r.shared), "private, for /proc/self/maps");

            // Writable: nothing is copied until a page is written, and then
            // only that page; the file and the other page (still the
            // window) are as they were.
            let before = s.frames_in_use();
            assert_eq!(s.mprotect(a, 0x2000, RW), 0);
            assert_eq!(s.frames_in_use(), before);
            assert_eq!(s.handle_fault(a + 0x1000, Access::Write), Ok(true));
            assert!(s.frames_in_use() > before);
            assert_eq!(read_byte(&s, a + 0x1000).unwrap(), b'c');
            s.write(a + 0x1000, b"mine").unwrap();
            let mut buf = [0; 4];
            file.read_exact_at(&mut buf, 0x2000).unwrap();
            assert_eq!(&buf, b"cccc");
            assert_eq!(read_byte(&s, a).unwrap(), b'b');
            assert_eq!(s.window_ranges().len(), 1);
            s.write(a, b"also").unwrap();
            assert!(s.window_ranges().is_empty(), "no longer read through");
            let b = s.mmap_file(0, 0x1000, RW, flags, file.as_raw_fd(), 0);
            assert!(b > 0, "a private mapping may be writable on a read-only fd");
        }

        #[test]
        fn map_fixed_over_a_mapping_keeps_the_new_window() {
            let f = pages(2);
            let file = File::open(f.path()).unwrap();
            let mut s = space();
            let a = s.mmap_file(0, 0x2000, prot::READ, mm::MAP_PRIVATE, file.as_raw_fd(), 0) as u64;
            // As a dynamic loader does: the second page again, from elsewhere
            // in the file, over the first mapping (whose window goes).
            let fixed = mm::MAP_PRIVATE | mm::MAP_FIXED;
            assert_eq!(s.mmap_file(a, 0x1000, prot::READ, fixed, file.as_raw_fd(), 0x1000), a as i64);
            assert_eq!(read_byte(&s, a).unwrap(), b'b');
            assert_eq!(read_byte(&s, a + 0x1000).unwrap(), b'b');
            // The same for shared mappings.
            let fixed = SHARED | mm::MAP_FIXED;
            assert_eq!(s.mmap_shared(a, 0x1000, prot::READ, fixed, Some(file.as_raw_fd()), 0), a as i64);
            assert_eq!(read_byte(&s, a).unwrap(), b'a');
        }

        #[test]
        fn shared_anonymous() {
            let mut s = space();
            let a = s.mmap_shared(0, 0x3000, RW, SHARED | mm::MAP_ANONYMOUS, None, 0) as u64;
            assert_eq!(read_byte(&s, a + 0x2fff).unwrap(), 0);
            s.write(a + 0x2fff, &[9]).unwrap();
            assert_eq!(read_byte(&s, a + 0x2fff).unwrap(), 9);
        }
    }
}
