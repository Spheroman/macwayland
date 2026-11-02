//! Calls that take paths: `openat`, `stat`, directories, links, permissions,
//! the working directory. Paths are resolved by [`crate::fs::Filesystem`] and
//! then handed to the host as absolute host paths.

#![allow(clippy::too_many_arguments)]

use std::{
    os::unix::ffi::OsStrExt,
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use super::{ok, path_arg, Process};
use crate::{
    abi::{self, at, fcntl, o, Dirent64, Stat, Timespec, DIRENT64_NAME_OFFSET},
    errno,
    fs::{DirEntry, DirSnapshot, FdEntry, Resolved, Target},
    memory::{GuestMemory, GuestMemoryExt},
    shm,
    proc,
};

/// Resolve a (dirfd, path) argument pair.
fn resolve<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, follow: bool) -> Result<Resolved, i64> {
    let path = path_arg(mem, path)?;
    p.fs.resolve(&p.fds, dirfd, &path, follow)
}

/// The host path, or EACCES for `/proc` (which can't be modified) and
/// shared memory objects (which are only opened and unlinked).
fn host_only(r: Resolved) -> Result<CString, i64> {
    match r.target {
        Target::Host(h) => Ok(h),
        Target::Proc(_) | Target::Shm(_) => Err(errno::EACCES),
    }
}

fn write_stat<M: GuestMemory + ?Sized>(mem: &M, buf: u64, st: &libc::stat) -> Result<i64, i64> {
    mem.write_pod(buf, &Stat::from_darwin(st))?;
    Ok(0)
}

pub(super) fn openat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, flags: u64, mode: u64) -> Result<i64, i64> {
    const O_TMPFILE: u64 = 0o20000000;
    if flags & O_TMPFILE != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    let r = match resolve(p, mem, dirfd, path, flags & o::NOFOLLOW == 0) {
        Err(errno::ENOENT) if flags & 3 == 0 => {
            // Perhaps a file the root lacks, along with its directory.
            let guest = p.fs.absolute(&path_arg(mem, path)?);
            let contents = stand_in(p, &guest).ok_or(errno::ENOENT)?;
            return Ok(p.fds.insert(proc::generated(contents.as_bytes())?.path(guest).cloexec(flags & o::CLOEXEC != 0)));
        }
        r => r?,
    };
    // /etc/passwd and /etc/group, read: with the user added (crate::identity).
    // (Only those are read: others may be big, or never end, like /dev/urandom.)
    if flags & 3 == 0
        && crate::identity::extends(&r.guest)
        && let Target::Host(h) = &r.target
        && let Ok(contents) = std::fs::read(std::ffi::OsStr::from_bytes(h.as_bytes()))
        && let Some(extended) = crate::identity::extend(&r.guest, &contents, |path| {
            p.fs.resolve(&p.fds, at::FDCWD as u64, path, true).is_ok_and(|r| matches!(r.target, Target::Host(h) if unsafe { libc::access(h.as_ptr(), libc::X_OK) } == 0))
        })
    {
        let fd = shm::create()?;
        shm::ShmFile::of(fd.as_raw_fd()).ok_or(errno::EIO)?.write_at(0, &extended)?;
        return Ok(p.fds.insert(FdEntry::new(fd).path(r.guest).cloexec(flags & o::CLOEXEC != 0)));
    }
    let entry = match r.target {
        Target::Host(h) => {
            let mode = mode as u32 & !p.umask();
            let fd = match ok(unsafe { libc::open(h.as_ptr(), abi::open_flags_to_darwin(flags), mode as libc::c_uint) } as i64) {
                Ok(fd) => fd as i32,
                Err(errno::ENOENT) if flags & 3 == 0 && let Some(contents) = stand_in(p, &r.guest) => {
                    return Ok(p.fds.insert(proc::generated(contents.as_bytes())?.path(r.guest).cloexec(flags & o::CLOEXEC != 0)));
                }
                Err(e) => return Err(e),
            };
            super::tty::slave_opened(p, h.as_bytes(), fd);
            let guest = if p.fs.is_host_root() { host_fd_path(fd).unwrap_or(r.guest) } else { r.guest };
            FdEntry::new(unsafe { OwnedFd::from_raw_fd(fd) }).path(guest)
        }
        Target::Proc(rest) => proc::open(p, mem, &rest)?,
        Target::Shm(name) => {
            let fd = shm::open_named(&name, abi::open_flags_to_darwin(flags), mode as u32 & !p.umask())?;
            FdEntry::new(fd).path(r.guest)
        }
    };
    Ok(p.fds.insert(entry.cloexec(flags & o::CLOEXEC != 0)))
}

/// What a root without them (the Mac's `/`) gets for files Linux programs
/// expect: `/etc/os-release` (saying what this is), `/etc/mtab`, and the
/// machine id (D-Bus and systemd's libraries want one).
fn stand_in(p: &Process, guest: &[u8]) -> Option<String> {
    match guest {
        b"/etc/os-release" | b"/usr/lib/os-release" => Some(crate::hostinfo::os_release()),
        b"/etc/machine-id" | b"/var/lib/dbus/machine-id" => crate::hostinfo::machine_id(),
        b"/etc/mtab" => Some(proc::mounts(p, false)),
        _ => None,
    }
}

/// The canonical host path of an open file (with a host root, also its guest
/// path): one `fcntl`, where resolving it ourselves would take a walk.
pub(super) fn host_fd_path(fd: i32) -> Option<Vec<u8>> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
        return None;
    }
    Some(std::ffi::CStr::from_bytes_until_nul(&buf).ok()?.to_bytes().to_vec())
}

pub(super) fn fstat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, buf: u64) -> Result<i64, i64> {
    write_stat(mem, buf, &shm::fstat(p.fds.host(fd)?)?)
}

pub(super) fn fstatat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, buf: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(at::SYMLINK_NOFOLLOW | at::EMPTY_PATH | at::NO_AUTOMOUNT) != 0 {
        return Err(errno::EINVAL);
    }
    let st = stat_at(p, mem, dirfd, path, flags)?;
    write_stat(mem, buf, &st)
}

/// `stat` for the `*at` calls: `dirfd` itself with AT_EMPTY_PATH and an
/// empty path, else the path relative to it.
fn stat_at<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, flags: u64) -> Result<libc::stat, i64> {
    let name = path_arg(mem, path)?;
    let follow = flags & at::SYMLINK_NOFOLLOW == 0;
    let r = if name.is_empty() && flags & at::EMPTY_PATH != 0 {
        if dirfd as i64 != at::FDCWD {
            return shm::fstat(p.fds.host(dirfd)?);
        }
        let cwd = p.fs.cwd().to_vec();
        p.fs.resolve(&p.fds, dirfd, &cwd, true)?
    } else {
        p.fs.resolve(&p.fds, dirfd, &name, follow)?
    };
    match r.target {
        Target::Host(h) => {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let f = if follow { libc::stat } else { libc::lstat };
            ok(unsafe { f(h.as_ptr(), &mut st) } as i64)?;
            Ok(st)
        }
        Target::Shm(name) => shm::fstat(shm::open_named(&name, libc::O_RDONLY, 0)?.as_raw_fd()),
        Target::Proc(rest) => proc::stat(p, &rest),
    }
}

/// `statx`: everything `stat` knows, plus the birth time Darwin keeps.
pub(super) fn statx<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, flags: u64, _mask: u64, buf: u64) -> Result<i64, i64> {
    const SYNC_TYPE: u64 = 0x6000; // AT_STATX_{FORCE,DONT}_SYNC: advisory
    if flags & !(at::SYMLINK_NOFOLLOW | at::EMPTY_PATH | at::NO_AUTOMOUNT | SYNC_TYPE) != 0 {
        return Err(errno::EINVAL);
    }
    let st = stat_at(p, mem, dirfd, path, flags)?;
    write_statx(mem, buf, &st)
}

fn write_statx<M: GuestMemory + ?Sized>(mem: &M, buf: u64, st: &libc::stat) -> Result<i64, i64> {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    const STATX_BTIME: u32 = 0x800;
    let mut out = [0u8; 256];
    let mut put = |off: usize, bytes: &[u8]| out[off..off + bytes.len()].copy_from_slice(bytes);
    put(0, &(STATX_BASIC_STATS | STATX_BTIME).to_le_bytes());
    put(4, &(st.st_blksize as u32).to_le_bytes());
    put(16, &(st.st_nlink as u32).to_le_bytes());
    put(20, &st.st_uid.to_le_bytes());
    put(24, &st.st_gid.to_le_bytes());
    put(28, &st.st_mode.to_le_bytes());
    put(32, &st.st_ino.to_le_bytes());
    put(40, &st.st_size.to_le_bytes());
    put(48, &st.st_blocks.to_le_bytes());
    let times = [
        (64, st.st_atime, st.st_atime_nsec),
        (80, st.st_birthtime, st.st_birthtime_nsec),
        (96, st.st_ctime, st.st_ctime_nsec),
        (112, st.st_mtime, st.st_mtime_nsec),
    ];
    for (off, sec, nsec) in times {
        put(off, &sec.to_le_bytes());
        put(off + 8, &(nsec as u32).to_le_bytes());
    }
    // Darwin's dev_t: 8-bit major, 24-bit minor.
    let split = |d: libc::dev_t| ((d as u32 >> 24) & 0xff, d as u32 & 0xff_ffff);
    let (rmaj, rmin) = split(st.st_rdev);
    let (dmaj, dmin) = split(st.st_dev);
    for (off, v) in [(128, rmaj), (132, rmin), (136, dmaj), (140, dmin)] {
        put(off, &v.to_le_bytes());
    }
    mem.write(buf, &out)?;
    Ok(0)
}

/// `statfs`/`fstatfs`. Host filesystems are reported as ext4, the type
/// Linux programs treat as an ordinary local disk; `/proc` as procfs.
pub(super) fn statfs<M: GuestMemory + ?Sized>(p: &Process, mem: &M, path: u64, buf: u64) -> Result<i64, i64> {
    const PROC_SUPER_MAGIC: i64 = 0x9fa0;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    match resolve(p, mem, at::FDCWD as u64, path, true)?.target {
        Target::Host(h) => {
            let mut st: libc::statfs = unsafe { std::mem::zeroed() };
            ok(unsafe { libc::statfs(h.as_ptr(), &mut st) } as i64)?;
            write_statfs(mem, buf, &st, None)
        }
        Target::Proc(_) => write_statfs(mem, buf, &unsafe { std::mem::zeroed() }, Some(PROC_SUPER_MAGIC)),
        Target::Shm(_) => write_statfs(mem, buf, &unsafe { std::mem::zeroed() }, Some(TMPFS_MAGIC)),
    }
}

pub(super) fn fstatfs<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, buf: u64) -> Result<i64, i64> {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::fstatfs(p.fds.host(fd)?, &mut st) } as i64)?;
    write_statfs(mem, buf, &st, None)
}

fn write_statfs<M: GuestMemory + ?Sized>(mem: &M, buf: u64, st: &libc::statfs, magic: Option<i64>) -> Result<i64, i64> {
    const EXT4_SUPER_MAGIC: i64 = 0xef53;
    const ST_RDONLY: i64 = 1;
    const ST_NOSUID: i64 = 2;
    let flags = st.f_flags as u64;
    let mut f_flags = 0;
    if flags & libc::MNT_RDONLY as u64 != 0 {
        f_flags |= ST_RDONLY;
    }
    if flags & libc::MNT_NOSUID as u64 != 0 {
        f_flags |= ST_NOSUID;
    }
    // fsid_t's fields are private in libc: two ints, as on Linux.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    // arm64's `struct statfs`: 64-bit fields throughout, fsid as two ints.
    let words: [i64; 15] = [
        magic.unwrap_or(EXT4_SUPER_MAGIC),
        st.f_bsize as i64,
        st.f_blocks as i64,
        st.f_bfree as i64,
        st.f_bavail as i64,
        st.f_files as i64,
        st.f_ffree as i64,
        (fsid[0] as u32 as i64) | ((fsid[1] as u32 as i64) << 32),
        255, // f_namelen
        st.f_bsize as i64, // f_frsize
        f_flags,
        0,
        0,
        0,
        0,
    ];
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    mem.write(buf, &bytes)?;
    Ok(0)
}

pub(super) fn readlinkat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, buf: u64, size: u64) -> Result<i64, i64> {
    if (size as i64) <= 0 {
        return Err(errno::EINVAL);
    }
    let target = match resolve(p, mem, dirfd, path, false)?.target {
        Target::Host(h) => crate::fs::host_readlink(&h)?,
        Target::Proc(rest) => proc::readlink(p, &rest)?,
        Target::Shm(_) => return Err(errno::EINVAL), // never a link
    };
    let n = target.len().min(size as usize);
    mem.write(buf, &target[..n])?;
    Ok(n as i64)
}

pub(super) fn faccessat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<i64, i64> {
    if mode & !7 != 0 || flags & !(at::EACCESS | at::SYMLINK_NOFOLLOW | at::EMPTY_PATH) != 0 {
        return Err(errno::EINVAL);
    }
    let r = resolve(p, mem, dirfd, path, flags & at::SYMLINK_NOFOLLOW == 0)?;
    match r.target {
        Target::Host(h) => {
            let mut hf = 0;
            if flags & at::EACCESS != 0 {
                hf |= libc::AT_EACCESS;
            }
            if flags & at::SYMLINK_NOFOLLOW != 0 {
                hf |= libc::AT_SYMLINK_NOFOLLOW;
            }
            ok(unsafe { libc::faccessat(libc::AT_FDCWD, h.as_ptr(), mode as i32, hf) } as i64).map(|_| 0)
        }
        Target::Proc(rest) => proc::exists(p, &rest).map(|_| 0),
        Target::Shm(name) => {
            let want = if mode & 2 != 0 { libc::O_RDWR } else { libc::O_RDONLY };
            shm::open_named(&name, want, 0).map(|_| 0)
        }
    }
}

pub(super) fn mkdirat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, mode: u64) -> Result<i64, i64> {
    let h = host_only(resolve(p, mem, dirfd, path, false)?)?;
    ok(unsafe { libc::mkdir(h.as_ptr(), (mode as u32 & !p.umask()) as libc::mode_t) } as i64).map(|_| 0)
}

pub(super) fn unlinkat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, flags: u64) -> Result<i64, i64> {
    if flags & !at::REMOVEDIR != 0 {
        return Err(errno::EINVAL);
    }
    let r = resolve(p, mem, dirfd, path, false)?;
    if let Target::Shm(name) = &r.target {
        return if flags & at::REMOVEDIR != 0 { Err(errno::ENOTDIR) } else { shm::unlink_named(name).map(|()| 0) };
    }
    let h = host_only(r)?;
    let r = if flags & at::REMOVEDIR != 0 { unsafe { libc::rmdir(h.as_ptr()) } } else { unsafe { libc::unlink(h.as_ptr()) } };
    ok(r as i64).map(|_| 0)
}

/// The link's contents are stored as given: guest semantics apply when it's
/// followed.
pub(super) fn symlinkat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, target: u64, dirfd: u64, path: u64) -> Result<i64, i64> {
    let target = CString::new(path_arg(mem, target)?).map_err(|_| errno::EINVAL)?;
    let h = host_only(resolve(p, mem, dirfd, path, false)?)?;
    ok(unsafe { libc::symlink(target.as_ptr(), h.as_ptr()) } as i64).map(|_| 0)
}

pub(super) fn linkat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, olddir: u64, old: u64, newdir: u64, new: u64, flags: u64) -> Result<i64, i64> {
    if flags & !at::SYMLINK_FOLLOW != 0 {
        return Err(errno::EINVAL);
    }
    let follow = flags & at::SYMLINK_FOLLOW != 0;
    let old = host_only(resolve(p, mem, olddir, old, follow)?)?;
    let new = host_only(resolve(p, mem, newdir, new, false)?)?;
    // With a host root the path is unresolved, so the host must follow.
    let hf = if follow { libc::AT_SYMLINK_FOLLOW } else { 0 };
    ok(unsafe { libc::linkat(libc::AT_FDCWD, old.as_ptr(), libc::AT_FDCWD, new.as_ptr(), hf) } as i64).map(|_| 0)
}

pub(super) fn renameat2<M: GuestMemory + ?Sized>(p: &Process, mem: &M, olddir: u64, old: u64, newdir: u64, new: u64, flags: u64) -> Result<i64, i64> {
    let host_flags = match flags {
        0 => 0,
        fcntl::RENAME_NOREPLACE => libc::RENAME_EXCL,
        fcntl::RENAME_EXCHANGE => libc::RENAME_SWAP,
        _ => return Err(errno::EINVAL),
    };
    let old = host_only(resolve(p, mem, olddir, old, false)?)?;
    let new = host_only(resolve(p, mem, newdir, new, false)?)?;
    ok(unsafe { libc::renamex_np(old.as_ptr(), new.as_ptr(), host_flags) } as i64).map(|_| 0)
}

pub(super) fn fchmodat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, mode: u64) -> Result<i64, i64> {
    let h = host_only(resolve(p, mem, dirfd, path, true)?)?;
    ok(unsafe { libc::chmod(h.as_ptr(), mode as libc::mode_t) } as i64).map(|_| 0)
}

pub(super) fn fchmod(p: &Process, fd: u64, mode: u64) -> Result<i64, i64> {
    ok(unsafe { libc::fchmod(p.fds.host(fd)?, mode as libc::mode_t) } as i64).map(|_| 0)
}

pub(super) fn fchownat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, uid: u64, gid: u64, flags: u64) -> Result<i64, i64> {
    let follow = flags & at::SYMLINK_NOFOLLOW == 0;
    let h = host_only(resolve(p, mem, dirfd, path, follow)?)?;
    let f = if follow { libc::chown } else { libc::lchown };
    ok(unsafe { f(h.as_ptr(), uid as u32, gid as u32) } as i64).map(|_| 0)
}

pub(super) fn fchown(p: &Process, fd: u64, uid: u64, gid: u64) -> Result<i64, i64> {
    ok(unsafe { libc::fchown(p.fds.host(fd)?, uid as u32, gid as u32) } as i64).map(|_| 0)
}

/// `utimensat`; a NULL path means the file `dirfd` itself (as `futimens`).
pub(super) fn utimensat<M: GuestMemory + ?Sized>(p: &Process, mem: &M, dirfd: u64, path: u64, times: u64, flags: u64) -> Result<i64, i64> {
    let host_ts = if times == 0 {
        None
    } else {
        let mut ts = [mem.read_pod::<Timespec>(times)?, mem.read_pod::<Timespec>(times + 16)?];
        for t in &mut ts {
            t.tv_nsec = match t.tv_nsec {
                fcntl::UTIME_NOW => libc::UTIME_NOW,
                fcntl::UTIME_OMIT => libc::UTIME_OMIT,
                n => n,
            };
        }
        Some(ts.map(|t| libc::timespec { tv_sec: t.tv_sec, tv_nsec: t.tv_nsec }))
    };
    let ts_ptr = host_ts.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
    if path == 0 {
        return ok(unsafe { libc::futimens(p.fds.host(dirfd)?, ts_ptr) } as i64).map(|_| 0);
    }
    let follow = flags & at::SYMLINK_NOFOLLOW == 0;
    let h = host_only(resolve(p, mem, dirfd, path, follow)?)?;
    let hf = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    ok(unsafe { libc::utimensat(libc::AT_FDCWD, h.as_ptr(), ts_ptr, hf) } as i64).map(|_| 0)
}

pub(super) fn truncate<M: GuestMemory + ?Sized>(p: &Process, mem: &M, path: u64, len: u64) -> Result<i64, i64> {
    if (len as i64) < 0 {
        return Err(errno::EINVAL);
    }
    let h = host_only(resolve(p, mem, at::FDCWD as u64, path, true)?)?;
    ok(unsafe { libc::truncate(h.as_ptr(), len as i64) } as i64).map(|_| 0)
}

pub(super) fn getcwd<M: GuestMemory + ?Sized>(p: &Process, mem: &M, buf: u64, size: u64) -> Result<i64, i64> {
    let mut cwd = p.fs.cwd().to_vec();
    cwd.push(0);
    if (size as usize) < cwd.len() {
        return Err(errno::ERANGE);
    }
    mem.write(buf, &cwd)?;
    Ok(cwd.len() as i64)
}

fn enter_dir(p: &Process, r: Resolved) -> Result<i64, i64> {
    let h = host_only(r.clone()).map_err(|_| errno::ENOTDIR)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::stat(h.as_ptr(), &mut st) } as i64)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(errno::ENOTDIR);
    }
    ok(unsafe { libc::access(h.as_ptr(), libc::X_OK) } as i64)?;
    let guest = if p.fs.is_host_root() {
        // r.guest is only lexical; the cwd should be the real path.
        let real = unsafe { libc::realpath(h.as_ptr(), std::ptr::null_mut()) };
        if real.is_null() {
            return Err(super::last_errno());
        }
        let path = unsafe { std::ffi::CStr::from_ptr(real) }.to_bytes().to_vec();
        unsafe { libc::free(real.cast()) };
        path
    } else {
        r.guest
    };
    p.fs.set_cwd(guest);
    Ok(0)
}

pub(super) fn chdir<M: GuestMemory + ?Sized>(p: &Process, mem: &M, path: u64) -> Result<i64, i64> {
    let r = resolve(p, mem, at::FDCWD as u64, path, true)?;
    enter_dir(p, r)
}

pub(super) fn fchdir(p: &Process, fd: u64) -> Result<i64, i64> {
    let guest = p.fds.get(fd)?.path.clone().ok_or(errno::ENOTDIR)?;
    let r = p.fs.resolve(&p.fds, at::FDCWD as u64, &guest, true)?;
    enter_dir(p, r)
}

/// Read a directory's entries through a duplicate of its host fd.
fn snapshot(host: i32) -> Result<DirSnapshot, i64> {
    let dup = ok(unsafe { libc::dup(host) } as i64)? as i32;
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = super::last_errno();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    unsafe { libc::rewinddir(dir) };
    let mut entries = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        let ent = unsafe { &*ent };
        let name = unsafe { std::ffi::CStr::from_ptr(ent.d_name.as_ptr()) }.to_bytes().to_vec();
        entries.push(DirEntry { ino: ent.d_ino, kind: ent.d_type, name });
    }
    unsafe { libc::closedir(dir) };
    Ok(DirSnapshot { entries, next: 0 })
}

pub(super) fn getdents64<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, buf: u64, count: u64) -> Result<i64, i64> {
    let entry = p.fds.get(fd)?;
    let mut dir = entry.dir.lock().unwrap_or_else(|e| e.into_inner());
    if dir.is_none() {
        *dir = Some(snapshot(entry.host.as_raw_fd())?);
    }
    let snap = dir.as_mut().unwrap();
    let mut out = Vec::new();
    while let Some(e) = snap.entries.get(snap.next) {
        let reclen = (DIRENT64_NAME_OFFSET + e.name.len() + 1).next_multiple_of(8);
        if out.len() + reclen > count as usize {
            if out.is_empty() {
                return Err(errno::EINVAL); // buffer too small for one entry
            }
            break;
        }
        let head = Dirent64 { d_ino: e.ino, d_off: snap.next as i64 + 1, d_reclen: reclen as u16, d_type: e.kind };
        let start = out.len();
        out.extend_from_slice(&unsafe { abi::as_bytes(&head) }[..DIRENT64_NAME_OFFSET]);
        out.extend_from_slice(&e.name);
        out.resize(start + reclen, 0);
        snap.next += 1;
    }
    mem.write(buf, &out)?;
    Ok(out.len() as i64)
}
