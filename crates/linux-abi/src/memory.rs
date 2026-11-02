//! The guest-memory interface the core provides to `linux-abi`.

use std::os::fd::RawFd;

use crate::errno;

/// A guest address the guest isn't allowed to touch (unmapped, wrong
/// permissions). System calls turn this into `-EFAULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub addr: u64,
}

impl From<Fault> for i64 {
    fn from(_: Fault) -> i64 {
        errno::EFAULT
    }
}

/// One mapped region, for `/proc/self/maps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    /// `PROT_*` bits.
    pub prot: u64,
    pub shared: bool,
}

/// Access to one Linux process's address space, shared by its threads (so
/// `&self`: implementations synchronise internally).
///
/// **The core implements this.** Addresses are guest virtual addresses — the
/// raw pointer values the Linux program passed in its system call arguments.
/// `linux-abi` never assumes anything about how guest memory is laid out on
/// the host; it only reads and writes through these methods.
pub trait GuestMemory {
    /// Copy `buf.len()` bytes starting at guest address `addr` into `buf`.
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault>;

    /// Copy `data` into guest memory starting at guest address `addr`.
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault>;

    /// Move the program break (the `brk` system call). Return the new break,
    /// or the current one if `addr` is 0 or the request can't be satisfied —
    /// that's Linux's contract, not an error code.
    fn brk(&self, addr: u64) -> u64;

    /// Map zeroed memory (`mmap` with MAP_ANONYMOUS). `flags` are Linux's; the
    /// core honours MAP_FIXED and MAP_FIXED_NOREPLACE and otherwise treats
    /// `addr_hint` as a hint. Return the guest address, or a negative Linux
    /// errno. File-backed mappings are handled inside `linux-abi` by reading
    /// the file into an anonymous mapping (good enough for shared libraries).
    fn mmap_anonymous(&self, addr_hint: u64, len: u64, prot: u64, flags: u64) -> i64;

    /// Map memory shared with other processes (`mmap` with MAP_SHARED): host
    /// file `host_fd` from `offset` (4 KiB-aligned), or new shared anonymous
    /// memory if `host_fd` is None. Writes are visible to every process that
    /// maps the same file, host processes included (this is how a Wayland
    /// client's `wl_shm` buffer reaches the compositor). Same placement and
    /// return value as [`GuestMemory::mmap_anonymous`].
    fn mmap_shared(&self, addr_hint: u64, len: u64, prot: u64, flags: u64, host_fd: Option<RawFd>, offset: u64) -> i64;

    /// Map a private view of regular file `host_fd` from `offset` (`mmap`
    /// with MAP_PRIVATE) without copying it: pages come from the file as
    /// they're read, show its later changes until written, and become a
    /// private copy when written, as on Linux. Same return value as
    /// [`GuestMemory::mmap_anonymous`]; -ENODEV if the memory can't, and the
    /// caller copies the file in instead.
    fn mmap_file(&self, _addr_hint: u64, _len: u64, _prot: u64, _flags: u64, _host_fd: RawFd, _offset: u64) -> i64 {
        -errno::ENODEV
    }

    /// Unmap a range (`munmap`). Return 0 or a negative Linux errno.
    fn munmap(&self, addr: u64, len: u64) -> i64;

    /// Change protection on a range (`mprotect`). Return 0 or a negative errno.
    fn mprotect(&self, addr: u64, len: u64, prot: u64) -> i64;

    /// Drop the private pages in a range (`madvise` DONTNEED/FREE): they read
    /// as zeros again. Shared pages keep their contents. Return 0 or a
    /// negative errno.
    fn discard(&self, _addr: u64, _len: u64) -> i64 {
        0
    }

    /// The mapped regions, in address order.
    fn regions(&self) -> Vec<Region> {
        Vec::new()
    }
}

/// Helpers built on [`GuestMemory`] for the dispatcher.
#[allow(dead_code)]
pub(crate) trait GuestMemoryExt: GuestMemory {
    /// Read a NUL-terminated string (paths), up to Linux's PATH_MAX.
    fn read_cstr(&self, addr: u64) -> Result<Vec<u8>, i64> {
        const PATH_MAX: usize = 4096;
        let mut out = Vec::new();
        let mut chunk = [0u8; 64];
        while out.len() < PATH_MAX {
            // A 64-byte chunk can run past the string into an unmapped page, so
            // fall back to a single byte when a chunk read faults.
            let addr_here = addr + out.len() as u64;
            let n = if self.read(addr_here, &mut chunk).is_ok() {
                chunk.len()
            } else {
                self.read(addr_here, &mut chunk[..1]).map_err(i64::from)?;
                1
            };
            if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..end]);
                return Ok(out);
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Err(errno::ENAMETOOLONG)
    }

    fn read_pod<T: Copy + Default>(&self, addr: u64) -> Result<T, i64> {
        let mut v = T::default();
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, std::mem::size_of::<T>())
        };
        self.read(addr, bytes).map_err(i64::from)?;
        Ok(v)
    }

    fn write_pod<T>(&self, addr: u64, v: &T) -> Result<(), i64> {
        self.write(addr, unsafe { crate::abi::as_bytes(v) }).map_err(i64::from)
    }
}

impl<M: GuestMemory + ?Sized> GuestMemoryExt for M {}
