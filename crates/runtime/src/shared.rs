//! Shared memory windows: host memory mapped `MAP_SHARED` (a file, or shared
//! anonymous memory) and placed in the VM's physical address space above the
//! private frame pool, so the guest and other host processes — a Wayland
//! compositor reading a client's `wl_shm` buffer — see the same bytes.
//!
//! The windows are the VM's (one [`SharedWindows`] per
//! [`crate::memory::Machine`]), counted by the address spaces that use them:
//! a fork inside the VM maps the parent's windows as they are, at the same
//! guest physical addresses, and a window goes once the last address space
//! using it lets it go.
//!
//! A window is of an [`Object`]: a file (by device and inode), a shared
//! memory object (by the id in its header), or shared anonymous memory. A
//! mapping of a range of an object that a window already shows (with the
//! same access) is that window: every process of a host mapping a library,
//! a font, a `wl_shm` pool or a POSIX shm segment shares one host mapping
//! and one place in guest physical memory, so the VM's stage 2 faults once
//! per host rather than once per process. Futexes in shared memory are
//! keyed by object and offset ([`SharedWindows::futex_key`]), so they're
//! one futex wherever the object is mapped.

use std::{
    collections::{BTreeMap, HashMap},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use linux_abi::{
    errno,
    wire::{Decoder, Encoder, Truncated},
    SharedKey,
};

use crate::{
    hv::{self, HvError},
    paging::POOL_SIZE,
};

/// macOS's page size: host mappings (and so windows) are aligned to it.
pub const HOST_PAGE: u64 = 16 << 10;
/// Windows live between the frame pool and the end of the VM's 40-bit IPA
/// space: `[512 GiB, 1 TiB)`.
const IPA_BASE: u64 = POOL_SIZE;
const IPA_LIMIT: u64 = 1 << hv::IPA_BITS;

/// What a window maps, so that two mappings of the same thing can be told:
/// a regular file by device and inode, a shared memory object by its
/// header's id, shared anonymous memory (and objects that can't be told, a
/// host program's shm) by a number of their own, unique to this host
/// process (its pid in the first word) and never looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Object(pub [u64; 2]);

const SHM_OBJECT: u64 = 1 << 40;
const OWN_OBJECT: u64 = 2 << 40;

impl Object {
    /// What `fd` is, if it's something other mappings can be of.
    fn of(fd: RawFd) -> Option<Object> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } < 0 {
            return None;
        }
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => Some(Object([st.st_dev as u32 as u64, st.st_ino])),
            // POSIX shm objects have no file type.
            0 => linux_abi::shm::ShmFile::of(fd).and_then(|f| f.id()).map(|id| Object([SHM_OBJECT, id])),
            _ => None,
        }
    }

    /// A new object of this host process's own.
    fn own() -> Object {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Object([OWN_OBJECT | std::process::id() as u64, NEXT.fetch_add(1, Ordering::Relaxed)])
    }

    /// Whether another mapping can be of this object (and so may find a
    /// window of it to share).
    fn shareable(&self) -> bool {
        self.0[0] & OWN_OBJECT == 0
    }
}

struct Window {
    host: NonNull<u8>,
    len: u64,
    writable: bool,
    object: Object,
    /// What's mapped, from `offset`: the guest's file, or a POSIX shm object
    /// for shared anonymous memory. Kept so a forked child can map it too.
    fd: OwnedFd,
    offset: u64,
    /// Address spaces using it.
    users: u32,
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
    /// The windows of each object (and access), by start: where a new
    /// mapping of it looks for one showing its range.
    by_object: HashMap<(Object, bool), Vec<u64>>,
    /// Free guest physical ranges, start → end.
    free: BTreeMap<u64, u64>,
    /// Whether the VM exists, so windows must be mapped into it as they're
    /// created and unmapped as they're released.
    vm_live: bool,
}

// Safety: the pointers are into host mappings the windows own; the VM's
// windows are reached only through its mutex.
unsafe impl Send for SharedWindows {}

impl Default for SharedWindows {
    fn default() -> Self {
        SharedWindows { windows: BTreeMap::new(), by_object: HashMap::new(), free: BTreeMap::from([(IPA_BASE, IPA_LIMIT)]), vm_live: false }
    }
}

impl SharedWindows {
    /// Map `len` bytes of `fd` from `offset` (or new shared anonymous memory
    /// if `fd` is None) for one address space, and return the window's start
    /// (for [`SharedWindows::release`]) and the guest physical address of
    /// `offset`. `offset` must be 4 KiB-aligned; the window itself starts at
    /// the 16 KiB host page below it. If a window of the same object with
    /// the same access shows the range already, that's the one, with one
    /// more user.
    pub fn create(&mut self, fd: Option<RawFd>, offset: u64, len: u64, writable: bool) -> Result<(u64, u64), i64> {
        let host_off = offset & !(HOST_PAGE - 1);
        let delta = offset - host_off;
        let host_len = (delta + len).next_multiple_of(HOST_PAGE);
        let object = fd.and_then(Object::of);
        if let Some(object) = object
            && let Some(start) = self.showing(object, writable, host_off, host_len)
        {
            let w = self.windows.get_mut(&start).expect("an indexed window");
            w.users += 1;
            return Ok((start, start + (offset - w.offset)));
        }
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
        let window = map_host(fd, host_off, host_len, writable, object.unwrap_or_else(Object::own))?;
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
        self.insert(ipa, window);
        Ok((ipa, ipa + delta))
    }

    /// The window of `object` (with access `writable`) that shows host
    /// range `[off, off + len)` of it, if one does.
    fn showing(&self, object: Object, writable: bool, off: u64, len: u64) -> Option<u64> {
        self.by_object.get(&(object, writable))?.iter().copied().find(|start| {
            let w = &self.windows[start];
            w.offset <= off && off + len <= w.offset + w.len
        })
    }

    fn insert(&mut self, ipa: u64, window: Window) {
        if window.object.shareable() {
            self.by_object.entry((window.object, window.writable)).or_default().push(ipa);
        }
        self.windows.insert(ipa, window);
    }

    /// The key of a futex at guest physical address `pa` in a window: the
    /// object and the offset in it, the same wherever it's mapped.
    pub fn futex_key(&self, pa: u64) -> Option<SharedKey> {
        let (start, _) = self.find(pa)?;
        let w = &self.windows[&start];
        Some(SharedKey { object: w.object.0, offset: w.offset + (pa - start) })
    }

    /// How many windows there are (for tests and measurements).
    pub fn count(&self) -> usize {
        self.windows.len()
    }

    /// Another address space uses the window starting at `start` (a fork
    /// inside the VM).
    pub fn add_user(&mut self, start: u64) {
        self.windows.get_mut(&start).expect("no such window").users += 1;
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

    /// Unmap every window from the VM, before it goes.
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

    /// An address space is done with the window starting at `start`: once
    /// none uses it, it's unmapped and freed.
    pub fn release(&mut self, start: u64) {
        let w = self.windows.get_mut(&start).expect("no such window");
        w.users -= 1;
        if w.users > 0 {
            return;
        }
        let w = self.windows.remove(&start).unwrap();
        if let Some(starts) = self.by_object.get_mut(&(w.object, w.writable)) {
            starts.retain(|&s| s != start);
            if starts.is_empty() {
                self.by_object.remove(&(w.object, w.writable));
            }
        }
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
fn map_host(fd: OwnedFd, offset: u64, len: u64, writable: bool, object: Object) -> Result<Window, i64> {
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
    Ok(Window { host: NonNull::new(p.cast()).unwrap(), len, writable, object, fd, offset, users: 1 })
}

impl SharedWindows {
    /// Encode the windows starting at `starts` for a child in another
    /// process (1:1 fork), which maps the same objects (so they stay
    /// shared). Returns their fds, in order, to pass along.
    pub fn encode<'a>(&self, e: &mut Encoder, starts: impl ExactSizeIterator<Item = &'a u64>) -> Vec<RawFd> {
        e.u64(starts.len() as u64);
        let mut fds = Vec::new();
        for ipa in starts {
            let w = &self.windows[ipa];
            e.u64(*ipa).u64(w.len).bool(w.writable).u64(w.offset).u64(w.object.0[0]).u64(w.object.0[1]);
            fds.push(w.fd.as_raw_fd());
        }
        fds
    }

    /// The child's side of [`SharedWindows::encode`], into a VM's windows
    /// (none yet): each is used by the one address space. Returns their
    /// starts.
    pub fn decode(&mut self, d: &mut Decoder, fds: &mut impl Iterator<Item = OwnedFd>) -> Result<Vec<u64>, Truncated> {
        let mut starts = Vec::new();
        for _ in 0..d.count(32)? {
            let (ipa, len, writable, offset) = (d.u64()?, d.u64()?, d.bool()?, d.u64()?);
            let object = Object([d.u64()?, d.u64()?]);
            let fd = fds.next().ok_or(Truncated)?;
            let w = map_host(fd, offset, len, writable, object).map_err(|_| Truncated)?;
            self.reserve(ipa, len).ok_or(Truncated)?;
            self.insert(ipa, w);
            starts.push(ipa);
        }
        Ok(starts)
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
        let a = s.create(None, 0, 4096, true).unwrap().1;
        let b = s.create(None, 0, 20 << 10, true).unwrap().1;
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
        let (start, pa) = s.create(None, 3 * 4096, 4096, true).unwrap();
        assert_eq!(start, IPA_BASE);
        assert_eq!(pa, IPA_BASE + 3 * 4096);
        assert_eq!(s.find(pa), Some((IPA_BASE, IPA_BASE + HOST_PAGE)));
    }

    #[test]
    fn a_decoded_copy_shares_the_same_memory() {
        let mut s = SharedWindows::default();
        let (first, _) = s.create(None, 0, 4096, true).unwrap();
        let (second, pa) = s.create(None, 0, 8192, true).unwrap();
        let mut e = Encoder::default();
        let fds: Vec<OwnedFd> =
            s.encode(&mut e, [first, second].iter()).into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(libc::dup(fd)) }).collect();
        let mut copy = SharedWindows::default();
        assert_eq!(copy.decode(&mut Decoder(&e.0), &mut fds.into_iter()).unwrap(), vec![first, second]);
        assert_eq!(copy.ranges(), s.ranges());
        assert_eq!(copy.free, s.free);
        unsafe { *s.ptr(pa).unwrap() = 42 };
        assert_eq!(unsafe { *copy.ptr(pa).unwrap() }, 42, "shared, not copied");
    }

    #[test]
    fn windows_go_with_their_last_user() {
        let mut s = SharedWindows::default();
        let (start, _) = s.create(None, 0, 4096, true).unwrap();
        s.add_user(start);
        s.release(start);
        assert_eq!(s.ranges().len(), 1, "still used");
        s.release(start);
        assert!(s.ranges().is_empty());
        assert_eq!(s.free, BTreeMap::from([(IPA_BASE, IPA_LIMIT)]));
    }
}
