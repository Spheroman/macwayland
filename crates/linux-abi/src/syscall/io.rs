//! Calls on file descriptors: reads and writes, seeking, duplication, `fcntl`,
//! `ioctl`, pipes, `ppoll`, `memfd_create`.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::atomic::Ordering,
};

use super::{gather, iov_len, iovecs, kfd, ok, path_arg, scatter, tty, Process, Thread, TtyOp, MAX_IO};
use crate::{
    abi::{self, fcntl, ioctl, o, PollFd, Timespec, POLLRDHUP},
    errno,
    fs::{FdEntry, FileKind},
    memory::{GuestMemory, GuestMemoryExt},
    shm::{self, ShmFile},
};

/// Highest guest fd number we hand out (Linux's default RLIMIT_NOFILE).
const MAX_FD: u64 = 1024;

/// Read from `fd` into `data`, at `off` or (None) the file position.
fn read_host(p: &Process, t: &Thread, fd: u64, data: &mut [u8], off: Option<u64>) -> Result<i64, i64> {
    let entry = p.fds.get(fd)?;
    crate::fs::hold(&entry);
    match &entry.kind {
        FileKind::EventFd(state) => return kfd::eventfd_read(p, t, &entry, state, data),
        FileKind::TimerFd(timer) => return kfd::timerfd_read(p, t, &entry, timer, data),
        FileKind::Inotify(_) => return super::inotify_read(p, t, &entry, data),
        FileKind::SignalFd(signalfd) => return super::signalfd_read(p, t, &entry, signalfd, data),
        FileKind::Epoll(_) | FileKind::PidFd(_) => return Err(errno::EINVAL),
        FileKind::Plain => {}
    }
    let host = entry.host.as_raw_fd();
    if let Some(f) = ShmFile::of(host) {
        let at = off.unwrap_or_else(|| entry.pos.load(Ordering::Acquire));
        let n = f.read_at(at, data)? as u64;
        if off.is_none() {
            entry.pos.store(at + n, Ordering::Release);
        }
        return Ok(n as i64);
    }
    if off.is_none() {
        // A terminal read from the background: SIGTTIN, as Linux's line
        // discipline checks before it waits.
        let wait = match entry.tty() {
            Some(tty) if !tty.master => p.kernel.tty_access(p, t, &entry, TtyOp::Read)?,
            _ => true,
        };
        if wait {
            super::block::ready(p, t, &entry, libc::POLLIN)?;
        }
    }
    let ptr = data.as_mut_ptr().cast();
    ok(match off {
        Some(off) => unsafe { libc::pread(host, ptr, data.len(), off as i64) },
        None => unsafe { libc::read(host, ptr, data.len()) },
    } as i64)
}

/// Write `data` to `fd`, at `off` or (None) the file position.
fn write_host(p: &Process, t: &Thread, fd: u64, data: &[u8], off: Option<u64>) -> Result<i64, i64> {
    let entry = p.fds.get(fd)?;
    match &entry.kind {
        FileKind::EventFd(state) => return kfd::eventfd_write(&entry, state, data),
        FileKind::TimerFd(_) | FileKind::Epoll(_) | FileKind::Inotify(_) | FileKind::SignalFd(_) | FileKind::PidFd(_) => return Err(errno::EINVAL),
        FileKind::Plain => {}
    }
    let host = entry.host.as_raw_fd();
    if let Some(f) = ShmFile::of(host) {
        let append = unsafe { libc::fcntl(host, libc::F_GETFL) } & libc::O_APPEND != 0;
        let at = match off {
            Some(off) => off,
            None if append => f.size(),
            None => entry.pos.load(Ordering::Acquire),
        };
        let n = f.write_at(at, data)? as u64;
        if off.is_none() {
            entry.pos.store(at + n, Ordering::Release);
        }
        return Ok(n as i64);
    }
    let tty = if off.is_none() { entry.tty() } else { None };
    if let Some(tty) = tty
        && !tty.master
    {
        p.kernel.tty_access(p, t, &entry, TtyOp::Write)?;
    }
    if off.is_none() {
        super::block::ready(p, t, &entry, libc::POLLOUT)?;
    }
    // Typed input, through the line discipline's signal characters.
    if let Some(tty) = tty
        && tty.master
        && let Some(r) = p.kernel.write_master(p, &entry, data)
    {
        return r;
    }
    let ptr = data.as_ptr().cast();
    match off {
        Some(off) => ok(unsafe { libc::pwrite(host, ptr, data.len(), off as i64) } as i64),
        // A seqpacket socket's sends wait for room, as on Linux.
        None => super::net::send_waiting(host, false, || unsafe { libc::write(host, ptr, data.len()) }),
    }
}

pub(super) fn read<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, buf: u64, len: u64) -> Result<i64, i64> {
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    let n = read_host(p, t, fd, &mut data, None)?;
    mem.write(buf, &data[..n as usize])?;
    Ok(n)
}

pub(super) fn write<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, buf: u64, len: u64) -> Result<i64, i64> {
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    mem.read(buf, &mut data)?;
    write_host(p, t, fd, &data, None)
}

pub(super) fn readv<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, iov: u64, count: u64) -> Result<i64, i64> {
    let iov = iovecs(mem, iov, count)?;
    let mut data = vec![0u8; iov_len(&iov) as usize];
    let n = read_host(p, t, fd, &mut data, None)?;
    scatter(mem, &iov, &data[..n as usize])?;
    Ok(n)
}

/// Gathers into one buffer for a single host `write`, which keeps writev's
/// all-at-once behaviour for pipes and sockets.
pub(super) fn writev<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, iov: u64, count: u64) -> Result<i64, i64> {
    let data = gather(mem, &iovecs(mem, iov, count)?)?;
    write_host(p, t, fd, &data, None)
}

pub(super) fn pread<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, buf: u64, len: u64, off: u64) -> Result<i64, i64> {
    if (off as i64) < 0 {
        return Err(errno::EINVAL);
    }
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    let n = read_host(p, t, fd, &mut data, Some(off))?;
    mem.write(buf, &data[..n as usize])?;
    Ok(n)
}

pub(super) fn pwrite<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, buf: u64, len: u64, off: u64) -> Result<i64, i64> {
    if (off as i64) < 0 {
        return Err(errno::EINVAL);
    }
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    mem.read(buf, &mut data)?;
    write_host(p, t, fd, &data, Some(off))
}

pub(super) fn lseek(p: &Process, fd: u64, off: i64, whence: u64) -> Result<i64, i64> {
    let whence = match whence {
        0 => libc::SEEK_SET,
        1 => libc::SEEK_CUR,
        2 => libc::SEEK_END,
        fcntl::SEEK_DATA => libc::SEEK_DATA,
        fcntl::SEEK_HOLE => libc::SEEK_HOLE,
        _ => return Err(errno::EINVAL),
    };
    let entry = p.fds.get(fd)?;
    {
        let mut dir = entry.dir.lock().unwrap_or_else(|e| e.into_inner());
        if dir.is_some() {
            // Directories: only rewinding is supported (all getdents64 needs).
            if off != 0 || whence != libc::SEEK_SET {
                return Err(errno::EINVAL);
            }
            match dir.as_mut() {
                // A listing made, not read (/proc's): from the top again.
                Some(d) if d.fixed => d.next = 0,
                _ => *dir = None,
            }
            return Ok(0);
        }
    }
    if let Some(f) = ShmFile::of(entry.host.as_raw_fd()) {
        let (size, cur) = (f.size() as i64, entry.pos.load(Ordering::Acquire) as i64);
        let new = match whence {
            libc::SEEK_SET => off,
            libc::SEEK_CUR => cur.checked_add(off).ok_or(errno::EOVERFLOW)?,
            libc::SEEK_END => size.checked_add(off).ok_or(errno::EOVERFLOW)?,
            // No holes: data runs to the end of the file.
            libc::SEEK_DATA if off < size => off,
            libc::SEEK_HOLE if off < size => size,
            _ => return Err(errno::ENXIO),
        };
        if new < 0 {
            return Err(errno::EINVAL);
        }
        entry.pos.store(new as u64, Ordering::Release);
        return Ok(new);
    }
    ok(unsafe { libc::lseek(entry.host.as_raw_fd(), off, whence) })
}

/// `close_range(first, last, flags)`.
pub(super) fn close_range(p: &Process, first: u64, last: u64, flags: u64) -> Result<i64, i64> {
    // UNSHARE does nothing here: no fd table is shared between processes.
    const UNSHARE: u64 = 2;
    const CLOEXEC: u64 = 4;
    if flags & !(UNSHARE | CLOEXEC) != 0 || first > last {
        return Err(errno::EINVAL);
    }
    p.fds.close_range(first, last, flags & CLOEXEC != 0);
    Ok(0)
}

pub(super) fn dup(p: &Process, fd: u64) -> Result<i64, i64> {
    p.fds.dup_from(fd, 0, false)
}

pub(super) fn dup3(p: &Process, old: u64, new: u64, flags: u64) -> Result<i64, i64> {
    if old == new || flags & !o::CLOEXEC != 0 {
        return Err(errno::EINVAL);
    }
    if new >= MAX_FD {
        return Err(errno::EBADF);
    }
    p.fds.dup_to(old, new as usize, flags & o::CLOEXEC != 0)?;
    Ok(new as i64)
}

pub(super) fn fcntl<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, cmd: u64, arg: u64) -> Result<i64, i64> {
    match cmd {
        // Open file description locks are taken as the host's POSIX
        // (per-process) locks: the same for a process that doesn't lock one
        // file through two descriptions.
        fcntl::GETLK | fcntl::SETLK | fcntl::SETLKW | fcntl::OFD_GETLK | fcntl::OFD_SETLK | fcntl::OFD_SETLKW => record_lock(p, mem, fd, cmd, arg),
        fcntl::DUPFD | fcntl::DUPFD_CLOEXEC => {
            if arg >= MAX_FD {
                return Err(errno::EINVAL);
            }
            p.fds.dup_from(fd, arg as usize, cmd == fcntl::DUPFD_CLOEXEC)
        }
        fcntl::GETFD => Ok(if p.fds.cloexec(fd)? { fcntl::FD_CLOEXEC as i64 } else { 0 }),
        fcntl::SETFD => p.fds.set_cloexec(fd, arg & fcntl::FD_CLOEXEC != 0).map(|()| 0),
        fcntl::GETFL => {
            let fl = ok(unsafe { libc::fcntl(p.fds.host(fd)?, libc::F_GETFL) } as i64)?;
            Ok(abi::open_flags_from_darwin(fl as i32) as i64)
        }
        fcntl::SETFL => {
            let host = p.fds.host(fd)?;
            let cur = ok(unsafe { libc::fcntl(host, libc::F_GETFL) } as i64)? as i32;
            let mut new = cur & !(libc::O_APPEND | libc::O_NONBLOCK);
            if arg & o::APPEND != 0 {
                new |= libc::O_APPEND;
            }
            if arg & o::NONBLOCK != 0 {
                new |= libc::O_NONBLOCK;
            }
            ok(unsafe { libc::fcntl(host, libc::F_SETFL, new) } as i64).map(|_| 0)
        }
        // memfd seals, kept with the object (see crate::shm).
        fcntl::ADD_SEALS | fcntl::GET_SEALS => {
            let file = ShmFile::of(p.fds.host(fd)?).ok_or(errno::EINVAL)?;
            if cmd == fcntl::GET_SEALS {
                return Ok(file.seals() as i64);
            }
            file.add_seals(arg).map(|()| 0)
        }
        _ => {
            p.unimplemented(format!("fcntl command {cmd}"));
            Err(errno::EINVAL)
        }
    }
}

/// `F_GETLK`/`F_SETLK`/`F_SETLKW` on the host's record locks. Linux's
/// `struct flock` (arm64): `short l_type, l_whence; off_t l_start, l_len;
/// pid_t l_pid`, with read, write, unlock as 0, 1, 2.
fn record_lock<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, cmd: u64, arg: u64) -> Result<i64, i64> {
    let host = p.fds.host(fd)?;
    let (kind, whence): (i16, i16) = (mem.read_pod(arg)?, mem.read_pod(arg + 2)?);
    let (start, len): (i64, i64) = (mem.read_pod(arg + 8)?, mem.read_pod(arg + 16)?);
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = match kind {
        0 => libc::F_RDLCK,
        1 => libc::F_WRLCK,
        2 => libc::F_UNLCK,
        _ => return Err(errno::EINVAL),
    };
    fl.l_whence = whence;
    fl.l_start = start;
    fl.l_len = len;
    let host_cmd = match cmd {
        fcntl::GETLK | fcntl::OFD_GETLK => libc::F_GETLK,
        fcntl::SETLK | fcntl::OFD_SETLK => libc::F_SETLK,
        _ => libc::F_SETLKW,
    };
    ok(unsafe { libc::fcntl(host, host_cmd, &mut fl) } as i64)?;
    if host_cmd == libc::F_GETLK {
        let kind: i16 = match fl.l_type {
            libc::F_RDLCK => 0,
            libc::F_WRLCK => 1,
            _ => 2,
        };
        mem.write_pod(arg, &kind)?;
        mem.write_pod(arg + 2, &fl.l_whence)?;
        mem.write_pod(arg + 8, &fl.l_start)?;
        mem.write_pod(arg + 16, &fl.l_len)?;
        // An OFD lock's owner is no process: Linux says -1.
        let pid = if cmd == fcntl::OFD_GETLK && fl.l_type != libc::F_UNLCK { -1 } else { fl.l_pid };
        mem.write_pod(arg + 24, &pid)?;
    }
    Ok(0)
}

pub(super) fn set_nonblocking(host: i32, on: bool) -> Result<(), i64> {
    let cur = ok(unsafe { libc::fcntl(host, libc::F_GETFL) } as i64)? as i32;
    let new = if on { cur | libc::O_NONBLOCK } else { cur & !libc::O_NONBLOCK };
    ok(unsafe { libc::fcntl(host, libc::F_SETFL, new) } as i64).map(|_| ())
}

pub(super) fn ioctl<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, fd: u64, req: u64, arg: u64) -> Result<i64, i64> {
    let file = p.fds.get(fd)?;
    let host = file.host.as_raw_fd();
    // Changing a terminal from the background: SIGTTOU, as Linux checks
    // (`tcsetpgrp` checks for itself).
    if matches!(req, tty::TCSETS | tty::TCSETSW | tty::TCSETSF | tty::TCSETS2 | tty::TCSETSW2 | tty::TCSETSF2 | tty::TCSBRK | tty::TCXONC | tty::TCFLSH)
        && file.tty().is_some()
    {
        p.kernel.tty_access(p, t, &file, TtyOp::Change)?;
    }
    match req {
        ioctl::TIOCGWINSZ => {
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(host, libc::TIOCGWINSZ, &mut ws) } < 0 {
                // A pty master whose slave isn't open: the size it was given.
                ws = tty::pending_winsize(p, host).ok_or_else(super::last_errno)?;
            }
            // Same layout: four u16s.
            let bytes = [ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel];
            let bytes: Vec<u8> = bytes.iter().flat_map(|v| v.to_le_bytes()).collect();
            mem.write(arg, &bytes)?;
            Ok(0)
        }
        ioctl::FIONREAD => {
            let mut n: libc::c_int = 0;
            match super::inotify_pending(&*p.fds.get(fd)?) {
                Some(pending) => n = pending as libc::c_int,
                None => ok(unsafe { libc::ioctl(host, libc::FIONREAD, &mut n) } as i64).map(|_| ())?,
            }
            mem.write(arg, &n.to_le_bytes())?;
            Ok(0)
        }
        ioctl::FIONBIO => {
            let on: i32 = mem.read_pod(arg)?;
            set_nonblocking(host, on != 0).map(|_| 0)
        }
        ioctl::FIOCLEX | ioctl::FIONCLEX => p.fds.set_cloexec(fd, req == ioctl::FIOCLEX).map(|()| 0),
        tty::TCGETS | tty::TCSETS | tty::TCSETSW | tty::TCSETSF | tty::TCGETS2 | tty::TCSETS2 | tty::TCSETSW2 | tty::TCSETSF2 | tty::TCSBRK | tty::TCXONC | tty::TCFLSH => {
            tty::ioctl(mem, host, req, arg)
        }
        tty::TIOCGPTN | tty::TIOCSPTLCK | tty::TIOCGPTPEER => {
            tty::pty_ioctl(mem, host, req, arg, |flags| {
                let mut name = [0 as libc::c_char; 128];
                ok(unsafe { libc::ioctl(host, libc::TIOCPTYGNAME as u64, name.as_mut_ptr()) } as i64)?;
                let fd = ok(unsafe { libc::open(name.as_ptr(), flags | libc::O_NOCTTY) } as i64)? as i32;
                let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes().to_vec();
                let new = p.fds.insert(FdEntry::new(unsafe { OwnedFd::from_raw_fd(fd) }).path(path).cloexec(arg & o::CLOEXEC != 0));
                if let Ok(slave) = p.fds.get(new as u64) {
                    p.kernel.tty_opened(p, &slave, arg & o::NOCTTY != 0);
                }
                Ok(new)
            })
        }
        tty::TIOCSCTTY | tty::TIOCNOTTY | tty::TIOCGSID if file.tty().is_none() && unsafe { libc::isatty(host) } == 0 => Err(errno::ENOTTY),
        tty::TIOCSCTTY => p.kernel.set_ctty(p, &file, arg == 1),
        tty::TIOCNOTTY => p.kernel.drop_ctty(p, &file),
        tty::TIOCGSID => {
            let sid = p.kernel.tcgetsid(p, &file)? as i32;
            mem.write_pod(arg, &sid)?;
            Ok(0)
        }
        ioctl::TIOCSWINSZ => {
            let v: [u16; 4] = mem.read_pod(arg)?;
            let ws = libc::winsize { ws_row: v[0], ws_col: v[1], ws_xpixel: v[2], ws_ypixel: v[3] };
            let old = tty::winsize(host);
            if unsafe { libc::ioctl(host, libc::TIOCSWINSZ, &ws) } < 0 {
                // macOS only sizes a pty once its slave is open; Linux any
                // time. Keep it for when the slave is opened.
                let e = super::last_errno();
                return if tty::park_winsize(p, host, &ws) { Ok(0) } else { Err(e) };
            }
            let same = old.is_some_and(|o| (o.ws_row, o.ws_col, o.ws_xpixel, o.ws_ypixel) == (ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel));
            if !same {
                p.kernel.winsize_changed(p, &file);
            }
            Ok(0)
        }
        ioctl::TIOCGPGRP => {
            let pgrp = p.kernel.tcgetpgrp(p, &file)? as i32;
            mem.write_pod(arg, &pgrp)?;
            Ok(0)
        }
        ioctl::TIOCSPGRP => {
            let pgrp: i32 = mem.read_pod(arg)?;
            p.kernel.tcsetpgrp(p, t, &file, pgrp as i64)
        }
        // No reflinks to be had into an existing file on macOS (clonefile
        // makes a new one): what Linux says on a file system without them,
        // and `cp` copies instead.
        ioctl::FICLONE | ioctl::FICLONERANGE => Err(errno::EOPNOTSUPP),
        // Not ext2-like and not FAT: what Linux answers on, say, tmpfs.
        ioctl::FS_IOC_GETFLAGS | ioctl::VFAT_IOCTL_READDIR_BOTH => Err(errno::ENOTTY),
        _ => {
            p.unimplemented(format!("ioctl {req:#x}"));
            Err(errno::ENOTTY)
        }
    }
}

/// `sendfile`: copy up to `count` bytes from `in_fd` (at `*offset`, if
/// given, which is then advanced) to `out_fd`, through a buffer.
pub(super) fn sendfile<M: GuestMemory + ?Sized>(p: &Process, mem: &M, out_fd: u64, in_fd: u64, offset: u64, count: u64) -> Result<i64, i64> {
    let (out, input) = (p.fds.host(out_fd)?, p.fds.host(in_fd)?);
    let mut pos: Option<i64> = if offset != 0 { Some(mem.read_pod(offset)?) } else { None };
    let mut buf = vec![0u8; count.min(1 << 20) as usize];
    let mut done = 0u64;
    while done < count {
        let want = buf.len().min((count - done) as usize);
        let n = match pos {
            Some(at) => unsafe { libc::pread(input, buf.as_mut_ptr().cast(), want, at) },
            None => unsafe { libc::read(input, buf.as_mut_ptr().cast(), want) },
        };
        if n <= 0 {
            if n < 0 && done == 0 {
                return ok(-1);
            }
            break;
        }
        let w = unsafe { libc::write(out, buf.as_ptr().cast(), n as usize) };
        if w < 0 {
            if done == 0 {
                return ok(-1);
            }
            break;
        }
        done += w as u64;
        if let Some(at) = pos.as_mut() {
            *at += w as i64;
        }
        if (w as isize) < n {
            // A short write (a full pipe or socket): put back what wasn't sent.
            if pos.is_none() {
                unsafe { libc::lseek(input, (w as i64) - (n as i64), libc::SEEK_CUR) };
            }
            break;
        }
    }
    if let Some(at) = pos {
        mem.write_pod(offset, &at)?;
    }
    Ok(done as i64)
}

pub(super) fn pipe2<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fds: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(o::CLOEXEC | o::NONBLOCK | o::DIRECT) != 0 {
        return Err(errno::EINVAL);
    }
    let mut host = [0; 2];
    ok(unsafe { libc::pipe(host.as_mut_ptr()) } as i64)?;
    let mut nums = [0i32; 2];
    for (i, &h) in host.iter().enumerate() {
        let owned = unsafe { OwnedFd::from_raw_fd(h) };
        if flags & o::NONBLOCK != 0 {
            set_nonblocking(h, true)?;
        }
        nums[i] = p.fds.insert(FdEntry::new(owned).cloexec(flags & o::CLOEXEC != 0)) as i32;
    }
    let bytes: Vec<u8> = nums.iter().flat_map(|n| n.to_le_bytes()).collect();
    if let Err(e) = mem.write(fds, &bytes) {
        for n in nums {
            p.fds.remove(n as u64).ok();
        }
        return Err(e.into());
    }
    Ok(0)
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)` (what
/// `select` is on arm64), on the host's `poll`. `sigmask` points at a
/// (mask pointer, size) pair.
#[allow(clippy::too_many_arguments)]
pub(super) fn pselect6<M: GuestMemory + ?Sized>(p: &Process, t: &mut super::Thread, mem: &M, nfds: u64, sets: [u64; 3], timeout: u64, sigmask: u64) -> Result<i64, i64> {
    const FD_SETSIZE: u64 = 1024;
    if nfds > FD_SETSIZE {
        return Err(errno::EINVAL);
    }
    let words = nfds.div_ceil(64) as usize;
    // The guest's three sets, as bit words (absent sets: empty).
    let mut bits = [vec![0u64; words], vec![0u64; words], vec![0u64; words]];
    for (set, &addr) in bits.iter_mut().zip(&sets) {
        if addr != 0 {
            for (w, word) in set.iter_mut().enumerate() {
                *word = mem.read_pod(addr + w as u64 * 8)?;
            }
        }
    }
    let wanted = |set: &[u64], fd: u64| set.get(fd as usize / 64).is_some_and(|w| w & (1 << (fd % 64)) != 0);
    let mut fds = Vec::new();
    let mut host = Vec::new();
    for fd in 0..nfds {
        let mut events = 0;
        if wanted(&bits[0], fd) {
            events |= libc::POLLIN;
        }
        if wanted(&bits[1], fd) {
            events |= libc::POLLOUT;
        }
        if wanted(&bits[2], fd) {
            events |= libc::POLLPRI;
        }
        if events != 0 {
            let host_fd = p.fds.host(fd)?;
            // Asked for no more than exceptions, a pipe stays in the set (a
            // closed fd must still be EBADF) with nothing to wait for.
            let events = if events & libc::POLLPRI != 0 { host_poll_events(host_fd, events) } else { events };
            host.push(libc::pollfd { fd: host_fd, events, revents: 0 });
            fds.push(fd);
        }
    }
    let deadline = t.deadline(poll_deadline(mem, timeout)?);
    if sigmask != 0 {
        let mask_ptr: u64 = mem.read_pod(sigmask)?;
        if mask_ptr != 0 {
            super::signal::wait_with_mask(t, mem.read_pod::<u64>(mask_ptr)?);
        }
    }
    let _ready_or_timed_out = super::block::poll(p, t, &mut host, deadline)?;
    // Back into the sets: what's ready, and only that.
    let mut out = [vec![0u64; words], vec![0u64; words], vec![0u64; words]];
    let mut count = 0;
    for (pf, &fd) in host.iter().zip(&fds) {
        let r = pf.revents;
        let ready = [
            r & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && pf.events & libc::POLLIN != 0,
            r & (libc::POLLOUT | libc::POLLERR) != 0 && pf.events & libc::POLLOUT != 0,
            r & libc::POLLPRI != 0,
        ];
        for (set, on) in out.iter_mut().zip(ready) {
            if on {
                set[fd as usize / 64] |= 1 << (fd % 64);
                count += 1;
            }
        }
    }
    for (set, &addr) in out.iter().zip(&sets) {
        if addr != 0 {
            for (w, word) in set.iter().enumerate() {
                mem.write_pod(addr + w as u64 * 8, word)?;
            }
        }
    }
    Ok(count)
}

/// Linux reports POLLPRI for sockets' out-of-band data (and a few special
/// files), never for pipes, but Darwin's poll reports it whenever a pipe is
/// readable; Firefox, asking for it, takes its wake-up pipe for broken and
/// makes a new one, for ever. So only sockets are asked.
fn host_poll_events(host: i32, events: i16) -> i16 {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let socket = unsafe { libc::fstat(host, &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK;
    if socket {
        events
    } else {
        events & !libc::POLLPRI
    }
}

/// When a `ppoll` or `pselect6` with this timeout (a guest `timespec`, 0
/// for none) gives up.
fn poll_deadline<M: GuestMemory + ?Sized>(mem: &M, timeout: u64) -> Result<Option<std::time::Instant>, i64> {
    if timeout == 0 {
        return Ok(None);
    }
    let ts: Timespec = mem.read_pod(timeout)?;
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(errno::EINVAL);
    }
    let d = std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
    Ok(Some(std::time::Instant::now().checked_add(d).unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(1 << 32))))
}

/// `ppoll(fds, nfds, timeout, sigmask, size)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn ppoll<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, fds: u64, nfds: u64, timeout: u64, sigmask: u64) -> Result<i64, i64> {
    if nfds > MAX_FD * 64 {
        return Err(errno::EINVAL);
    }
    let mut polls: Vec<PollFd> = (0..nfds).map(|i| mem.read_pod(fds + i * 8)).collect::<Result<_, _>>()?;
    // Guest fds with no host fd report POLLNVAL; negative ones are skipped.
    let mut invalid = vec![false; polls.len()];
    let mut host: Vec<libc::pollfd> = polls
        .iter()
        .enumerate()
        .map(|(i, pf)| {
            let fd = if pf.fd < 0 {
                -1
            } else {
                p.fds.host(pf.fd as u64).unwrap_or_else(|_| {
                    invalid[i] = true;
                    -1
                })
            };
            let events = if pf.events & libc::POLLPRI != 0 && fd >= 0 { host_poll_events(fd, pf.events) } else { pf.events };
            libc::pollfd { fd, events: events & !POLLRDHUP, revents: 0 }
        })
        .collect();
    let deadline = t.deadline(poll_deadline(mem, timeout)?);
    if sigmask != 0 {
        super::signal::wait_with_mask(t, mem.read_pod::<u64>(sigmask)?);
    }
    let mut n = super::block::poll(p, t, &mut host, deadline)?.unwrap_or(0);
    for (i, pf) in polls.iter_mut().enumerate() {
        pf.revents = if invalid[i] {
            n += 1;
            libc::POLLNVAL
        } else {
            host[i].revents
        };
        mem.write_pod(fds + i as u64 * 8, pf)?;
    }
    Ok(n)
}

pub(super) fn ftruncate(p: &Process, fd: u64, len: u64) -> Result<i64, i64> {
    if (len as i64) < 0 {
        return Err(errno::EINVAL);
    }
    let host = p.fds.host(fd)?;
    if let Some(f) = ShmFile::of(host) {
        return f.set_size(len).map(|()| 0);
    }
    ok(unsafe { libc::ftruncate(host, len as i64) } as i64).map(|_| 0)
}

/// `fallocate`: making room (mode 0) grows a file to cover the range, which is
/// how `posix_fallocate` grows Wayland pools; space isn't reserved on macOS.
pub(super) fn fallocate(p: &Process, fd: u64, mode: u64, off: u64, len: u64) -> Result<i64, i64> {
    const KEEP_SIZE: u64 = 1;
    if (off as i64) < 0 || (len as i64) <= 0 {
        return Err(errno::EINVAL);
    }
    let end = off.checked_add(len).ok_or(errno::EFBIG)?;
    let host = p.fds.host(fd)?;
    match mode {
        KEEP_SIZE => Ok(0),
        0 => {
            if let Some(f) = ShmFile::of(host) {
                return if end > f.size() { f.set_size(end).map(|()| 0) } else { Ok(0) };
            }
            let st = shm::fstat(host)?;
            if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(errno::ENODEV);
            }
            if (st.st_size as u64) < end {
                ok(unsafe { libc::ftruncate(host, end as i64) } as i64)?;
            }
            Ok(0)
        }
        _ => Err(errno::EOPNOTSUPP),
    }
}

pub(super) fn fsync(p: &Process, fd: u64) -> Result<i64, i64> {
    ok(unsafe { libc::fsync(p.fds.host(fd)?) } as i64).map(|_| 0)
}

/// `memfd_create`: a shared memory object ([`crate::shm`]): in RAM, unlike a
/// file, whose pages macOS would write to disk. Seals are accepted but not
/// enforced.
pub(super) fn memfd_create<M: GuestMemory + ?Sized>(p: &Process, mem: &M, name: u64, flags: u64) -> Result<i64, i64> {
    const ALLOWED: u64 = fcntl::MFD_CLOEXEC | 0x2 /* ALLOW_SEALING */ | 0x8 /* NOEXEC_SEAL */ | 0x10 /* EXEC */;
    let name = path_arg(mem, name)?;
    if name.len() > 249 || flags & !ALLOWED != 0 {
        return Err(errno::EINVAL);
    }
    let file = shm::create()?;
    const ALLOW_SEALING: u64 = 0x2;
    const NOEXEC_SEAL: u64 = 0x8;
    if flags & (ALLOW_SEALING | NOEXEC_SEAL) != 0 {
        let seals = if flags & NOEXEC_SEAL != 0 { shm::SEAL_EXEC } else { 0 };
        ShmFile::of(file.as_raw_fd()).ok_or(errno::EIO)?.allow_sealing(seals);
    }
    let mut path = b"/memfd:".to_vec();
    path.extend_from_slice(&name);
    path.extend_from_slice(b" (deleted)");
    let entry = FdEntry::new(file).path(path).cloexec(flags & fcntl::MFD_CLOEXEC != 0);
    Ok(p.fds.insert(entry))
}
