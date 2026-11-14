//! Linux shared-memory files — `memfd_create` and `/dev/shm` (`shm_open`) —
//! as macOS POSIX shared memory objects, which live in RAM.
//!
//! The obvious host equivalent, an unlinked temp file, works but costs SSD
//! writes: macOS writes dirty pages of file mappings back to disk every ~15 s,
//! and these are what Wayland clients draw into (a full-screen client's
//! buffers would mean several GB an hour). POSIX shm objects never go to
//! disk, but they're limited: their size is fixed by the first `ftruncate`,
//! and they can only be mapped shared — no `read`/`write`/`lseek`, no private
//! mappings. So each object is made [`OBJECT_SIZE`] large up front (free
//! until touched) and the Linux file is emulated on top:
//!
//! - the Linux-visible size is kept in a header in the object's last page, so
//!   every holder agrees (dup'd and forked fds, other processes it's passed
//!   to, and the compositor), and so are its seals (`F_ADD_SEALS`), which
//!   are enforced as Linux does for shrinking, growing and writing;
//! - reads and writes copy through a mapping; the file position belongs to
//!   the fd entry (shared by `dup`, as Linux's open file description is);
//! - the header names the object too (`id`, random), as an inode names a
//!   file: macOS's `fstat` gives shm objects no inode, and the runtime
//!   tells two mappings of one object by it (one shared window for both,
//!   and one futex in it for every process);
//! - private mappings copy, like other private file mappings.
//!
//! Host processes the fd is passed to (the compositor) just map it.

use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::errno;

/// How big every object is: the most a Linux file on it can hold, plus the
/// header page. Only touched pages cost memory.
pub const OBJECT_SIZE: u64 = 8 << 30;
/// The header lives in the last host page.
const HEADER_PAGE: u64 = 16 << 10;
/// Largest Linux file size.
pub const CAPACITY: u64 = OBJECT_SIZE - HEADER_PAGE;
const MAGIC: u64 = u64::from_le_bytes(*b"macwlshm");

#[repr(C)]
struct Header {
    magic: AtomicU64,
    size: AtomicU64,
    /// `F_SEAL_*`.
    seals: AtomicU64,
    /// The object's identity: random, never 0 once set.
    id: AtomicU64,
}

/// Seals (`F_ADD_SEALS`).
pub const SEAL_SEAL: u64 = 0x1;
pub const SEAL_SHRINK: u64 = 0x2;
pub const SEAL_GROW: u64 = 0x4;
pub const SEAL_WRITE: u64 = 0x8;
pub const SEAL_FUTURE_WRITE: u64 = 0x10;
pub const SEAL_EXEC: u64 = 0x20;
const SEALS_ALL: u64 = 0x3f;

fn last_errno() -> i64 {
    errno::from_io(&std::io::Error::last_os_error())
}

/// A new anonymous object (for `memfd_create`), empty.
pub fn create() -> Result<OwnedFd, i64> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!("/macwl.m.{}.{n}", std::process::id())).unwrap();
        match open_host(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) {
            Ok(fd) => {
                unsafe { libc::shm_unlink(name.as_ptr()) };
                return Ok(fd);
            }
            Err(errno::EEXIST) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(errno::EEXIST)
}

/// macOS's name for the Linux `/dev/shm/<name>`: names are at most 31 bytes
/// there, so long ones are hashed.
fn host_name(name: &[u8]) -> CString {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for &b in name {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    CString::new(format!("/macwl.s.{h:016x}")).unwrap()
}

/// `open("/dev/shm/<name>", flags, mode)`, with Darwin open flags.
pub fn open_named(name: &[u8], flags: i32, mode: u32) -> Result<OwnedFd, i64> {
    if name.is_empty() || name.contains(&b'/') {
        return Err(errno::EINVAL);
    }
    // macOS shm objects can't be opened write-only.
    let access = if flags & libc::O_ACCMODE == libc::O_RDONLY { libc::O_RDONLY } else { libc::O_RDWR };
    let fd = open_host(&host_name(name), access | (flags & (libc::O_CREAT | libc::O_EXCL)), mode)?;
    if flags & libc::O_TRUNC != 0 {
        ShmFile::of(fd.as_raw_fd()).ok_or(errno::EINVAL)?.set_size(0)?;
    }
    Ok(fd)
}

pub fn unlink_named(name: &[u8]) -> Result<(), i64> {
    let name = host_name(name);
    if unsafe { libc::shm_unlink(name.as_ptr()) } < 0 { Err(last_errno()) } else { Ok(()) }
}

/// Open (or create) host object `name`. A new one is sized and given its
/// header.
fn open_host(name: &CString, flags: i32, mode: u32) -> Result<OwnedFd, i64> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(last_errno());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    if st.st_size == 0 {
        // Ours to set up (a new object; macOS sizes it once).
        if unsafe { libc::ftruncate(fd.as_raw_fd(), OBJECT_SIZE as libc::off_t) } < 0 {
            return Err(last_errno());
        }
        let file = ShmFile::of_unchecked(fd.as_raw_fd(), true)?;
        file.header().size.store(0, Ordering::Relaxed);
        // Unsealable, as Linux's tmpfs files are; memfd_create with
        // MFD_ALLOW_SEALING opens it up (`allow_sealing`).
        file.header().seals.store(SEAL_SEAL, Ordering::Relaxed);
        file.header().id.store(new_id(), Ordering::Relaxed);
        file.header().magic.store(MAGIC, Ordering::Release);
    }
    Ok(fd)
}

/// A new object's identity: random (objects are made by many host
/// processes, and their names reused), and not 0, which means none yet.
fn new_id() -> u64 {
    let mut id = 0u64;
    while id == 0 {
        unsafe { libc::arc4random_buf((&mut id as *mut u64).cast(), 8) };
    }
    id
}

/// Map `[off, off + len)` of `fd` (`off` 16 KiB-aligned).
fn map(fd: RawFd, off: u64, len: u64, writable: bool) -> Result<*mut u8, i64> {
    let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, prot, libc::MAP_SHARED, fd, off as libc::off_t) };
    if p == libc::MAP_FAILED { Err(last_errno()) } else { Ok(p.cast()) }
}

/// A host fd that is one of these objects, with its header mapped.
pub struct ShmFile {
    fd: RawFd,
    header: *mut Header,
    writable: bool,
}

impl ShmFile {
    /// `host` as a shared-memory file, if it is one.
    pub fn of(host: RawFd) -> Option<ShmFile> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // POSIX shm objects are the fds with no file type at all.
        if unsafe { libc::fstat(host, &mut st) } < 0 || st.st_mode & libc::S_IFMT != 0 || st.st_size as u64 != OBJECT_SIZE {
            return None;
        }
        let flags = unsafe { libc::fcntl(host, libc::F_GETFL) };
        let file = Self::of_unchecked(host, flags & libc::O_ACCMODE != libc::O_RDONLY).ok()?;
        (file.header().magic.load(Ordering::Acquire) == MAGIC).then_some(file)
    }

    fn of_unchecked(fd: RawFd, writable: bool) -> Result<ShmFile, i64> {
        let header = map(fd, OBJECT_SIZE - HEADER_PAGE, HEADER_PAGE, writable)?.cast();
        Ok(ShmFile { fd, header, writable })
    }

    fn header(&self) -> &Header {
        unsafe { &*self.header }
    }

    pub fn size(&self) -> u64 {
        self.header().size.load(Ordering::Acquire)
    }

    /// The object's identity, the same through every fd and mapping of it
    /// (an object made before there were ids gets one now, through a
    /// writable fd; through a read-only one it has none).
    pub fn id(&self) -> Option<u64> {
        let id = self.header().id.load(Ordering::Acquire);
        if id != 0 || !self.writable {
            return (id != 0).then_some(id);
        }
        let new = new_id();
        match self.header().id.compare_exchange(0, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Some(new),
            Err(theirs) => Some(theirs),
        }
    }

    /// The seals (`F_GET_SEALS`).
    pub fn seals(&self) -> u64 {
        self.header().seals.load(Ordering::Acquire)
    }

    /// A new memfd that may be sealed (MFD_ALLOW_SEALING), starting with
    /// `seals`.
    pub fn allow_sealing(&self, seals: u64) {
        self.header().seals.store(seals, Ordering::Release);
    }

    /// `F_ADD_SEALS`: EPERM once sealed against more (F_SEAL_SEAL), or
    /// through an fd not open for writing.
    pub fn add_seals(&self, seals: u64) -> Result<(), i64> {
        if seals & !SEALS_ALL != 0 {
            return Err(errno::EINVAL);
        }
        if !self.writable {
            return Err(errno::EPERM);
        }
        self.header()
            .seals
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| (old & SEAL_SEAL == 0).then_some(old | seals))
            .map(|_| ())
            .map_err(|_| errno::EPERM)
    }

    /// Whether a seal forbids writing (and writable shared mappings).
    pub fn write_sealed(&self) -> bool {
        self.seals() & (SEAL_WRITE | SEAL_FUTURE_WRITE) != 0
    }

    /// `ftruncate`: what's cut off reads as zeros if the file grows again.
    pub fn set_size(&self, size: u64) -> Result<(), i64> {
        if !self.writable {
            return Err(errno::EINVAL);
        }
        if size > CAPACITY {
            return Err(errno::EFBIG);
        }
        let current = self.size();
        if (size < current && self.seals() & SEAL_SHRINK != 0) || (size > current && self.seals() & SEAL_GROW != 0) {
            return Err(errno::EPERM);
        }
        let old = self.header().size.swap(size, Ordering::AcqRel);
        if size < old {
            self.zero(size, old - size)?;
        }
        Ok(())
    }

    fn zero(&self, off: u64, len: u64) -> Result<(), i64> {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(16 << 20);
            self.with_range(off + done, n, true, |s| s.fill(0))?;
            done += n;
        }
        Ok(())
    }

    /// Run `f` on `[off, off + len)` of the object, mapped.
    fn with_range(&self, off: u64, len: u64, writable: bool, f: impl FnOnce(&mut [u8])) -> Result<(), i64> {
        let start = off & !(HEADER_PAGE - 1);
        let map_len = (off + len - start).next_multiple_of(HEADER_PAGE);
        let p = map(self.fd, start, map_len, writable)?;
        f(unsafe { std::slice::from_raw_parts_mut(p.add((off - start) as usize), len as usize) });
        unsafe { libc::munmap(p.cast(), map_len as usize) };
        Ok(())
    }

    /// `pread`: up to `buf.len()` bytes at `off`, short at the end of file.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let n = (buf.len() as u64).min(self.size().saturating_sub(off));
        if n > 0 {
            self.with_range(off, n, false, |s| buf[..n as usize].copy_from_slice(s))?;
        }
        Ok(n as usize)
    }

    /// `pwrite`: grows the file as needed.
    pub fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        if !self.writable {
            return Err(errno::EBADF);
        }
        let end = off.checked_add(data.len() as u64).ok_or(errno::EFBIG)?;
        if end > CAPACITY {
            return Err(errno::EFBIG);
        }
        if self.write_sealed() || (end > self.size() && self.seals() & SEAL_GROW != 0) {
            return Err(errno::EPERM);
        }
        if !data.is_empty() {
            self.with_range(off, data.len() as u64, true, |s| s.copy_from_slice(data))?;
        }
        self.header().size.fetch_max(end, Ordering::AcqRel);
        Ok(data.len())
    }

    /// What `fstat` should say: a regular file of the Linux size.
    pub fn stat(&self) -> libc::stat {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        unsafe { libc::fstat(self.fd, &mut st) };
        st.st_mode |= libc::S_IFREG;
        st.st_size = self.size() as libc::off_t;
        // Its inode, as a tmpfs file would have one.
        st.st_ino = self.id().unwrap_or(0);
        st.st_blksize = 4096;
        st.st_blocks = self.size().div_ceil(512) as i64;
        st.st_nlink = 1;
        st
    }
}

impl Drop for ShmFile {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.header.cast(), HEADER_PAGE as usize) };
    }
}

/// `fstat` for any host fd, with shared-memory files shown as Linux sees them.
pub fn fstat(host: RawFd) -> Result<libc::stat, i64> {
    if let Some(f) = ShmFile::of(host) {
        return Ok(f.stat());
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(host, &mut st) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_a_file_and_maps_shared() {
        let fd = create().unwrap();
        let f = ShmFile::of(fd.as_raw_fd()).expect("recognised");
        assert_eq!(f.size(), 0);
        assert_eq!(f.write_at(5, b"hello").unwrap(), 5);
        assert_eq!(f.size(), 10);
        let mut buf = [0u8; 16];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 10);
        assert_eq!(&buf[..10], b"\0\0\0\0\0hello");
        // Shrink and regrow: the tail reads as zeros.
        f.set_size(7).unwrap();
        f.set_size(10).unwrap();
        assert_eq!(f.read_at(5, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"he\0\0\0");
        assert_eq!(f.set_size(CAPACITY + 1), Err(errno::EFBIG));
        let st = f.stat();
        assert_eq!((st.st_mode & libc::S_IFMT, st.st_size), (libc::S_IFREG, 10));
        // Another mapping of the same object (the compositor's) sees writes.
        let p = map(fd.as_raw_fd(), 0, 16 << 10, false).unwrap();
        assert_eq!(unsafe { std::slice::from_raw_parts(p.add(5), 2) }, b"he");
        // Plain files aren't mistaken for one.
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(ShmFile::of(file.as_raw_fd()).is_none());
    }

    #[test]
    fn named_objects_are_shared_and_unlinkable() {
        let name = format!("macwl-test-{}", std::process::id());
        let a = open_named(name.as_bytes(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600).unwrap();
        ShmFile::of(a.as_raw_fd()).unwrap().write_at(0, b"x").unwrap();
        let b = open_named(name.as_bytes(), libc::O_RDONLY, 0).unwrap();
        let mut c = [0u8];
        assert_eq!(ShmFile::of(b.as_raw_fd()).unwrap().read_at(0, &mut c).unwrap(), 1);
        assert_eq!(c, *b"x");
        assert_eq!(open_named(name.as_bytes(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600).err(), Some(errno::EEXIST));
        unlink_named(name.as_bytes()).unwrap();
        assert_eq!(open_named(name.as_bytes(), libc::O_RDONLY, 0).err(), Some(errno::ENOENT));
    }
}
