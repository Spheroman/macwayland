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

/// A thread's way of being woken from a blocking wait: a kqueue of its own.
///
/// A user event in it (ident [`WAKE_EVENT`]) is the wake: whoever wakes the thread
/// triggers it, and the kqueue reads as ready until the thread takes it. A
/// thread sleeping in a host call sleeps on the kqueue's fd too (`poll`, see
/// `block`), so a wake ends the sleep however it's timed. A thread whose
/// task has left its CPU to wait (parked, by the core) has the files it waits
/// for and its deadline registered in the kqueue as well, and the core
/// watches the kqueue itself, in a kqueue of the host's: whatever comes, the
/// task is made runnable. For waits with nothing to watch (a futex, a
/// child), the core's unpark hook makes it runnable directly.
pub struct Waker {
    kq: OwnedFd,
    /// The core's: make the thread's task runnable if it's parked, or else
    /// call the function it's given (which triggers the wake's event).
    unpark: std::sync::OnceLock<Box<dyn Fn(&dyn Fn()) + Send + Sync>>,
    /// What a parked wait registered (ident, filter), to take out again
    /// when the wait is over.
    registered: Mutex<Vec<(usize, i16)>>,
}

impl std::fmt::Debug for Waker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Waker({})", self.kq.as_raw_fd())
    }
}

/// The wake's user event, and a parked wait's timer, in a thread's kqueue.
const WAKE_EVENT: usize = 0;
const TIMER: usize = 1;

fn kev(ident: usize, filter: i16, flags: u16, fflags: u32, data: isize) -> libc::kevent {
    libc::kevent { ident, filter, flags, fflags, data, udata: std::ptr::null_mut() }
}

impl Waker {
    pub fn new() -> Waker {
        let kq = unsafe { libc::kqueue() };
        assert!(kq >= 0, "kqueue for a thread waker: {}", std::io::Error::last_os_error());
        unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) };
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        let ev = kev(WAKE_EVENT, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, 0);
        assert_eq!(super::kfd::now(kq.as_raw_fd(), &[ev], &mut []), 0, "a thread waker's event");
        Waker { kq, unpark: std::sync::OnceLock::new(), registered: Mutex::default() }
    }

    /// The core's way of making the thread's task runnable while it's
    /// parked (set once, as the thread becomes a task).
    pub fn set_unpark(&self, unpark: Box<dyn Fn(&dyn Fn()) + Send + Sync>) {
        let _ = self.unpark.set(unpark);
    }

    /// Wake the thread (from any thread, not a signal handler): its task
    /// runs again if it's parked, else a sleep in a host call ends (or the
    /// next one doesn't start).
    pub fn wake(&self) {
        match self.unpark.get() {
            Some(unpark) => unpark(&|| self.trigger()),
            None => self.trigger(),
        }
    }

    /// For the core, parking the thread's task: whether something came to
    /// the kqueue since the thread last looked (a wake, a signal it
    /// watches), taken out. Then the task looks again rather than park.
    pub fn take_wake(&self) -> bool {
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let mut any = false;
        while super::kfd::now(self.kq.as_raw_fd(), &[], &mut out) > 0 {
            any = true;
        }
        any
    }

    /// Take out what a wait registered, leaving what has come (a wake) for
    /// the next look: a wait that parks hands its files to the core.
    pub(super) fn unregister(&self) {
        let old = std::mem::take(&mut *self.registered.lock().unwrap_or_else(|e| e.into_inner()));
        if old.is_empty() {
            return;
        }
        // Receipts (no events taken) for each: errors for what had fired.
        let changes: Vec<libc::kevent> = old.iter().map(|&(ident, filter)| kev(ident, filter, libc::EV_DELETE | libc::EV_RECEIPT, 0, 0)).collect();
        let mut out = changes.clone();
        super::kfd::now(self.kq.as_raw_fd(), &changes, &mut out);
    }

    /// The wake's event, alone (async-signal-safe).
    pub fn trigger(&self) {
        let ev = kev(WAKE_EVENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
        super::kfd::now(self.kq.as_raw_fd(), &[ev], &mut []);
    }

    /// The kqueue's fd: readable while something's come (to sleep on with
    /// other things, see `block`; the core watches it for a parked task).
    pub fn fd(&self) -> i32 {
        self.kq.as_raw_fd()
    }

    /// Take out whatever has come so far, and what a parked wait
    /// registered: a wait that's over leaves nothing behind that would end
    /// the next one early.
    pub fn drain(&self) {
        let old = std::mem::take(&mut *self.registered.lock().unwrap_or_else(|e| e.into_inner()));
        let changes: Vec<libc::kevent> = old.iter().map(|&(ident, filter)| kev(ident, filter, libc::EV_DELETE, 0, 0)).collect();
        let mut out: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        // (Deletes of what has fired already fail, one event each in `out`.)
        let mut n = super::kfd::now(self.kq.as_raw_fd(), &changes, &mut out);
        while n == out.len() as i32 {
            n = super::kfd::now(self.kq.as_raw_fd(), &[], &mut out);
        }
    }

    /// Sleep until woken (anything coming into the kqueue), a host signal
    /// interrupts, or `deadline`. True unless the deadline passed; what
    /// came is taken out.
    ///
    /// Asleep in `poll` on the kqueue's fd, not in `kevent`: a thread asleep
    /// in `kevent` takes ~8 µs longer to wake (measured, wakebench futex: 36
    /// µs a round trip against 12.5), every time. The deadline is a timer
    /// in the kqueue, to the nanosecond (`poll`'s own timeout is in ms).
    pub fn sleep(&self, deadline: Option<Instant>) -> bool {
        let ms = match deadline {
            None => -1,
            Some(d) if self.arm_timer(d) => -1,
            Some(d) => d.saturating_duration_since(Instant::now()).as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32,
        };
        let mut pf = libc::pollfd { fd: self.kq.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pf, 1, ms) };
        if n > 0 || deadline.is_some() {
            self.drain();
        }
        deadline.is_none_or(|d| Instant::now() < d)
    }

    /// A timer in the kqueue for `deadline` (taken out by [`Waker::drain`]):
    /// false if it couldn't be set.
    pub(super) fn arm_timer(&self, deadline: Instant) -> bool {
        let ns = deadline.saturating_duration_since(Instant::now()).as_nanos().clamp(1, isize::MAX as u128) as isize;
        let ev = kev(TIMER, libc::EVFILT_TIMER, libc::EV_ADD | libc::EV_ONESHOT, libc::NOTE_NSECONDS | libc::NOTE_CRITICAL, ns);
        if super::kfd::now(self.kq.as_raw_fd(), &[ev], &mut []) < 0 {
            return false;
        }
        self.registered.lock().unwrap_or_else(|e| e.into_inner()).push((TIMER, libc::EVFILT_TIMER));
        true
    }

    /// Register `fds` (host fd, poll events; one-shot) and the `deadline`
    /// in the kqueue, so that it becomes ready when any of them is (or the
    /// time is up), and take what has come already, in one call: whether
    /// one of the files is ready, or something else came (a wake, the
    /// timer). What stays registered is for a sleep on the kqueue
    /// ([`Waker::wait`]) or a parked wait (the core watches the kqueue).
    /// Err if a file can't be watched this way (kqueue refuses it, or the
    /// events have no filter): nothing is left registered then.
    pub(super) fn arm(&self, fds: &[(i32, i16)], deadline: Option<Instant>) -> Result<Armed, ()> {
        let mut changes = Vec::new();
        for &(fd, events) in fds {
            if fd < 0 {
                continue;
            }
            let mut any = false;
            if events & (libc::POLLIN | libc::POLLRDNORM) != 0 {
                changes.push(kev(fd as usize, libc::EVFILT_READ, libc::EV_ADD | libc::EV_ONESHOT, 0, 0));
                any = true;
            }
            if events & (libc::POLLOUT | libc::POLLWRNORM) != 0 {
                changes.push(kev(fd as usize, libc::EVFILT_WRITE, libc::EV_ADD | libc::EV_ONESHOT, 0, 0));
                any = true;
            }
            // Only exceptional conditions (POLLPRI, or hangups and errors
            // alone): kqueue's filters would say ready for plain data too,
            // and the wait would never settle.
            if !any {
                return Err(());
            }
        }
        if let Some(d) = deadline {
            let ns = d.saturating_duration_since(Instant::now()).as_nanos().clamp(1, isize::MAX as u128) as isize;
            changes.push(kev(TIMER, libc::EVFILT_TIMER, libc::EV_ADD | libc::EV_ONESHOT, libc::NOTE_NSECONDS | libc::NOTE_CRITICAL, ns));
        }
        self.registered.lock().unwrap_or_else(|e| e.into_inner()).extend(changes.iter().map(|c| (c.ident, c.filter)));
        // Room for an error per change, and what was there already.
        let mut out: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; changes.len() + 4];
        let mut armed = Armed::default();
        loop {
            let n = super::kfd::now(self.kq.as_raw_fd(), &changes, &mut out);
            if n < 0 {
                self.drain();
                return Err(());
            }
            for e in &out[..n as usize] {
                match e.filter {
                    _ if e.flags & libc::EV_ERROR != 0 => {
                        if e.data != 0 {
                            self.drain();
                            return Err(());
                        }
                    }
                    libc::EVFILT_READ | libc::EVFILT_WRITE => armed.ready = true,
                    _ => armed.woken = true,
                }
            }
            if (n as usize) < out.len() {
                return Ok(armed);
            }
            changes.clear(); // more to take
        }
    }

    /// Watch host signal `sig` (as sent to the host process) in the kqueue
    /// from now on: a wait for a host child (1:1) wakes on SIGCHLD.
    pub(super) fn watch_signal(&self, sig: i32) {
        let ev = kev(sig as usize, libc::EVFILT_SIGNAL, libc::EV_ADD, 0, 0);
        super::kfd::now(self.kq.as_raw_fd(), &[ev], &mut []);
    }
}

/// What [`Waker::arm`] found had come already.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Armed {
    /// One of the files is ready (or in error).
    pub ready: bool,
    /// Something else: a wake, the timer, a signal watched.
    pub woken: bool,
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
///
/// On a CPU wanted for another thread, the thread parks instead (PARK, with
/// the wait still queued in [`Thread::take_park`]): a wake makes its task
/// runnable, and the core calls this again with the same wait, which then
/// finds it woken. Nothing is lost, as leaving the queue and waiting again
/// would lose a wake sent in between.
pub fn finish_wait(p: &Process, t: &Thread, w: FutexWait) -> Result<i64, i64> {
    let FutexWait { me, deadline } = w;
    let result = loop {
        if me.woken.load(Ordering::Acquire) {
            break Ok(0);
        }
        if p.signal_ready(t) || p.exiting() {
            break Err(errno::EINTR);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break Err(errno::ETIMEDOUT);
        }
        if super::signal::must_yield() {
            t.set_park(super::Park { watch: Vec::new(), deadline, futex: Some(FutexWait { me, deadline }), files: Vec::new() });
            return Err(errno::PARK);
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
