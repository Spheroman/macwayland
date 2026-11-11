//! `eventfd` and `timerfd`: files Linux makes out of a counter or a timer,
//! emulated on a host kqueue so they can be polled (and watched by epoll):
//! the kqueue is readable exactly when the guest's `read` would succeed.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Mutex,
    time::{Duration, Instant},
};

use super::{misc, ok, Process, Thread};
use crate::{
    abi::Timespec,
    errno,
    fs::{FdEntry, FileKind, OpenFile},
    memory::{GuestMemory, GuestMemoryExt},
};

const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;
const EFD_SEMAPHORE: u64 = 1;
const TFD_TIMER_ABSTIME: u64 = 1;

/// An eventfd's counter.
#[derive(Debug)]
pub struct EventFd {
    count: u64,
    semaphore: bool,
}

/// A timerfd's timer.
#[derive(Debug)]
pub struct TimerFd {
    clock: u64,
    /// When it next expires (None: disarmed).
    deadline: Option<Instant>,
    interval: Duration,
}

pub(super) fn new_kqueue(nonblock: bool) -> Result<OwnedFd, i64> {
    let kq = ok(unsafe { libc::kqueue() } as i64)? as RawFd;
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    if nonblock {
        unsafe { libc::fcntl(kq.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    }
    Ok(kq)
}

pub(super) fn change(kq: RawFd, ev: libc::kevent) -> Result<(), i64> {
    ok(now(kq, &[ev], &mut []) as i64).map(|_| ())
}

pub(super) fn user_event(flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent { ident: 1, filter: libc::EVFILT_USER, flags, fflags, data: 0, udata: std::ptr::null_mut() }
}

/// Take whatever the kqueue has ready (so it reads as not ready again).
pub(super) fn drain(kq: RawFd) {
    let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
    while now(kq, &[], &mut out) > 0 {}
}

/// `kevent` that doesn't wait: apply `changes`, and take what's ready into
/// `out` (the count, or -1 with errno set).
///
/// Every kqueue of ours is used through `kevent64` alone (a kqueue takes
/// one kind of call or the other, and refuses the other kind once used):
/// a look with nothing there takes ~11 µs through `kevent` with a zero
/// timeout (it goes the way of a timed wait), and 0.13 µs through
/// `kevent64` with KEVENT_FLAG_IMMEDIATE (measured, M5 Pro).
pub fn now(kq: RawFd, changes: &[libc::kevent], out: &mut [libc::kevent]) -> i32 {
    kevent64(kq, changes, out, libc::KEVENT_FLAG_IMMEDIATE, None)
}

/// `kevent`, through `kevent64` (see [`now`]): waiting up to `timeout`
/// (None: for ever) unless `flags` says KEVENT_FLAG_IMMEDIATE.
pub fn kevent64(kq: RawFd, changes: &[libc::kevent], out: &mut [libc::kevent], flags: u32, timeout: Option<&libc::timespec>) -> i32 {
    let wide = |e: &libc::kevent| libc::kevent64_s {
        ident: e.ident as u64,
        filter: e.filter,
        flags: e.flags,
        fflags: e.fflags,
        data: e.data as i64,
        udata: e.udata as u64,
        ext: [0; 2],
    };
    // On the stack for the usual few; (a signal handler calls this too,
    // with one change and no events: no allocation then).
    let mut small_changes: [libc::kevent64_s; 4] = unsafe { std::mem::zeroed() };
    let big_changes: Vec<libc::kevent64_s>;
    let changes64: &[libc::kevent64_s] = if changes.len() <= 4 {
        for (w, e) in small_changes.iter_mut().zip(changes) {
            *w = wide(e);
        }
        &small_changes[..changes.len()]
    } else {
        big_changes = changes.iter().map(wide).collect();
        &big_changes
    };
    let mut small_out: [libc::kevent64_s; 4] = unsafe { std::mem::zeroed() };
    let mut big_out: Vec<libc::kevent64_s>;
    let out64: &mut [libc::kevent64_s] = if out.len() <= 4 {
        &mut small_out[..out.len()]
    } else {
        big_out = vec![unsafe { std::mem::zeroed() }; out.len()];
        &mut big_out
    };
    let n = unsafe {
        libc::kevent64(kq, changes64.as_ptr(), changes64.len() as i32, out64.as_mut_ptr(), out64.len() as i32, flags, timeout.map_or(std::ptr::null(), |t| t))
    };
    for (o, e) in out.iter_mut().zip(&out64[..n.max(0) as usize]) {
        *o = libc::kevent { ident: e.ident as usize, filter: e.filter, flags: e.flags, fflags: e.fflags, data: e.data as isize, udata: e.udata as usize as *mut libc::c_void };
    }
    n
}

/// Block until the kqueue has something (a signal interrupts: EINTR): for
/// a host thread of the runtime's own.
pub(super) fn wait_raw(kq: RawFd) -> Result<(), i64> {
    let mut out: [libc::kevent; 1] = unsafe { std::mem::zeroed() };
    ok(kevent64(kq, &[], &mut out, 0, None) as i64).map(|_| ())
}

/// Thread `t` of `p` waits until the kqueue has something, or it's called
/// away (EINTR): see `block`.
pub(super) fn wait(p: &Process, t: &Thread, kq: RawFd) -> Result<(), i64> {
    super::block::poll(p, t, &mut vec![libc::pollfd { fd: kq, events: libc::POLLIN, revents: 0 }], None).map(|_| ())
}

pub(super) fn nonblocking(file: &OpenFile) -> bool {
    let flags = unsafe { libc::fcntl(file.host.as_raw_fd(), libc::F_GETFL) };
    flags & libc::O_NONBLOCK != 0
}

pub(super) fn eventfd2(p: &Process, initval: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(O_NONBLOCK | O_CLOEXEC | EFD_SEMAPHORE) != 0 {
        return Err(errno::EINVAL);
    }
    let kq = new_kqueue(flags & O_NONBLOCK != 0)?;
    // A user event, triggered while the count is non-zero.
    change(kq.as_raw_fd(), user_event(libc::EV_ADD | libc::EV_CLEAR, 0))?;
    if initval as u32 != 0 {
        change(kq.as_raw_fd(), user_event(0, libc::NOTE_TRIGGER))?;
    }
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::EventFd(Mutex::new(EventFd { count: initval as u32 as u64, semaphore: flags & EFD_SEMAPHORE != 0 }));
    Ok(p.fds.insert(entry.cloexec(flags & O_CLOEXEC != 0)))
}

/// `read` on an eventfd: the count (or 1, for a semaphore), which resets.
pub(crate) fn eventfd_read(p: &Process, t: &Thread, file: &OpenFile, state: &Mutex<EventFd>, buf: &mut [u8]) -> Result<i64, i64> {
    if buf.len() < 8 {
        return Err(errno::EINVAL);
    }
    let kq = file.host.as_raw_fd();
    loop {
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.count > 0 {
                let v = if s.semaphore { 1 } else { s.count };
                s.count -= v;
                drain(kq);
                if s.count > 0 {
                    change(kq, user_event(0, libc::NOTE_TRIGGER))?;
                }
                buf[..8].copy_from_slice(&v.to_le_bytes());
                return Ok(8);
            }
        }
        if nonblocking(file) {
            return Err(errno::EAGAIN);
        }
        wait(p, t, kq)?;
    }
}

/// `write` on an eventfd: add to the count.
pub(crate) fn eventfd_write(file: &OpenFile, state: &Mutex<EventFd>, data: &[u8]) -> Result<i64, i64> {
    if data.len() < 8 {
        return Err(errno::EINVAL);
    }
    let v = u64::from_le_bytes(data[..8].try_into().unwrap());
    if v == u64::MAX {
        return Err(errno::EINVAL);
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    // Linux blocks rather than overflow; it never comes up in practice.
    s.count = s.count.checked_add(v).filter(|&c| c < u64::MAX).ok_or(errno::EAGAIN)?;
    if s.count > 0 {
        change(file.host.as_raw_fd(), user_event(0, libc::NOTE_TRIGGER))?;
    }
    Ok(8)
}

pub(super) fn timerfd_create(p: &Process, clockid: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    misc::clock_now(clockid)?; // a clock we know
    let kq = new_kqueue(flags & O_NONBLOCK != 0)?;
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::TimerFd(Mutex::new(TimerFd { clock: clockid, deadline: None, interval: Duration::ZERO }));
    Ok(p.fds.insert(entry.cloexec(flags & O_CLOEXEC != 0)))
}

fn timer_of(file: &OpenFile) -> Result<&Mutex<TimerFd>, i64> {
    match &file.kind {
        FileKind::TimerFd(t) => Ok(t),
        _ => Err(errno::EINVAL),
    }
}

/// Arm the kqueue's timer for `deadline` (or take it off).
fn arm(kq: RawFd, deadline: Option<Instant>) -> Result<(), i64> {
    drain(kq);
    let _ = change(kq, libc::kevent { ident: 1, filter: libc::EVFILT_TIMER, flags: libc::EV_DELETE, fflags: 0, data: 0, udata: std::ptr::null_mut() });
    if let Some(d) = deadline {
        let ns = d.saturating_duration_since(Instant::now()).as_nanos().clamp(1, isize::MAX as u128) as isize;
        let ev = libc::kevent {
            ident: 1,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_NSECONDS,
            data: ns,
            udata: std::ptr::null_mut(),
        };
        change(kq, ev)?;
    }
    Ok(())
}

fn to_duration(t: Timespec) -> Result<Duration, i64> {
    if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
        return Err(errno::EINVAL);
    }
    Ok(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
}

fn to_timespec(d: Duration) -> Timespec {
    Timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 }
}

/// The current setting: (interval, time to the next expiry).
fn current(t: &TimerFd) -> [Timespec; 2] {
    let left = t.deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()).max(Duration::from_nanos(1)));
    [to_timespec(t.interval), to_timespec(if t.deadline.is_some() { left } else { Duration::ZERO })]
}

pub(super) fn timerfd_settime<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, flags: u64, new: u64, old: u64) -> Result<i64, i64> {
    if flags & !(TFD_TIMER_ABSTIME | 2 /* CANCEL_ON_SET */) != 0 {
        return Err(errno::EINVAL);
    }
    let file = p.fds.get(fd)?;
    let timer = timer_of(&file)?;
    let [interval, value]: [Timespec; 2] = mem.read_pod(new)?;
    let (interval, value) = (to_duration(interval)?, to_duration(value)?);
    let mut t = timer.lock().unwrap_or_else(|e| e.into_inner());
    if old != 0 {
        mem.write_pod(old, &current(&t))?;
    }
    t.interval = interval;
    t.deadline = if value.is_zero() {
        None
    } else if flags & TFD_TIMER_ABSTIME != 0 {
        Some(Instant::now() + value.saturating_sub(misc::clock_now(t.clock)?))
    } else {
        Some(Instant::now() + value)
    };
    arm(file.host.as_raw_fd(), t.deadline)?;
    Ok(0)
}

pub(super) fn timerfd_gettime<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, cur: u64) -> Result<i64, i64> {
    let file = p.fds.get(fd)?;
    let t = timer_of(&file)?.lock().unwrap_or_else(|e| e.into_inner());
    mem.write_pod(cur, &current(&t))?;
    Ok(0)
}

/// `read` on a timerfd: how many times it has expired since the last read.
pub(crate) fn timerfd_read(p: &Process, t: &Thread, file: &OpenFile, timer: &Mutex<TimerFd>, buf: &mut [u8]) -> Result<i64, i64> {
    if buf.len() < 8 {
        return Err(errno::EINVAL);
    }
    let kq = file.host.as_raw_fd();
    loop {
        {
            let mut t = timer.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if let Some(d) = t.deadline.filter(|&d| now >= d) {
                let n = if t.interval.is_zero() {
                    t.deadline = None;
                    1
                } else {
                    let missed = ((now - d).as_nanos() / t.interval.as_nanos()) as u64;
                    t.deadline = Some(d + t.interval * (missed as u32 + 1));
                    missed + 1
                };
                arm(kq, t.deadline)?;
                buf[..8].copy_from_slice(&n.to_le_bytes());
                return Ok(8);
            }
            if t.deadline.is_none() && nonblocking(file) {
                return Err(errno::EAGAIN);
            }
        }
        if nonblocking(file) {
            return Err(errno::EAGAIN);
        }
        wait(p, t, kq)?;
    }
}
