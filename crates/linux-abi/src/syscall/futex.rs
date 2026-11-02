//! `futex`: what threads block on (mutexes, condition variables, joins).
//!
//! Each process keeps a table of waiters by guest address. The usual futex
//! protocol holds: a waiter checks the value and queues itself under the
//! table's lock, and a waker (who changed the value first) takes the lock to
//! wake, so a wake can't slip between the check and the sleep.
//!
//! A waiting thread sleeps in `poll` on its own wake pipe ([`Waker`]), so a
//! wake from another thread, a host signal (`poll` returns EINTR), a timeout,
//! or the process exiting all end the wait.

use std::{
    collections::HashMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use super::{misc, Process, Thread};
use crate::{
    abi::{clock, Timespec},
    errno,
    memory::{GuestMemory, GuestMemoryExt},
};

/// A thread's way of being woken from a blocking wait: a nonblocking pipe.
#[derive(Debug)]
pub struct Waker {
    read: OwnedFd,
    write: OwnedFd,
}

impl Waker {
    pub fn new() -> Waker {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe for a thread waker");
        for fd in fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Waker { read, write }
    }

    /// Wake the thread (from any thread; async-signal-safe).
    pub fn wake(&self) {
        unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    /// The pipe's read end, to sleep on with other things (see `block`).
    pub(super) fn fd(&self) -> i32 {
        self.read.as_raw_fd()
    }

    /// Take out the wakes so far.
    pub(super) fn drain(&self) {
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }

    /// Sleep until woken, a host signal arrives, or `deadline`. True if woken
    /// (the wakes so far are used up).
    pub fn sleep(&self, deadline: Option<Instant>) -> bool {
        let ms = match deadline {
            None => -1,
            Some(d) => d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32,
        };
        let mut pfd = libc::pollfd { fd: self.read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let woken = unsafe { libc::poll(&mut pfd, 1, ms) } > 0;
        if woken {
            let mut buf = [0u8; 64];
            while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
        woken
    }
}

impl Default for Waker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Waiter {
    bitset: u32,
    woken: AtomicBool,
    waker: Arc<Waker>,
    /// The address whose queue it's in: where it waited, or where a requeue
    /// moved it (changed under the table's lock).
    queue: AtomicU64,
}

/// A process's futex waiters, by guest address.
#[derive(Default)]
pub struct Futexes(Mutex<HashMap<u64, Vec<Arc<Waiter>>>>);

impl std::fmt::Debug for Futexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Futexes")
    }
}

impl Futexes {
    /// Wake up to `n` waiters on `addr` whose bitset meets `bitset`.
    pub fn wake(&self, addr: u64, n: u64, bitset: u32) -> i64 {
        let mut table = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = table.get_mut(&addr) else { return 0 };
        let mut woken = 0;
        queue.retain(|w| {
            if woken < n && w.bitset & bitset != 0 {
                w.woken.store(true, Ordering::Release);
                w.waker.wake();
                woken += 1;
                false
            } else {
                true
            }
        });
        if queue.is_empty() {
            table.remove(&addr);
        }
        woken as i64
    }
}

const WAIT: u64 = 0;
const WAKE: u64 = 1;
const REQUEUE: u64 = 3;
const CMP_REQUEUE: u64 = 4;
const WAIT_BITSET: u64 = 9;
const WAKE_BITSET: u64 = 10;
const PRIVATE: u64 = 128;
const CLOCK_REALTIME: u64 = 256;
const BITSET_ALL: u32 = u32::MAX;

/// `futex(uaddr, op, val, timeout/val2, uaddr2, val3)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn futex<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> Result<i64, i64> {
    if !uaddr.is_multiple_of(4) {
        return Err(errno::EINVAL);
    }
    // Shared and private futexes are kept alike: by address in this process.
    match op & !(PRIVATE | CLOCK_REALTIME) {
        WAIT => wait(p, t, mem, uaddr, val as u32, deadline(mem, timeout, None)?, BITSET_ALL),
        WAIT_BITSET => {
            let clock = if op & CLOCK_REALTIME != 0 { clock::REALTIME } else { clock::MONOTONIC };
            wait(p, t, mem, uaddr, val as u32, deadline(mem, timeout, Some(clock))?, nonzero(val3)?)
        }
        WAKE => Ok(p.futexes.wake(uaddr, val, BITSET_ALL)),
        WAKE_BITSET => Ok(p.futexes.wake(uaddr, val, nonzero(val3)?)),
        cmd @ (REQUEUE | CMP_REQUEUE) => {
            if cmd == CMP_REQUEUE && mem.read_pod::<u32>(uaddr)? != val3 as u32 {
                return Err(errno::EAGAIN);
            }
            // `timeout` is the number to requeue.
            requeue(p, uaddr, val, timeout, uaddr2)
        }
        _ => {
            p.unimplemented(format!("futex op {op:#x}"));
            Err(errno::ENOSYS)
        }
    }
}

fn nonzero(bitset: u64) -> Result<u32, i64> {
    if bitset as u32 == 0 { Err(errno::EINVAL) } else { Ok(bitset as u32) }
}

/// When a wait with `timeout` (relative, or absolute on `clock`) ends.
fn deadline<M: GuestMemory + ?Sized>(mem: &M, timeout: u64, clock: Option<u64>) -> Result<Option<Instant>, i64> {
    if timeout == 0 {
        return Ok(None);
    }
    let ts: Timespec = mem.read_pod(timeout)?;
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(errno::EINVAL);
    }
    let mut wait = Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
    if let Some(clock) = clock {
        let now = misc::clock_now(clock)?;
        wait = wait.saturating_sub(now);
    }
    Ok(Some(Instant::now() + wait))
}

fn wait<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, val: u32, deadline: Option<Instant>, bitset: u32) -> Result<i64, i64> {
    let w = enqueue(p, t, mem, uaddr, val, deadline, bitset)?;
    finish_wait(p, t, w)
}

/// A futex wait begun, the thread queued: what's left is to sleep until
/// woken ([`finish_wait`]), on whichever host thread.
#[derive(Debug)]
pub struct FutexWait {
    me: Arc<Waiter>,
    deadline: Option<Instant>,
}

/// Begin a `futex` call's wait, if it's one (FUTEX_WAIT, FUTEX_WAIT_BITSET):
/// check the value and queue the thread, in one step under the table's lock
/// (as the protocol needs: a wake sent from then on reaches it). The sleep
/// can then go elsewhere ([`finish_wait`]): the core hands it to another host
/// thread, so as to run another guest thread here. Ok(None): not a wait;
/// Err: the call's result, at once (EAGAIN: the value has changed).
pub fn begin_wait<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, args: [u64; 6]) -> Result<Option<FutexWait>, i64> {
    let [uaddr, op, val, timeout, _, val3] = args;
    if !uaddr.is_multiple_of(4) {
        return Err(errno::EINVAL);
    }
    let (deadline, bitset) = match op & !(PRIVATE | CLOCK_REALTIME) {
        WAIT => (deadline(mem, timeout, None)?, BITSET_ALL),
        WAIT_BITSET => {
            let clock = if op & CLOCK_REALTIME != 0 { clock::REALTIME } else { clock::MONOTONIC };
            (deadline(mem, timeout, Some(clock))?, nonzero(val3)?)
        }
        _ => return Ok(None),
    };
    enqueue(p, t, mem, uaddr, val as u32, deadline, bitset).map(Some)
}

fn enqueue<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, val: u32, deadline: Option<Instant>, bitset: u32) -> Result<FutexWait, i64> {
    let me = Arc::new(Waiter { bitset, woken: AtomicBool::new(false), waker: t.waker.clone(), queue: AtomicU64::new(uaddr) });
    let mut table = p.futexes.0.lock().unwrap_or_else(|e| e.into_inner());
    if mem.read_pod::<u32>(uaddr)? != val {
        return Err(errno::EAGAIN);
    }
    table.entry(uaddr).or_default().push(me.clone());
    Ok(FutexWait { me, deadline })
}

/// Sleep out a wait [`begin_wait`] began: until woken (0), a signal for the
/// thread or the process ending (EINTR), or the timeout (ETIMEDOUT).
pub fn finish_wait(p: &Process, t: &Thread, w: FutexWait) -> Result<i64, i64> {
    let FutexWait { me, deadline } = w;
    let result = loop {
        if me.woken.load(Ordering::Acquire) {
            break Ok(0);
        }
        if p.signal_ready(t) || p.exiting() || super::signal::must_yield() {
            break Err(errno::EINTR);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break Err(errno::ETIMEDOUT);
        }
        t.waker.sleep(deadline);
    };
    if result.is_err() {
        // Leave the queue — unless a wake got here first, which counts. The
        // queue it's in now: a requeue may have moved it (left in the one it
        // was moved to, it would take a later wake meant for another).
        let mut table = p.futexes.0.lock().unwrap_or_else(|e| e.into_inner());
        if me.woken.load(Ordering::Acquire) {
            return Ok(0);
        }
        let addr = me.queue.load(Ordering::Relaxed);
        if let Some(queue) = table.get_mut(&addr) {
            queue.retain(|w| !Arc::ptr_eq(w, &me));
            if queue.is_empty() {
                table.remove(&addr);
            }
        }
    }
    result
}

/// Wake `n_wake` waiters on `uaddr` and move up to `n_requeue` more to
/// `uaddr2`.
fn requeue(p: &Process, uaddr: u64, n_wake: u64, n_requeue: u64, uaddr2: u64) -> Result<i64, i64> {
    let mut table = p.futexes.0.lock().unwrap_or_else(|e| e.into_inner());
    let mut queue = table.remove(&uaddr).unwrap_or_default();
    let n = (n_wake as usize).min(queue.len());
    for w in queue.drain(..n) {
        w.woken.store(true, Ordering::Release);
        w.waker.wake();
    }
    let m = (n_requeue as usize).min(queue.len());
    let moved: Vec<_> = queue.drain(..m).collect();
    for w in &moved {
        w.queue.store(uaddr2, Ordering::Relaxed);
    }
    if !queue.is_empty() {
        table.insert(uaddr, queue);
    }
    if !moved.is_empty() {
        table.entry(uaddr2).or_default().extend(moved);
    }
    Ok((n + m) as i64)
}
