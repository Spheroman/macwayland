//! pidfds: a file that names a process. `pidfd_open`, `pidfd_send_signal`,
//! `pidfd_getfd`, and `waitid(P_PIDFD)`.
//!
//! The host fd behind one reads as ready once the process has ended, so
//! `poll` and `epoll` on it wait for that, as on Linux. Where the process
//! table is the runtime's (many processes in one VM) it hands out the fd and
//! makes it ready as the process becomes a zombie, and `pidfd_getfd` takes a
//! file from another process's table. In 1:1 mode the fd is a kqueue
//! watching the Mac process's exit; another process's files are out of
//! reach (ENOSYS).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{ok, signal, Process};
use crate::{
    errno,
    fs::{FdEntry, FileKind},
};

const PIDFD_NONBLOCK: u64 = 0o4000;

/// A host fd that becomes readable when Mac process `pid` exits (1:1 mode).
pub(crate) fn host_exit_watch(pid: i64) -> Result<OwnedFd, i64> {
    let kq = ok(unsafe { libc::kqueue() } as i64)? as i32;
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let ev = libc::kevent { ident: pid as usize, filter: libc::EVFILT_PROC, flags: libc::EV_ADD, fflags: libc::NOTE_EXIT, data: 0, udata: std::ptr::null_mut() };
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    ok(unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, &zero) } as i64)?;
    Ok(kq)
}

/// A pidfd entry for `pid` on host fd `host`.
pub(crate) fn entry(pid: i64, host: OwnedFd, nonblock: bool) -> FdEntry {
    if nonblock {
        unsafe { libc::fcntl(host.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    }
    let mut entry = FdEntry::new(host).cloexec(true);
    entry.kind = FileKind::PidFd(pid);
    entry
}

/// `pidfd_open(pid, flags)`.
pub(super) fn pidfd_open(p: &Process, pid: u64, flags: u64) -> Result<i64, i64> {
    let pid = pid as i32 as i64;
    if flags & !PIDFD_NONBLOCK != 0 || pid <= 0 {
        return Err(errno::EINVAL);
    }
    let host = p.kernel.pidfd(p, pid)?;
    Ok(p.fds.insert(entry(pid, host, flags & PIDFD_NONBLOCK != 0)))
}

/// The process a pidfd names.
pub(super) fn pid_of(p: &Process, fd: u64) -> Result<i64, i64> {
    match p.fds.get(fd)?.kind {
        FileKind::PidFd(pid) => Ok(pid),
        _ => Err(errno::EBADF),
    }
}

/// `pidfd_send_signal(pidfd, sig, info, flags)`: `kill` of the process it
/// names. (A `siginfo` from the caller is taken as SI_USER, as `kill`'s.)
pub(super) fn pidfd_send_signal(p: &Process, fd: u64, sig: u64, _info: u64, flags: u64) -> Result<i64, i64> {
    if flags != 0 {
        return Err(errno::EINVAL);
    }
    let pid = pid_of(p, fd)?;
    signal::kill(p, pid, sig)
}

/// `pidfd_getfd(pidfd, targetfd, flags)`: a new fd (close-on-exec) for the
/// same open file as the other process's `targetfd`.
pub(super) fn pidfd_getfd(p: &Process, fd: u64, target: u64, flags: u64) -> Result<i64, i64> {
    if flags != 0 {
        return Err(errno::EINVAL);
    }
    let pid = pid_of(p, fd)?;
    let file = if pid == p.pid { p.fds.get(target)? } else { p.kernel.process_file(p, pid, target)? };
    Ok(p.fds.insert_file(file, true))
}
