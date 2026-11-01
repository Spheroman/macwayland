//! Shared memory windows: host memory mapped `MAP_SHARED` (a file, or shared
//! anonymous memory) and placed in the VM's physical address space above the
//! private frame pool, so the guest and other host processes — a Wayland
//! compositor reading a client's `wl_shm` buffer — see the same bytes.

use std::{
    collections::BTreeMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use linux_abi::{
    errno,
    wire::{Decoder, Encoder, Truncated},
};

use crate::{
    hv::{self, HvError},
    paging::PHYS_SIZE,
};

/// macOS's page size: host mappings (and so windows) are aligned to it.
pub const HOST_PAGE: u64 = 16 << 10;
/// Windows live between the private pool and the end of the VM's default
/// 36-bit IPA space.
const IPA_BASE: u64 = PHYS_SIZE;
const IPA_LIMIT: u64 = 1 << 36;

struct Window {
    host: NonNull<u8>,
    len: u64,
    writable: bool,
    /// What's mapped, from `offset`: the guest's file, or a POSIX shm object
    /// for shared anonymous memory. Kept so a forked child can map it too.
    fd: OwnedFd,
    offset: u64,
}

/// A new, nameless POSIX shared memory object of `len` bytes. Unlike a file,
/// its pages are never written to disk (only swapped, under pressure).
pub fn anonymous_shm(len: u64) -> Result<OwnedFd, i64> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let last_errno = || errno::from_io(&std::io::Error::last_os_error());
    // Names are at most 31 bytes on macOS, and the object is unlinked at once.
    for _ in 0..16 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = std::ffi::CString::new(format!("/macwl.{}.{n}", std::process::id())).unwrap();
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
        if fd < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(last_errno());
        }
        unsafe { libc::shm_unlink(name.as_ptr()) };
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // macOS sizes a shm object once; that's all a mapping needs.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(last_errno());
        }
        return Ok(fd);
    }
    Err(errno::EEXIST)
}

pub struct SharedWindows {
    /// Keyed by guest physical start.
    windows: BTreeMap<u64, Window>,
    /// Free guest physical ranges, start → end.
    free: BTreeMap<u64, u64>,
    /// Whether the VM exists, so windows must be mapped into it as they're
    /// created and unmapped as they're released.
    vm_live: bool,
}

impl Default for SharedWindows {
    fn default() -> Self {
        SharedWindows { windows: BTreeMap::new(), free: BTreeMap::from([(IPA_BASE, IPA_LIMIT)]), vm_live: false }
    }
}

impl SharedWindows {
    /// Map `len` bytes of `fd` from `offset` (or new shared anonymous memory
    /// if `fd` is None) and return the guest physical address of `offset`.
    /// `offset` must be 4 KiB-aligned; the window itself starts at the 16 KiB
    /// host page below it.
    pub fn create(&mut self, fd: Option<RawFd>, offset: u64, len: u64, writable: bool) -> Result<u64, i64> {
        let host_off = offset & !(HOST_PAGE - 1);
        let delta = offset - host_off;
        let host_len = (delta + len).next_multiple_of(HOST_PAGE);
        let (fd, host_off) = match fd {
            Some(fd) => {
                let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                if dup < 0 {
                    return Err(errno::from_io(&std::io::Error::last_os_error()));
                }
                (unsafe { OwnedFd::from_raw_fd(dup) }, host_off)
            }
            None => (anonymous_shm(host_len)?, 0),
        };
        let window = map_host(fd, host_off, host_len, writable)?;
        let Some(ipa) = self.alloc(host_len) else {
            unsafe { libc::munmap(window.host.as_ptr().cast(), host_len as usize) };
            return Err(errno::ENOMEM);
        };
        if self.vm_live
            && let Err(e) = map_window(ipa, &window)
        {
            eprintln!("macwl-runtime: {e}");
            unsafe { libc::munmap(window.host.as_ptr().cast(), host_len as usize) };
            self.dealloc(ipa, host_len);
            return Err(errno::ENOMEM);
        }
        self.windows.insert(ipa, window);
        Ok(ipa + delta)
    }

    /// Map every window into the (newly created) VM, and later ones as they
    /// are created.
    pub fn attach_vm(&mut self) -> Result<(), HvError> {
        for (&ipa, w) in &self.windows {
            map_window(ipa, w)?;
        }
        self.vm_live = true;
        Ok(())
    }

    /// Unmap every window from the VM (which stays), for `execve`.
    pub fn detach_vm(&mut self) -> Result<(), HvError> {
        for (&ipa, w) in &self.windows {
            hv::unmap(ipa, w.len as usize)?;
        }
        self.vm_live = false;
        Ok(())
    }

    /// The window containing guest physical address `pa`, as (start, end).
    pub fn find(&self, pa: u64) -> Option<(u64, u64)> {
        let (&start, w) = self.windows.range(..=pa).next_back()?;
        (pa < start + w.len).then_some((start, start + w.len))
    }

    /// Host pointer to guest physical address `pa` inside a window.
    pub fn ptr(&self, pa: u64) -> Option<*mut u8> {
        let (start, _) = self.find(pa)?;
        Some(unsafe { self.windows[&start].host.as_ptr().add((pa - start) as usize) })
    }

    /// Whether the window holding `pa` can be mapped writable.
    pub fn writable(&self, pa: u64) -> bool {
        self.find(pa).is_some_and(|(start, _)| self.windows[&start].writable)
    }

    /// All windows, as (start, end).
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.windows.iter().map(|(&s, w)| (s, s + w.len)).collect()
    }

    /// Unmap and free the window starting at `start`.
    pub fn release(&mut self, start: u64) {
        let w = self.windows.remove(&start).expect("no such window");
        if self.vm_live
            && let Err(e) = hv::unmap(start, w.len as usize)
        {
            eprintln!("macwl-runtime: {e}");
        }
        unsafe { libc::munmap(w.host.as_ptr().cast(), w.len as usize) };
        self.dealloc(start, w.len);
    }

    fn alloc(&mut self, len: u64) -> Option<u64> {
        let (&start, &end) = self.free.iter().find(|&(&s, &e)| e - s >= len)?;
        self.free.remove(&start);
        if end - start > len {
            self.free.insert(start + len, end);
        }
        Some(start)
    }

    fn dealloc(&mut self, mut start: u64, len: u64) {
        let mut end = start + len;
        if let Some((&prev, &prev_end)) = self.free.range(..start).next_back()
            && prev_end == start
        {
            self.free.remove(&prev);
            start = prev;
        }
        if let Some(next_end) = self.free.remove(&end) {
            end = next_end;
        }
        self.free.insert(start, end);
    }
}

/// Map `len` bytes of `fd` from `offset` into this process.
///
/// Hypervisor.framework won't map host memory into the VM if the mapping can
/// never be writable — a shared mapping of a file opened read-only (how
/// fontconfig maps its caches). Such a window is mapped private and writable
/// on the host side instead: nothing writes it (the guest's page tables and
/// the VM mapping are read-only), and a private mapping still sees the file's
/// changes in pages no one has written.
fn map_host(fd: OwnedFd, offset: u64, len: u64, writable: bool) -> Result<Window, i64> {
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let (prot, flags) = if writable {
        (rw, libc::MAP_SHARED)
    } else {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
        let read_only_fd = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE == libc::O_RDONLY;
        if read_only_fd && st.st_mode & libc::S_IFMT == libc::S_IFREG {
            (rw, libc::MAP_PRIVATE)
        } else {
            (libc::PROT_READ, libc::MAP_SHARED)
        }
    };
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, prot, flags, fd.as_raw_fd(), offset as libc::off_t) };
    if p == libc::MAP_FAILED {
        return Err(errno::from_io(&std::io::Error::last_os_error()));
    }
    Ok(Window { host: NonNull::new(p.cast()).unwrap(), len, writable, fd, offset })
}

impl SharedWindows {
    /// Encode the windows for a forked child, which maps the same objects
    /// (so they stay shared). Returns their fds, in order, to pass along.
    pub fn encode(&self, e: &mut Encoder) -> Vec<RawFd> {
        e.u64(self.windows.len() as u64);
        for (&ipa, w) in &self.windows {
            e.u64(ipa).u64(w.len).bool(w.writable).u64(w.offset);
        }
        self.windows.values().map(|w| w.fd.as_raw_fd()).collect()
    }

    pub fn decode(d: &mut Decoder, fds: &mut impl Iterator<Item = OwnedFd>) -> Result<Self, Truncated> {
        let mut s = SharedWindows::default();
        for _ in 0..d.count(32)? {
            let (ipa, len, writable, offset) = (d.u64()?, d.u64()?, d.bool()?, d.u64()?);
            let fd = fds.next().ok_or(Truncated)?;
            let w = map_host(fd, offset, len, writable).map_err(|_| Truncated)?;
            s.reserve(ipa, len).ok_or(Truncated)?;
            s.windows.insert(ipa, w);
        }
        Ok(s)
    }

    /// Take `[start, start + len)` out of the free IPA ranges.
    fn reserve(&mut self, start: u64, len: u64) -> Option<()> {
        let (&fs, &fe) = self.free.range(..=start).next_back()?;
        if fe < start + len {
            return None;
        }
        self.free.remove(&fs);
        if fs < start {
            self.free.insert(fs, start);
        }
        if start + len < fe {
            self.free.insert(start + len, fe);
        }
        Some(())
    }
}

fn map_window(ipa: u64, w: &Window) -> Result<(), HvError> {
    let flags = hv::MEMORY_READ | hv::MEMORY_EXEC | if w.writable { hv::MEMORY_WRITE } else { 0 };
    // Safety: the window stays mapped until `release` (which unmaps it from
    // the VM first) or until drop, which happens after the VM is destroyed.
    unsafe { hv::map(w.host.as_ptr(), ipa, w.len as usize, flags) }
}

impl Drop for SharedWindows {
    fn drop(&mut self) {
        // The VM is gone by now (see `crate::run`), so only the host side.
        for w in self.windows.values() {
            unsafe { libc::munmap(w.host.as_ptr().cast(), w.len as usize) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipa_ranges_are_reused_and_coalesced() {
        let mut s = SharedWindows::default();
        let a = s.create(None, 0, 4096, true).unwrap();
        let b = s.create(None, 0, 20 << 10, true).unwrap();
        assert_eq!(a, IPA_BASE);
        assert_eq!(b, IPA_BASE + HOST_PAGE, "4 KiB rounds up to one 16 KiB host page");
        assert_eq!(s.find(b + (20 << 10)), Some((b, b + 2 * HOST_PAGE)));
        s.release(a);
        s.release(b);
        assert_eq!(s.free, BTreeMap::from([(IPA_BASE, IPA_LIMIT)]));
    }

    #[test]
    fn unaligned_offset_lands_inside_the_window() {
        let mut s = SharedWindows::default();
        let pa = s.create(None, 3 * 4096, 4096, true).unwrap();
        assert_eq!(pa, IPA_BASE + 3 * 4096);
        assert_eq!(s.find(pa), Some((IPA_BASE, IPA_BASE + HOST_PAGE)));
    }

    #[test]
    fn a_decoded_copy_shares_the_same_memory() {
        let mut s = SharedWindows::default();
        let _first = s.create(None, 0, 4096, true).unwrap();
        let pa = s.create(None, 0, 8192, true).unwrap();
        let mut e = Encoder::default();
        let fds: Vec<OwnedFd> = s.encode(&mut e).into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(libc::dup(fd)) }).collect();
        let copy = SharedWindows::decode(&mut Decoder(&e.0), &mut fds.into_iter()).unwrap();
        assert_eq!(copy.ranges(), s.ranges());
        assert_eq!(copy.free, s.free);
        unsafe { *s.ptr(pa).unwrap() = 42 };
        assert_eq!(unsafe { *copy.ptr(pa).unwrap() }, 42, "shared, not copied");
    }
}
