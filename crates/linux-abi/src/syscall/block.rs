//! Sleeping in a host call without missing a wakeup.
//!
//! A thread blocked in a host call is reached by an interrupt (a host signal
//! to its host thread, see the core's wakers): the call returns EINTR, and
//! the thread looks at why. But an interrupt that lands after the thread
//! looked and before the call sleeps is lost, and the thread sleeps through
//! what it was woken for (a signal, or its process ending: with many
//! processes in a host, that process then never finishes).
//!
//! So a thread waiting here sleeps in `poll` on what it waits for **and its
//! waker's pipe** ([`Waker`]). Whoever wakes a thread changes what it should
//! look at first, then writes the pipe, then interrupts it: the mark first.
//! The thread looks (a signal it would act on, its process ending, its CPU
//! wanted) and then sleeps on the pipe too: a wake after the look leaves a
//! byte that ends the sleep at once, however it's timed against the
//! interrupt. Bytes from wakes before the look are taken out and looked
//! again for, so they cost a turn of the loop, never an early return.
//!
//! Calls that wait on a file (a pipe, socket, terminal) wait here for it to
//! be ready, then make the call, which won't block unless another reader or
//! writer got there first (then it blocks as before, still interruptible).
//!
//! **Parking** (docs/processes.md, step 3a). On a CPU that's wanted for
//! another thread (the core's yield check: tasks wait for a CPU, or this
//! one is being taken back), a wait doesn't sleep in the host call at all:
//! the files it waits for and its deadline are registered in the thread's
//! waker kqueue ([`super::Waker::arm`]), and the call returns
//! [`errno::PARK`]. The core sets the task aside, watching the waker's
//! kqueue in a kqueue of its own, and makes the call again on a CPU once
//! something comes (a file ready, the deadline, a wake): blocked threads
//! cost no host thread. The call made again keeps its deadline
//! ([`Thread::deadline`]). Nothing a call does before it waits here may be
//! visible, as it's made again from the start.

use std::{os::fd::AsRawFd, time::Instant};

use super::{signal::must_yield, Process, Thread};
use crate::{errno, fs::OpenFile};

/// Why a [`poll`] ended without a file ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Woken {
    /// The deadline passed.
    TimedOut,
}

/// Whether thread `t` has something to do other than wait: a signal it
/// would act on, or its process ending. (Its CPU wanted for another thread
/// parks the wait instead.)
fn called_away(p: &Process, t: &Thread) -> bool {
    p.exiting() || p.signal_ready(t)
}

/// Park thread `t`'s call (see the module's comment) to wait until
/// `deadline`, or a wake: PARK.
fn park(t: &Thread, deadline: Option<Instant>) -> i64 {
    t.set_park(super::Park { watch: Vec::new(), deadline, futex: None, files: Vec::new() });
    errno::PARK
}

/// Wait until one of `fds` is ready (the count of those that are), until
/// `deadline` (Err(TimedOut) as Ok), or until thread `t` is called away
/// (EINTR). `fds` may be empty: a sleep. On a CPU wanted elsewhere, PARK
/// (or OFFLOAD) instead of a sleep.
pub(super) fn poll(p: &Process, t: &Thread, fds: &mut Vec<libc::pollfd>, deadline: Option<Instant>) -> Result<Result<i64, Woken>, i64> {
    wait_for(p, t, fds, t.deadline(deadline), true)
}

/// [`poll`]; `exact`: false for one file the call will try itself, which
/// needs no word from `poll` on what it's ready for.
///
/// A sleep here is `poll` on the files and the waker's kqueue. Parking
/// instead, the files are registered in the waker's kqueue first (one-shot,
/// in the same call that takes what's there), which says at once whether
/// one is ready, and that kqueue is known to take them, so the core's will
/// too; then they're handed to the core. (Not `poll` with a zero timeout to
/// look first: on macOS that takes ~10 µs when nothing is ready.)
fn wait_for(p: &Process, t: &Thread, fds: &mut Vec<libc::pollfd>, deadline: Option<Instant>, exact: bool) -> Result<Result<i64, Woken>, i64> {
    if fds.is_empty() {
        return sleep_until(p, t, deadline).map(|()| Err(Woken::TimedOut));
    }
    fds.push(libc::pollfd { fd: t.waker.fd(), events: libc::POLLIN, revents: 0 });
    let real = fds.len() - 1;
    let result = loop {
        // Look, then sleep on the waker too (see the module's comment).
        if called_away(p, t) {
            break Err(errno::EINTR);
        }
        if must_yield() {
            let watch: Vec<(i32, i16)> = fds[..real].iter().map(|f| (f.fd, f.events)).collect();
            let Ok(armed) = t.waker.arm(&watch, None) else {
                // A file kqueue won't take: a worker waits.
                fds.pop();
                return Err(errno::OFFLOAD);
            };
            if armed.ready {
                t.waker.unregister();
                if !exact {
                    break Ok(Ok(1));
                }
                // Something is ready: poll says what (at once).
                let n = unsafe { libc::poll(fds.as_mut_ptr(), real as libc::nfds_t, 0) };
                if n > 0 {
                    break Ok(Ok(n as i64));
                }
                // (Taken by another reader meanwhile: wait on.)
            } else if !armed.woken {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    t.waker.unregister();
                    break Ok(Err(Woken::TimedOut));
                }
                // The core watches the files now; this kqueue keeps only
                // what has come (a wake) for the core to look for.
                t.waker.unregister();
                fds.pop();
                t.set_park(super::Park { watch, deadline, futex: None, files: Vec::new() });
                return Err(errno::PARK);
            }
            t.waker.unregister();
            continue;
        }
        let ms = match deadline {
            None => -1,
            // A timer in the waker's kqueue: to the nanosecond.
            Some(d) if t.waker.arm_timer(d) => -1,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                // Round up: never wake before the deadline.
                (left.as_nanos().div_ceil(1_000_000)).min(i32::MAX as u128) as i32
            }
        };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if e == libc::EINTR {
                continue; // an interrupt: look again
            }
            break Err(errno::from_darwin(e));
        }
        let waker = fds.last_mut().unwrap();
        let woken = waker.revents != 0;
        waker.revents = 0;
        let ready = n as i64 - woken as i64;
        if woken {
            t.waker.drain();
        }
        if ready > 0 {
            break Ok(Ok(ready));
        }
        if n == 0 || deadline.is_some_and(|d| Instant::now() >= d) {
            break Ok(Err(Woken::TimedOut));
        }
    };
    fds.pop();
    if deadline.is_some() {
        // The timer, if it hasn't fired.
        t.waker.drain();
    }
    result
}

/// Sleep until `deadline` (None: for ever) or until thread `t` is called
/// away (EINTR). Ok once the time is up.
pub(super) fn sleep(p: &Process, t: &Thread, deadline: Option<Instant>) -> Result<(), i64> {
    sleep_until(p, t, t.deadline(deadline))
}

fn sleep_until(p: &Process, t: &Thread, deadline: Option<Instant>) -> Result<(), i64> {
    loop {
        if called_away(p, t) {
            return Err(errno::EINTR);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
        if must_yield() {
            return Err(park(t, deadline));
        }
        // On the waker's kqueue alone: its timeout is to the nanosecond,
        // where poll's is to the millisecond.
        t.waker.sleep(deadline);
    }
}

/// Wait once for anything to wake thread `t` (or for `deadline`), for a
/// caller that looks at what it waits for itself, before and after: a
/// signal it takes (`rt_sigtimedwait`), a child (`wait4`). Ok(false) once
/// the deadline has passed. Not called away by signals (the caller looks);
/// on a CPU wanted elsewhere, PARK.
pub fn wait_woken(t: &Thread, deadline: Option<Instant>) -> Result<bool, i64> {
    let deadline = t.deadline(deadline);
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Ok(false);
    }
    if must_yield() {
        return Err(park(t, deadline));
    }
    t.waker.sleep(deadline);
    Ok(deadline.is_none_or(|d| Instant::now() < d))
}

/// Whether a host file can make a call on it block: a pipe, socket,
/// terminal or other device; not a regular file or directory.
pub(crate) fn can_block(host: i32) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(host, &mut st) } != 0 {
        return false;
    }
    matches!(st.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK | libc::S_IFCHR)
}

/// Before a call on `file` that waits for it to be ready for `events`
/// (POLLIN or POLLOUT): wait for that here, where a wakeup can't be missed.
/// At once for files that never block, one that's ready (or hung up, or in
/// error: the call says so), or one opened nonblocking (the call says
/// EAGAIN).
pub(super) fn ready(p: &Process, t: &Thread, file: &std::sync::Arc<OpenFile>, events: i16) -> Result<(), i64> {
    // Held for the call (and while it waits parked).
    crate::fs::hold(file);
    ready_fd(p, t, file.host.as_raw_fd(), file.can_block(), events)
}

/// [`ready`], for a host fd whose kind is known (`can_block`).
pub(super) fn ready_fd(p: &Process, t: &Thread, host: i32, can_block: bool, events: i16) -> Result<(), i64> {
    if !can_block {
        return Ok(());
    }
    let mut pf = libc::pollfd { fd: host, events, revents: 0 };
    // Ready now (or in error, which the call reports). Not an interrupted
    // look (-1, EINTR): that one may be the wakeup, and the call would then
    // sleep through it; the wait below looks again.
    let n = unsafe { libc::poll(&mut pf, 1, 0) };
    if n > 0 || (n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)) {
        return Ok(());
    }
    // Opened nonblocking: the call says EAGAIN itself.
    if unsafe { libc::fcntl(host, libc::F_GETFL) } & libc::O_NONBLOCK != 0 {
        return Ok(());
    }
    // A socket's receive or send timeout (SO_RCVTIMEO, SO_SNDTIMEO) bounds
    // the wait, which then says EAGAIN, as the call would have.
    let deadline = t.deadline(socket_timeout(host, events).map(|d| Instant::now() + d));
    // Ready (or in error, which the call reports) as soon as kqueue says so.
    match wait_for(p, t, &mut vec![libc::pollfd { fd: host, events, revents: 0 }], deadline, false)? {
        Ok(_) => Ok(()),
        Err(Woken::TimedOut) => Err(errno::EAGAIN),
    }
}

/// A socket's timeout for waits for `events` (POLLIN: SO_RCVTIMEO; else
/// SO_SNDTIMEO), if it's a socket and has one.
fn socket_timeout(host: i32, events: i16) -> Option<std::time::Duration> {
    let opt = if events & libc::POLLIN != 0 { libc::SO_RCVTIMEO } else { libc::SO_SNDTIMEO };
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    if unsafe { libc::getsockopt(host, libc::SOL_SOCKET, opt, (&raw mut tv).cast(), &mut len) } != 0 {
        return None; // not a socket
    }
    let d = std::time::Duration::new(tv.tv_sec.max(0) as u64, tv.tv_usec.max(0) as u32 * 1000);
    (!d.is_zero()).then_some(d)
}

