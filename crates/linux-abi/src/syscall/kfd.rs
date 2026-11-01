//! `eventfd` and `timerfd`: files Linux makes out of a counter or a timer,
//! emulated on a host kqueue so they can be polled (and watched by epoll):
//! the kqueue is readable exactly when the guest's `read` would succeed.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Mutex,
    time::{Duration, Instant},
};

use super::{misc, ok, Process};
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
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    ok(unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, &zero) } as i64).map(|_| ())
}

pub(super) fn user_event(flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent { ident: 1, filter: libc::EVFILT_USER, flags, fflags, data: 0, udata: std::ptr::null_mut() }
}

/// Take whatever the kqueue has ready (so it reads as not ready again).
pub(super) fn drain(kq: RawFd) {
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
    while unsafe { libc::kevent(kq, std::ptr::null(), 0, out.as_mut_ptr(), 4, &zero) } > 0 {}
}

/// Block until the kqueue has something (a signal interrupts: EINTR).
pub(super) fn wait(kq: RawFd) -> Result<(), i64> {
    let mut out: libc::kevent = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null()) } as i64).map(|_| ())
}

pub(super) fn nonblocking(file: &OpenFile) -> bool {
    let flags = unsafe { libc::fcntl(file.host.as_raw_fd(), libc::F_GETFL) };
    flags & libc::O_NONBLOCK != 0
}

impl EventFd {
    /// For a forked child: (count, semaphore).
    pub fn state(&self) -> (u64, bool) {
        (self.count, self.semaphore)
    }
}

impl TimerFd {
    /// For a forked child: (clock, interval, time to the next expiry).
    pub fn state(&self) -> (u64, Duration, Option<Duration>) {
        (self.clock, self.interval, self.deadline.map(|d| d.saturating_duration_since(Instant::now())))
    }
}

/// A forked child's copy of an eventfd.
pub(crate) fn eventfd_copy(count: u64, semaphore: bool, nonblock: bool) -> Result<FdEntry, i64> {
    let kq = new_kqueue(nonblock)?;
    change(kq.as_raw_fd(), user_event(libc::EV_ADD | libc::EV_CLEAR, 0))?;
    if count != 0 {
        change(kq.as_raw_fd(), user_event(0, libc::NOTE_TRIGGER))?;
    }
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::EventFd(Mutex::new(EventFd { count, semaphore }));
    Ok(entry)
}

/// A forked child's copy of a timerfd.
pub(crate) fn timerfd_copy(clock: u64, interval: Duration, left: Option<Duration>, nonblock: bool) -> Result<FdEntry, i64> {
    let kq = new_kqueue(nonblock)?;
    let deadline = left.map(|l| Instant::now() + l);
    arm(kq.as_raw_fd(), deadline)?;
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::TimerFd(Mutex::new(TimerFd { clock, deadline, interval }));
    Ok(entry)
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
pub(crate) fn eventfd_read(file: &OpenFile, state: &Mutex<EventFd>, buf: &mut [u8]) -> Result<i64, i64> {
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
        wait(kq)?;
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
pub(crate) fn timerfd_read(file: &OpenFile, timer: &Mutex<TimerFd>, buf: &mut [u8]) -> Result<i64, i64> {
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
        wait(kq)?;
    }
}
