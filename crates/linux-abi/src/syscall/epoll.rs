//! `epoll` on top of kqueue.
//!
//! An epoll fd is a host kqueue, plus the guest's interest list (events and
//! the 64-bit `data` per guest fd, which kqueue can't hold for us). `EPOLLIN`
//! and `EPOLLOUT` become read and write filters on the watched fd's host
//! descriptor; `EPOLLET` is `EV_CLEAR`; level-triggered is kqueue's default
//! too. A wait turns each fd's kevents back into one `epoll_event`.

use std::{
    collections::BTreeMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Mutex,
};

use super::{ok, signal, Process, Thread};
use crate::{
    abi::Timespec,
    errno,
    fs::{FdEntry, FileKind, Interest, OpenFile},
    memory::{GuestMemory, GuestMemoryExt},
};

pub const EPOLLIN: u32 = 0x1;
pub const EPOLLPRI: u32 = 0x2;
pub const EPOLLOUT: u32 = 0x4;
pub const EPOLLERR: u32 = 0x8;
pub const EPOLLHUP: u32 = 0x10;
pub const EPOLLRDHUP: u32 = 0x2000;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
const CTL_ADD: u64 = 1;
const CTL_DEL: u64 = 2;
const CTL_MOD: u64 = 3;
const EPOLL_CLOEXEC: u64 = 0o2000000;
/// `struct epoll_event` on arm64: `u32 events`, padding, `u64 data`.
const EVENT_SIZE: u64 = 16;
/// Linux's cap on `maxevents`.
const EP_MAX_EVENTS: u64 = i32::MAX as u64 / EVENT_SIZE;

/// A new, empty epoll instance as an fd entry: a host kqueue.
pub(crate) fn new_instance() -> Result<FdEntry, i64> {
    let kq = ok(unsafe { libc::kqueue() } as i64)? as RawFd;
    let mut entry = FdEntry::new(unsafe { OwnedFd::from_raw_fd(kq) });
    entry.kind = FileKind::Epoll(Mutex::new(BTreeMap::new()));
    Ok(entry)
}

/// In a forked child: make epoll instance `epfd` watch `fd` again.
pub(crate) fn rewatch(table: &crate::fs::FdTable, epfd: u64, fd: i32, interest: Interest) {
    let (Ok(ep), Ok(target)) = (table.get(epfd), table.get(fd as u64)) else { return };
    if let Ok(set) = interests(&ep)
        && register(ep.host.as_raw_fd(), target.host.as_raw_fd(), fd, Some(interest.events)).is_ok()
    {
        set.lock().unwrap_or_else(|e| e.into_inner()).insert(fd, interest);
    }
}

pub(super) fn epoll_create1(p: &Process, flags: u64) -> Result<i64, i64> {
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(errno::EINVAL);
    }
    Ok(p.fds.insert(new_instance()?.cloexec(flags & EPOLL_CLOEXEC != 0)))
}

fn interests(file: &OpenFile) -> Result<&Mutex<BTreeMap<i32, Interest>>, i64> {
    match &file.kind {
        FileKind::Epoll(set) => Ok(set),
        _ => Err(errno::EINVAL),
    }
}

fn kevent_for(ident: RawFd, filter: i16, flags: u16, udata: i32) -> libc::kevent {
    libc::kevent { ident: ident as usize, filter, flags, fflags: 0, data: 0, udata: udata as usize as *mut libc::c_void }
}

/// Make the kqueue's filters for guest fd `fd` (host `host`) match `events`.
fn register(kq: RawFd, host: RawFd, fd: i32, events: Option<u32>) -> Result<(), i64> {
    let mut changes = Vec::new();
    for (filter, wanted) in [(libc::EVFILT_READ, EPOLLIN | EPOLLPRI | EPOLLRDHUP), (libc::EVFILT_WRITE, EPOLLOUT)] {
        match events {
            Some(ev) if ev & wanted != 0 => {
                let mut flags = libc::EV_ADD | libc::EV_ENABLE;
                if ev & EPOLLET != 0 {
                    flags |= libc::EV_CLEAR;
                }
                changes.push(kevent_for(host, filter, flags, fd));
            }
            // Not (or no longer) wanted: off, if it was on.
            _ => changes.push(kevent_for(host, filter, libc::EV_DELETE, fd)),
        }
    }
    for change in changes {
        let mut result = change;
        let n = unsafe { libc::kevent(kq, &change, 1, &mut result, 1, &libc::timespec { tv_sec: 0, tv_nsec: 0 }) };
        // Deleting a filter that wasn't there is fine.
        let failed = n < 0 || (n == 1 && result.flags & libc::EV_ERROR != 0 && result.data != 0 && result.data as i32 != libc::ENOENT);
        if failed && change.flags & libc::EV_DELETE == 0 {
            let code = if n < 0 { std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL) } else { result.data as i32 };
            return Err(errno::from_io(&std::io::Error::from_raw_os_error(code)));
        }
    }
    Ok(())
}

pub(super) fn epoll_ctl<M: GuestMemory + ?Sized>(p: &Process, mem: &M, epfd: u64, op: u64, fd: u64, event: u64) -> Result<i64, i64> {
    let ep = p.fds.get(epfd)?;
    let set = interests(&ep)?;
    let target = p.fds.get(fd)?;
    if epfd == fd || std::ptr::eq(&*ep, &*target) {
        return Err(errno::EINVAL);
    }
    // Linux can't watch regular files or directories (they're always ready).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::fstat(target.host.as_raw_fd(), &mut st) } as i64)?;
    let kind = st.st_mode & libc::S_IFMT;
    let emulated = !matches!(target.kind, FileKind::Plain);
    if kind == libc::S_IFREG || kind == libc::S_IFDIR || (kind == 0 && !emulated) {
        return Err(errno::EPERM);
    }
    let mut set = set.lock().unwrap_or_else(|e| e.into_inner());
    let (kq, host, fd) = (ep.host.as_raw_fd(), target.host.as_raw_fd(), fd as i32);
    match op {
        CTL_ADD | CTL_MOD => {
            let (events, data) = (mem.read_pod::<u32>(event)?, mem.read_pod::<u64>(event + 8)?);
            match (op, set.contains_key(&fd)) {
                (CTL_ADD, true) => return Err(errno::EEXIST),
                (CTL_MOD, false) => return Err(errno::ENOENT),
                _ => {}
            }
            // Emulated files are kqueues underneath: readable is all they say.
            let watch = if emulated { events & !EPOLLOUT } else { events };
            register(kq, host, fd, Some(watch))?;
            set.insert(fd, Interest { events, data });
        }
        CTL_DEL => {
            set.remove(&fd).ok_or(errno::ENOENT)?;
            register(kq, host, fd, None)?;
        }
        _ => return Err(errno::EINVAL),
    }
    Ok(0)
}

/// `epoll_pwait` (`timeout_ms`, -1: forever) and `epoll_pwait2`
/// (`timespec`, None: forever). `sigmask`, if non-zero, is the mask to wait
/// with, as in `sigsuspend`.
#[allow(clippy::too_many_arguments)]
pub(super) fn epoll_wait<M: GuestMemory + ?Sized>(
    p: &Process,
    t: &mut Thread,
    mem: &M,
    epfd: u64,
    events: u64,
    maxevents: u64,
    timeout: Option<Timespec>,
    sigmask: u64,
) -> Result<i64, i64> {
    if maxevents as i64 <= 0 || maxevents > EP_MAX_EVENTS {
        return Err(errno::EINVAL);
    }
    let ep = p.fds.get(epfd)?;
    let set = interests(&ep)?;
    if sigmask != 0 {
        signal::wait_with_mask(t, mem.read_pod::<u64>(sigmask)?);
    }
    // Room for a read and a write event per fd.
    let n = (maxevents as usize).min(1024) * 2;
    let mut out: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; n];
    let kq = ep.host.as_raw_fd();
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let deadline = timeout.map(|ts| {
        let now = std::time::Instant::now();
        now.checked_add(std::time::Duration::new(ts.tv_sec.max(0) as u64, ts.tv_nsec.clamp(0, 999_999_999) as u32)).unwrap_or(now + std::time::Duration::from_secs(1 << 32))
    });
    // The kqueue reads as ready when it has events: the thread sleeps on it
    // where a wakeup can't be missed (see `block`), then takes them.
    let got = loop {
        if p.signal_ready(t) {
            return Err(errno::EINTR);
        }
        let got = ok(unsafe { libc::kevent(kq, std::ptr::null(), 0, out.as_mut_ptr(), n as i32, &zero) } as i64)? as usize;
        if got > 0 || deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break got;
        }
        let mut fds = vec![libc::pollfd { fd: kq, events: libc::POLLIN, revents: 0 }];
        if super::block::poll(p, t, &mut fds, deadline)?.is_err() {
            // Timed out: whatever came last.
            break ok(unsafe { libc::kevent(kq, std::ptr::null(), 0, out.as_mut_ptr(), n as i32, &zero) } as i64)? as usize;
        }
    };

    // One epoll_event per guest fd, in the order kqueue reported them.
    let mut ready: Vec<(i32, u32)> = Vec::new();
    for ev in &out[..got] {
        let fd = ev.udata as usize as i32;
        let mut bits = 0;
        if ev.flags & libc::EV_ERROR != 0 {
            bits |= EPOLLERR;
        } else if ev.filter == libc::EVFILT_READ {
            bits |= EPOLLIN;
            if ev.flags & libc::EV_EOF != 0 {
                bits |= EPOLLRDHUP | EPOLLHUP;
            }
        } else if ev.filter == libc::EVFILT_WRITE {
            bits |= if ev.flags & libc::EV_EOF != 0 { EPOLLERR } else { EPOLLOUT };
        }
        match ready.iter_mut().find(|(f, _)| *f == fd) {
            Some((_, b)) => *b |= bits,
            None => ready.push((fd, bits)),
        }
    }
    let mut set = set.lock().unwrap_or_else(|e| e.into_inner());
    let mut written = 0u64;
    for (fd, bits) in ready {
        if written == maxevents {
            break;
        }
        let Some(interest) = set.get_mut(&fd) else { continue };
        // What was asked for, plus errors and hangups (always reported).
        let bits = bits & (interest.events | EPOLLERR | EPOLLHUP);
        if bits == 0 {
            continue;
        }
        let mut rec = [0u8; EVENT_SIZE as usize];
        rec[..4].copy_from_slice(&bits.to_le_bytes());
        rec[8..].copy_from_slice(&interest.data.to_le_bytes());
        mem.write(events + written * EVENT_SIZE, &rec)?;
        written += 1;
        if interest.events & EPOLLONESHOT != 0 {
            // Disarmed until EPOLL_CTL_MOD re-arms it.
            interest.events = 0;
            if let Ok(target) = p.fds.get(fd as u64) {
                let _ = register(ep.host.as_raw_fd(), target.host.as_raw_fd(), fd, None);
            }
        }
    }
    Ok(written as i64)
}
