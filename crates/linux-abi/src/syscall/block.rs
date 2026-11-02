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
/// would act on, its process ending, or its CPU wanted for another thread.
fn called_away(p: &Process, t: &Thread) -> bool {
    p.exiting() || p.signal_ready(t) || must_yield()
}

/// Wait until one of `fds` is ready (the count of those that are), until
/// `deadline` (Err(TimedOut) as Ok), or until thread `t` is called away
/// (EINTR). `fds` may be empty: a sleep.
pub(super) fn poll(p: &Process, t: &Thread, fds: &mut Vec<libc::pollfd>, deadline: Option<Instant>) -> Result<Result<i64, Woken>, i64> {
    fds.push(libc::pollfd { fd: t.waker.fd(), events: libc::POLLIN, revents: 0 });
    let result = loop {
        // Look, then sleep on the pipe too (see the module's comment).
        if called_away(p, t) {
            break Err(errno::EINTR);
        }
        let ms = match deadline {
            None => -1,
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
    result
}

/// Sleep until `deadline` (None: for ever) or until thread `t` is called
/// away (EINTR).
pub(super) fn sleep(p: &Process, t: &Thread, deadline: Option<Instant>) -> Result<(), i64> {
    poll(p, t, &mut Vec::new(), deadline).map(|_| ())
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
pub(super) fn ready(p: &Process, t: &Thread, file: &OpenFile, events: i16) -> Result<(), i64> {
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
    // sleep through it; `poll` below looks again.
    let n = unsafe { libc::poll(&mut pf, 1, 0) };
    if n > 0 || (n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)) {
        return Ok(());
    }
    if unsafe { libc::fcntl(host, libc::F_GETFL) } & libc::O_NONBLOCK != 0 {
        return Ok(());
    }
    pf.revents = 0;
    // A socket's receive or send timeout (SO_RCVTIMEO, SO_SNDTIMEO) bounds
    // the wait, which then says EAGAIN, as the call would have.
    let deadline = socket_timeout(host, events).map(|d| Instant::now() + d);
    match poll(p, t, &mut vec![pf], deadline)? {
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

