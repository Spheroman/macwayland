//! `mmap`; the other memory calls go straight to the core.

use super::{last_errno, neg, Process};
use crate::{abi::mm, errno, memory::GuestMemory, shm::ShmFile};

/// A host POSIX shm object (not one of ours), mapped read-only for copying.
struct ForeignShm {
    ptr: *mut u8,
    /// Bytes of the object from the requested offset.
    avail: usize,
    map_len: usize,
    delta: usize,
}

impl ForeignShm {
    fn read(&self, at: u64, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.avail.saturating_sub(at as usize));
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(self.delta + at as usize), buf.as_mut_ptr(), n) };
        n
    }
}

impl Drop for ForeignShm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.map_len) };
    }
}

fn foreign_shm(host: i32, offset: u64, len: u64) -> Option<ForeignShm> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // POSIX shm objects have no file type.
    if unsafe { libc::fstat(host, &mut st) } < 0 || st.st_mode & libc::S_IFMT != 0 {
        return None;
    }
    let size = st.st_size as u64;
    let start = offset & !(16384 - 1);
    let end = size.min(offset.checked_add(len)?);
    if end <= offset {
        return None;
    }
    let map_len = (end - start) as usize;
    let p = unsafe { libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ, libc::MAP_SHARED, host, start as libc::off_t) };
    if p == libc::MAP_FAILED {
        return None;
    }
    Some(ForeignShm { ptr: p.cast(), avail: (end - offset) as usize, map_len, delta: (offset - start) as usize })
}

/// `madvise`: dropping pages (DONTNEED, FREE: glibc does it to finished
/// threads' stacks) is done; the rest is advice, taken as given.
pub(super) fn madvise<M: GuestMemory + ?Sized>(p: &Process, mem: &M, addr: u64, len: u64, advice: u64) -> Result<i64, i64> {
    const DONTNEED: u64 = 4;
    const FREE: u64 = 8;
    if !addr.is_multiple_of(4096) {
        return Err(errno::EINVAL);
    }
    let len = len.next_multiple_of(4096);
    match advice {
        DONTNEED | FREE => neg(mem.discard(addr, len)),
        // NORMAL, RANDOM, SEQUENTIAL, WILLNEED, REMOVE..., MERGEABLE,
        // HUGEPAGE, NOHUGEPAGE, DONTDUMP, DODUMP, COLD, PAGEOUT...
        0..=3 | 9..=21 => Ok(0),
        // Guard pages (Linux 6.13): kernels without them say EINVAL, and
        // glibc falls back on mprotect.
        102 | 103 => Err(errno::EINVAL),
        _ => {
            p.unimplemented(format!("madvise advice {advice}"));
            Err(errno::EINVAL)
        }
    }
}

/// The files mapped where, for `/proc/self/maps` (and for naming where a
/// fault was): start to (end, guest path, offset in the file).
#[derive(Debug, Default)]
pub struct FileMaps(std::collections::BTreeMap<u64, (u64, Vec<u8>, u64)>);

impl FileMaps {
    /// Nothing is mapped in `[start, end)` any more: trim what overlaps.
    pub(super) fn forget(&mut self, start: u64, end: u64) {
        let overlapping: Vec<u64> = self.0.range(..end).filter(|(_, (e, ..))| *e > start).map(|(&s, _)| s).collect();
        for s in overlapping {
            let (e, path, offset) = self.0.remove(&s).unwrap();
            if s < start {
                self.0.insert(s, (start, path.clone(), offset));
            }
            if e > end {
                self.0.insert(end, (e, path, offset + (end - s)));
            }
        }
    }

    pub(super) fn record(&mut self, start: u64, end: u64, path: Vec<u8>, offset: u64) {
        self.forget(start, end);
        self.0.insert(start, (end, path, offset));
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// For a forked child (whose memory is a copy).
    pub(super) fn encode(&self, e: &mut crate::wire::Encoder) {
        e.u64(self.0.len() as u64);
        for (&start, (end, path, offset)) in &self.0 {
            e.u64(start).u64(*end).bytes(path).u64(*offset);
        }
    }

    pub(super) fn decode(d: &mut crate::wire::Decoder) -> Result<Self, crate::wire::Truncated> {
        let mut maps = FileMaps::default();
        for _ in 0..d.count(32)? {
            let (start, end, path, offset) = (d.u64()?, d.u64()?, d.bytes()?, d.u64()?);
            maps.0.insert(start, (end, path, offset));
        }
        Ok(maps)
    }

    /// The file mapped at `addr`, and the offset in it.
    pub fn at(&self, addr: u64) -> Option<(&[u8], u64)> {
        let (&s, (e, path, offset)) = self.0.range(..=addr).next_back()?;
        (addr < *e).then(|| (path.as_slice(), offset + (addr - s)))
    }

    /// The mappings overlapping `[start, end)`, in order: (start, end, path,
    /// offset), clipped to it.
    pub(crate) fn within(&self, start: u64, end: u64) -> Vec<(u64, u64, &[u8], u64)> {
        self.0
            .range(..end)
            .filter(|(_, (e, ..))| *e > start)
            .map(|(&s, (e, path, offset))| {
                let from = s.max(start);
                (from, (*e).min(end), path.as_slice(), offset + (from - s))
            })
            .collect()
    }
}

/// Anonymous and shared mappings go straight to the core, and so do private
/// read-only mappings of regular files where it can ([`GuestMemory::mmap_file`]).
/// Other private file mappings are emulated by reading the file into an
/// anonymous mapping, which is exact since nobody else sees them.
pub(super) fn mmap<M: GuestMemory + ?Sized>(p: &Process, mem: &M, args: [u64; 6]) -> Result<i64, i64> {
    let [addr, len, prot, flags, fd, offset] = args;
    let kind = flags & mm::MAP_TYPE;
    if len == 0 || !offset.is_multiple_of(4096) || !matches!(kind, mm::MAP_SHARED | mm::MAP_PRIVATE | mm::MAP_SHARED_VALIDATE) {
        return Err(errno::EINVAL);
    }
    let shared = kind != mm::MAP_PRIVATE;
    if flags & mm::MAP_ANONYMOUS != 0 {
        let base = neg(if shared {
            mem.mmap_shared(addr, len, prot, flags, None, 0)
        } else {
            mem.mmap_anonymous(addr, len, prot, flags)
        })?;
        p.file_maps().forget(base as u64, base as u64 + len.next_multiple_of(4096));
        return Ok(base);
    }
    let host = p.fds.host(fd)?;
    let path = p.fds.get(fd)?.path.clone();
    // A memfd sealed against writing can't be mapped shared and writable.
    if shared && prot & mm::PROT_WRITE != 0 && ShmFile::of(host).is_some_and(|f| f.write_sealed()) {
        return Err(errno::EPERM);
    }
    let named = |base: i64| {
        if let Some(path) = path {
            p.file_maps().record(base as u64, base as u64 + len.next_multiple_of(4096), path, offset);
        }
        base
    };
    if shared {
        return neg(mem.mmap_shared(addr, len, prot, flags, Some(host), offset)).map(named);
    }
    // Shared memory files can't be read, only mapped (see crate::shm); nor
    // can other programs' POSIX shm objects (a host process's fd).
    let shm = ShmFile::of(host);
    let foreign = if shm.is_none() { foreign_shm(host, offset, len) } else { None };
    // A regular file mapped read-only (libraries, fonts) isn't read in at all
    // if the memory can map it as it is.
    if prot & mm::PROT_WRITE == 0 && shm.is_none() && foreign.is_none() && regular_file(host) {
        let base = mem.mmap_file(addr, len, prot, flags, host, offset);
        if base >= 0 {
            return Ok(named(base));
        }
    }
    let base = neg(mem.mmap_anonymous(addr, len, prot | mm::PROT_WRITE, flags))? as u64;
    let mut buf = vec![0u8; len.min(1 << 20) as usize];
    let mut done = 0;
    while done < len {
        let want = buf.len().min((len - done) as usize);
        let n = match (&shm, &foreign) {
            (Some(f), _) => f.read_at(offset + done, &mut buf[..want]).map_or(-1, |n| n as isize),
            (None, Some(m)) => m.read(done, &mut buf[..want]) as isize,
            (None, None) => unsafe { libc::pread(host, buf.as_mut_ptr().cast(), want, (offset + done) as libc::off_t) },
        };
        if n < 0 {
            let e = last_errno();
            mem.munmap(base, len);
            return Err(e);
        }
        if n == 0 {
            break; // past EOF the mapping stays zero
        }
        mem.write(base + done, &buf[..n as usize])?;
        done += n as u64;
    }
    if prot & mm::PROT_WRITE == 0 {
        neg(mem.mprotect(base, len, prot))?;
    }
    Ok(named(base as i64))
}

fn regular_file(host: i32) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(host, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFREG }
}

/// `munmap`.
pub(super) fn munmap<M: GuestMemory + ?Sized>(p: &Process, mem: &M, addr: u64, len: u64) -> Result<i64, i64> {
    let r = neg(mem.munmap(addr, len))?;
    p.file_maps().forget(addr, addr.saturating_add(len.next_multiple_of(4096)));
    Ok(r)
}
