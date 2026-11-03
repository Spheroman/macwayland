//! `futex`: what threads block on (mutexes, condition variables, joins).
//!
//! Waiters are queued by key in one table for the host process: a private
//! futex's key is its address in its process, a futex in shared memory's
//! is the object mapped there and the offset, so processes sharing memory
//! (in one VM) wait and wake each other there, as on Linux. The usual futex
//! protocol holds: a waiter checks the value and queues itself under its
//! queue's lock, and a waker (who changed the value first) takes the lock to
//! wake, so a wake can't slip between the check and the sleep.
//!
//! A waiting thread sleeps in `poll` on its own wake pipe ([`Waker`]), so a
//! wake from another thread, a host signal (`poll` returns EINTR), a timeout,
//! or the process exiting all end the wait.

use std::{
    collections::HashMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use super::{misc, Process, Thread};
use crate::{
    abi::{clock, Timespec},
    errno,
    memory::{GuestMemory, GuestMemoryExt, SharedKey},
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


/// Bits of a PI or robust futex's word, as Linux has them: the owner's tid,
/// whether others wait for it, whether its owner died holding it.
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// What a futex is known by. Linux keys a private futex by address space
/// and address, and one in shared memory by what's mapped there (inode and
/// page), so that it's the same futex in every process mapping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    /// An address in one process's memory (by its [`Futexes`]' number).
    Private(u64, u64),
    /// A word of a shared object, whoever maps it.
    Shared(SharedKey),
}

/// One wait, queued under one key, or several (`futex_waitv`).
#[derive(Debug)]
struct WaitState {
    /// -1 until woken; then which of its futexes woke it.
    woken: AtomicI64,
    waker: Arc<Waker>,
}

#[derive(Debug)]
struct Waiter {
    wait: Arc<WaitState>,
    /// Its place among the wait's futexes.
    index: i64,
    bitset: u32,
    /// The waiting thread, which a PI futex's word names once it's given
    /// the lock.
    tid: u32,
    /// It waits for a PI futex's lock (FUTEX_LOCK_PI): its wake gives it
    /// the lock, and plain wakes pass it by.
    pi: bool,
    /// The queue it's in: where it waited, or where a requeue moved it
    /// (changed with both queues' locks held).
    key: Mutex<Key>,
}

impl Waiter {
    /// Wake its wait, unless something has already (another of a
    /// `futex_waitv`'s futexes): whether this did.
    fn wake(&self) -> bool {
        let woke = self.wait.woken.compare_exchange(-1, self.index, Ordering::AcqRel, Ordering::Acquire).is_ok();
        if woke {
            self.wait.waker.wake();
        }
        woke
    }

    fn key(&self) -> Key {
        *self.key.lock().unwrap_or_else(|e| e.into_inner())
    }
}

type Queues = HashMap<Key, Vec<Arc<Waiter>>>;

/// Every futex queue of the host process, hashed into buckets with a lock
/// each, as Linux's are: one table for private and shared futexes alike,
/// so a requeue or a wake from any process finds any waiter, and a waiter
/// leaves its queue without knowing whose it is.
struct Table {
    buckets: Box<[Mutex<Queues>]>,
}

static TABLE: std::sync::LazyLock<Table> = std::sync::LazyLock::new(|| Table { buckets: (0..256).map(|_| Mutex::default()).collect() });

fn bucket(key: &Key) -> &'static Mutex<Queues> {
    let h = match *key {
        Key::Private(space, addr) => space.rotate_left(32) ^ (addr >> 2),
        Key::Shared(k) => k.object[0] ^ k.object[1].rotate_left(21) ^ (k.offset >> 2),
    };
    let h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let buckets = &TABLE.buckets;
    &buckets[(h >> 56) as usize % buckets.len()]
}

fn lock(m: &Mutex<Queues>) -> MutexGuard<'_, Queues> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Two keys' buckets, locked in a fixed order (by address), so that two
/// calls locking the same pair can't deadlock; one lock if they share one.
struct Pair {
    a: MutexGuard<'static, Queues>,
    b: Option<MutexGuard<'static, Queues>>,
}

impl Pair {
    fn lock(a: &Key, b: &Key) -> Pair {
        let (a, b) = (bucket(a), bucket(b));
        if std::ptr::eq(a, b) {
            return Pair { a: lock(a), b: None };
        }
        if (a as *const Mutex<Queues>) < (b as *const Mutex<Queues>) {
            let a = lock(a);
            Pair { a, b: Some(lock(b)) }
        } else {
            let b = lock(b);
            Pair { a: lock(a), b: Some(b) }
        }
    }

    fn a(&mut self) -> &mut Queues {
        &mut self.a
    }

    fn b(&mut self) -> &mut Queues {
        match &mut self.b {
            Some(b) => b,
            None => &mut self.a,
        }
    }
}

/// A process's private futexes: its number in the table's keys.
pub struct Futexes(u64);

impl Default for Futexes {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Futexes(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Debug for Futexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Futexes({})", self.0)
    }
}

/// The key of the futex at `uaddr`: by address in the process if the call
/// says it's private, or if it's in memory no other process maps; else by
/// the shared object and offset.
fn key_of<M: GuestMemory + ?Sized>(p: &Process, mem: &M, uaddr: u64, private: bool) -> Result<Key, i64> {
    if !private && let Some(k) = mem.shared_futex_key(uaddr)? {
        return Ok(Key::Shared(k));
    }
    Ok(Key::Private(p.futexes.0, uaddr))
}

/// Wake up to `n` waiters queued under `key` whose bitset meets `bitset`
/// (not PI waiters, whom only an unlock wakes).
fn wake_in(q: &mut Queues, key: &Key, n: u64, bitset: u32) -> i64 {
    let Some(queue) = q.get_mut(key) else { return 0 };
    let mut woken = 0;
    queue.retain(|w| {
        if woken >= n || w.pi || w.bitset & bitset == 0 {
            return true;
        }
        // A waiter something else woke first (another of its futexes) goes
        // too, uncounted.
        if w.wake() {
            woken += 1;
        }
        false
    });
    if queue.is_empty() {
        q.remove(key);
    }
    woken as i64
}

fn wake(key: Key, n: u64, bitset: u32) -> i64 {
    wake_in(&mut lock(bucket(&key)), &key, n, bitset)
}

/// Take `entries` out of whatever queues they're in now.
fn leave(entries: &[Arc<Waiter>]) {
    for e in entries {
        loop {
            let key = e.key();
            let mut q = lock(bucket(&key));
            // A requeue moved it meanwhile: to the queue it's in now.
            if e.key() != key {
                continue;
            }
            if let Some(queue) = q.get_mut(&key) {
                queue.retain(|w| !Arc::ptr_eq(w, e));
                if queue.is_empty() {
                    q.remove(&key);
                }
            }
            break;
        }
    }
}

const WAIT: u64 = 0;
const WAKE: u64 = 1;
const REQUEUE: u64 = 3;
const CMP_REQUEUE: u64 = 4;
const WAKE_OP: u64 = 5;
const LOCK_PI: u64 = 6;
const UNLOCK_PI: u64 = 7;
const TRYLOCK_PI: u64 = 8;
const WAIT_BITSET: u64 = 9;
const WAKE_BITSET: u64 = 10;
const WAIT_REQUEUE_PI: u64 = 11;
const LOCK_PI2: u64 = 13;
const PRIVATE: u64 = 128;
const CLOCK_REALTIME: u64 = 256;
const BITSET_ALL: u32 = u32::MAX;

/// futex2 flags (`futex_waitv`, `futex_wait`, `futex_wake`,
/// `futex_requeue`): the word's size, and private.
const FUTEX2_SIZE_MASK: u64 = 0x3;
const FUTEX2_SIZE_U32: u64 = 0x2;
const FUTEX2_PRIVATE: u64 = 128;
/// The most futexes one `futex_waitv` waits on.
const WAITV_MAX: u64 = 128;

/// Whether a `futex` op with this `op` argument is a PI lock, which Linux
/// makes again after a signal handler rather than failing with EINTR
/// (ERESTARTNOINTR): glibc and musl don't expect EINTR from one.
pub fn restarts_after_handler(op: u64) -> bool {
    matches!(op & !(PRIVATE | CLOCK_REALTIME), LOCK_PI | LOCK_PI2)
}

/// Whether a `futex` op waits (so EINTR and, with a timeout, ETIMEDOUT
/// are among its results).
pub fn waits(op: u64) -> bool {
    matches!(op & !(PRIVATE | CLOCK_REALTIME), WAIT | WAIT_BITSET | LOCK_PI | LOCK_PI2 | WAIT_REQUEUE_PI)
}

/// `futex(uaddr, op, val, timeout/val2, uaddr2, val3)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn futex<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> Result<i64, i64> {
    if !uaddr.is_multiple_of(4) {
        return Err(errno::EINVAL);
    }
    let private = op & PRIVATE != 0;
    let cmd = op & !(PRIVATE | CLOCK_REALTIME);
    if op & CLOCK_REALTIME != 0 && !matches!(cmd, WAIT | WAIT_BITSET | WAIT_REQUEUE_PI | LOCK_PI2) {
        return Err(errno::ENOSYS);
    }
    let key = |addr| key_of(p, mem, addr, private);
    // How many to wake: Linux wakes one even when asked for none.
    let count = |n: u64| (n as u32 as i32).max(1) as u64;
    match cmd {
        WAIT => wait(p, t, mem, Want { key: key(uaddr)?, uaddr, val: val as u32, bitset: BITSET_ALL }, deadline(mem, timeout, None)?),
        WAIT_BITSET => {
            let clock = if op & CLOCK_REALTIME != 0 { clock::REALTIME } else { clock::MONOTONIC };
            let bitset = nonzero(val3)?;
            wait(p, t, mem, Want { key: key(uaddr)?, uaddr, val: val as u32, bitset }, deadline(mem, timeout, Some(clock))?)
        }
        WAKE => Ok(wake(key(uaddr)?, count(val), BITSET_ALL)),
        WAKE_BITSET => {
            let bitset = nonzero(val3)?;
            Ok(wake(key(uaddr)?, count(val), bitset))
        }
        // `timeout` is how many to requeue.
        REQUEUE | CMP_REQUEUE => {
            if !uaddr2.is_multiple_of(4) {
                return Err(errno::EINVAL);
            }
            let cmp = (cmd == CMP_REQUEUE).then_some((uaddr, val3 as u32));
            requeue(mem, key(uaddr)?, key(uaddr2)?, val as u32 as i32 as i64, timeout as u32 as i32 as i64, cmp)
        }
        // `timeout` is how many to wake at `uaddr2`.
        WAKE_OP => {
            if !uaddr2.is_multiple_of(4) {
                return Err(errno::EINVAL);
            }
            wake_op(mem, key(uaddr)?, key(uaddr2)?, uaddr2, count(val), timeout as u32 as i32 as i64, val3 as u32)
        }
        LOCK_PI | LOCK_PI2 | TRYLOCK_PI => {
            // LOCK_PI's timeout is on CLOCK_REALTIME, LOCK_PI2's on the
            // clock the op says; both absolute.
            let clock = if cmd == LOCK_PI || op & CLOCK_REALTIME != 0 { clock::REALTIME } else { clock::MONOTONIC };
            let deadline = if cmd == TRYLOCK_PI { None } else { deadline(mem, timeout, Some(clock))? };
            match lock_pi(p, t, mem, uaddr, key(uaddr)?, deadline, cmd == TRYLOCK_PI)? {
                None => Ok(0),
                Some(w) => finish_wait(p, t, mem, w),
            }
        }
        UNLOCK_PI => unlock_pi(t, mem, uaddr, key(uaddr)?),
        // FUTEX_WAIT_REQUEUE_PI and FUTEX_CMP_REQUEUE_PI: no C library uses
        // them since glibc 2.25's condition variables.
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

/// One futex a wait is for: queued under `key` while the word at `uaddr`
/// is `val`.
struct Want {
    key: Key,
    uaddr: u64,
    val: u32,
    bitset: u32,
}

fn wait<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, want: Want, deadline: Option<Instant>) -> Result<i64, i64> {
    let w = enqueue(p, t, mem, &[want], deadline)?;
    finish_wait(p, t, mem, w)
}

/// A futex wait begun, the thread queued: what's left is to sleep until
/// woken ([`finish_wait`]), on whichever host thread.
#[derive(Debug)]
pub struct FutexWait {
    wait: Arc<WaitState>,
    entries: Vec<Arc<Waiter>>,
    deadline: Option<Instant>,
    /// Woken, it returns which futex woke it (`futex_waitv`), not 0.
    index: bool,
    /// See [`Poll`].
    poll: Option<Poll>,
}

impl FutexWait {
    fn woken(&self) -> Option<i64> {
        let i = self.wait.woken.load(Ordering::Acquire);
        (i >= 0).then_some(i)
    }
}

/// A wait on shared memory in a host of one process (1:1 mode): the
/// processes it shares the memory with are other Mac processes, whose
/// wakes can't reach it, so it looks at the word itself, from every 1 ms
/// to every 32 ms, and a word changed ends the wait as a wake would (a
/// spurious wake, as the futex protocol allows). A PI lock's wait tries to
/// take the lock instead.
#[derive(Debug)]
struct Poll {
    /// What each futex's word was when the wait began.
    words: Vec<(u64, u32)>,
    pi: bool,
    next: Instant,
    every: Duration,
}

const POLL_FIRST: Duration = Duration::from_millis(1);
const POLL_MOST: Duration = Duration::from_millis(32);

/// The wait needs to [`Poll`]: one of its futexes is in shared memory, and
/// other processes' wakes don't reach this one's waiters.
fn poll_for(p: &Process, wants: &[Want], pi: bool) -> Option<Poll> {
    if p.kernel.shares_memory() || !wants.iter().any(|w| matches!(w.key, Key::Shared(_))) {
        return None;
    }
    Some(Poll { words: wants.iter().map(|w| (w.uaddr, w.val)).collect(), pi, next: Instant::now() + POLL_FIRST, every: POLL_FIRST })
}

/// Queue the thread on each of `wants` while its word is what the caller
/// expects, checked with the queue's lock held (as the protocol needs: a
/// waker changes the word, then wakes, so a wake from then on reaches it).
/// EAGAIN if a word differs (or, for `futex_waitv`, a wait one of the
/// earlier futexes has woken by then: finished, it says which).
fn enqueue<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, wants: &[Want], deadline: Option<Instant>) -> Result<FutexWait, i64> {
    let wait = Arc::new(WaitState { woken: AtomicI64::new(-1), waker: t.waker.clone() });
    let mut entries = Vec::with_capacity(wants.len());
    for (i, want) in wants.iter().enumerate() {
        let me = Arc::new(Waiter { wait: wait.clone(), index: i as i64, bitset: want.bitset, tid: t.tid as u32, pi: false, key: Mutex::new(want.key) });
        let mut q = lock(bucket(&want.key));
        let now = mem.read_pod::<u32>(want.uaddr);
        if now != Ok(want.val) {
            drop(q);
            leave(&entries);
            if wait.woken.load(Ordering::Acquire) >= 0 {
                return Ok(FutexWait { wait, entries: Vec::new(), deadline, index: true, poll: None });
            }
            return Err(now.err().unwrap_or(errno::EAGAIN));
        }
        q.entry(want.key).or_default().push(me.clone());
        entries.push(me);
    }
    Ok(FutexWait { wait, entries, deadline, index: wants.len() > 1, poll: poll_for(p, wants, false) })
}

/// Sleep out a wait begun by a `futex` call: until woken (0, or for
/// `futex_waitv` the index of the futex that woke it; for a PI lock, the
/// lock is the thread's), a signal for the thread or the process ending
/// (EINTR), or the timeout (ETIMEDOUT).
///
/// On a CPU wanted for another thread, the thread parks instead (PARK, with
/// the wait still queued in [`Thread::take_park`]): a wake makes its task
/// runnable, and the core calls this again with the same wait, which then
/// finds it woken. Nothing is lost, as leaving the queue and waiting again
/// would lose a wake sent in between.
pub fn finish_wait<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, mut w: FutexWait) -> Result<i64, i64> {
    let mut polled = None;
    let result = loop {
        if let Some(i) = w.woken() {
            break Ok(i);
        }
        if p.signal_ready(t) || p.exiting() {
            break Err(errno::EINTR);
        }
        let now = Instant::now();
        if w.deadline.is_some_and(|d| now >= d) {
            break Err(errno::ETIMEDOUT);
        }
        if let Some(poll) = &mut w.poll
            && now >= poll.next
        {
            polled = look(t, mem, poll, &w.entries);
            if let Some(i) = polled {
                break Ok(i);
            }
            poll.next = now + poll.every;
            poll.every = (poll.every * 2).min(POLL_MOST);
        }
        let until = match (w.deadline, w.poll.as_ref().map(|p| p.next)) {
            (Some(d), Some(n)) => Some(d.min(n)),
            (d, n) => d.or(n),
        };
        if super::signal::must_yield() {
            t.set_park(super::Park { watch: Vec::new(), deadline: until, futex: Some(w), files: Vec::new() });
            return Err(errno::PARK);
        }
        t.waker.sleep(until);
    };
    let result = match result {
        // Woken by a wake, which took the entry it woke out of its queue;
        // a `futex_waitv`'s others (or a wait a look ended) are still in
        // theirs.
        Ok(i) => {
            if w.entries.len() > 1 || polled.is_some() {
                leave(&w.entries);
            }
            Ok(w.woken().unwrap_or(i))
        }
        // Out of the queues; a wake that got there first counts (for a PI
        // lock, it gave the thread the lock).
        Err(e) => {
            leave(&w.entries);
            w.woken().ok_or(e)
        }
    };
    result.map(|i| if w.index { i } else { 0 })
}

/// A [`Poll`]'s look: Some(index) if the wait is over, the word at that
/// index changed (or, for a PI lock, taken; the waiter still queued).
fn look<M: GuestMemory + ?Sized>(t: &Thread, mem: &M, poll: &Poll, entries: &[Arc<Waiter>]) -> Option<i64> {
    if poll.pi {
        let (uaddr, entry) = (poll.words[0].0, entries.first()?);
        let tid = t.tid as u32 & FUTEX_TID_MASK;
        let key = entry.key();
        let q = lock(bucket(&key));
        let others = q.get(&key).is_some_and(|v| v.iter().any(|w| w.pi && !Arc::ptr_eq(w, entry)));
        let old = mem
            .update_u32(uaddr, &mut |v| (v & FUTEX_TID_MASK == 0).then_some(tid | (v & FUTEX_OWNER_DIED) | if others { FUTEX_WAITERS } else { 0 }))
            .ok()?;
        return (old & FUTEX_TID_MASK == 0).then_some(0);
    }
    poll.words.iter().position(|&(uaddr, val)| mem.read_pod::<u32>(uaddr).is_ok_and(|v| v != val)).map(|i| i as i64)
}

/// Wake `n_wake` waiters on `k1` and move up to `n_requeue` more to `k2`
/// (FUTEX_REQUEUE; FUTEX_CMP_REQUEUE if `cmp` has the word at its address
/// to be, looked at with both queues locked). How many were woken and
/// moved.
fn requeue<M: GuestMemory + ?Sized>(mem: &M, k1: Key, k2: Key, n_wake: i64, n_requeue: i64, cmp: Option<(u64, u32)>) -> Result<i64, i64> {
    if n_wake < 0 || n_requeue < 0 {
        return Err(errno::EINVAL);
    }
    let mut pair = Pair::lock(&k1, &k2);
    if let Some((uaddr, val)) = cmp
        && mem.read_pod::<u32>(uaddr)? != val
    {
        return Err(errno::EAGAIN);
    }
    let queue = pair.a().remove(&k1).unwrap_or_default();
    let (mut woken, mut keep, mut moved) = (0, Vec::new(), Vec::new());
    for w in queue {
        if w.pi {
            keep.push(w);
        } else if w.wait.woken.load(Ordering::Acquire) >= 0 {
            // Woken through another of its futexes: done with this queue.
        } else if woken < n_wake {
            if w.wake() {
                woken += 1;
            }
        } else if (moved.len() as i64) < n_requeue {
            *w.key.lock().unwrap_or_else(|e| e.into_inner()) = k2;
            moved.push(w);
        } else {
            keep.push(w);
        }
    }
    if !keep.is_empty() {
        pair.a().insert(k1, keep);
    }
    let n = moved.len() as i64;
    if n > 0 {
        pair.b().entry(k2).or_default().extend(moved);
    }
    Ok(woken + n)
}

/// FUTEX_WAKE_OP: change the word at `uaddr2` (`k2`) by `encoded`'s
/// operation, atomically, then wake `n1` waiters on `k1`, and `n2` on
/// `k2` too if the word's old value passes `encoded`'s comparison.
fn wake_op<M: GuestMemory + ?Sized>(mem: &M, k1: Key, k2: Key, uaddr2: u64, n1: u64, n2: i64, encoded: u32) -> Result<i64, i64> {
    let op = (encoded >> 28) & 7;
    let cmp = (encoded >> 24) & 15;
    // 12-bit fields, sign-extended.
    let mut oparg = ((encoded << 8) as i32) >> 20;
    let cmparg = ((encoded << 20) as i32) >> 20;
    if encoded & (8 << 28) != 0 {
        // FUTEX_OP_OPARG_SHIFT: the argument is a shift (Linux takes it
        // modulo 32).
        oparg = 1i32.wrapping_shl((oparg & 31) as u32);
    }
    if op > 4 || cmp > 5 {
        return Err(errno::ENOSYS);
    }
    let mut pair = Pair::lock(&k1, &k2);
    let old = mem.update_u32(uaddr2, &mut |v| {
        let a = oparg as u32;
        Some(match op {
            0 => a,
            1 => v.wrapping_add(a),
            2 => v | a,
            3 => v & !a,
            _ => v ^ a,
        })
    })?;
    let mut woken = wake_in(pair.a(), &k1, n1, BITSET_ALL);
    let (old, cmparg) = (old as i32, cmparg);
    let pass = match cmp {
        0 => old == cmparg,
        1 => old != cmparg,
        2 => old < cmparg,
        3 => old <= cmparg,
        4 => old > cmparg,
        _ => old >= cmparg,
    };
    if pass && n2 > 0 {
        woken += wake_in(pair.b(), &k2, n2 as u64, BITSET_ALL);
    }
    Ok(woken)
}

/// FUTEX_LOCK_PI (or TRYLOCK_PI, `try_only`): take the lock whose word is
/// at `uaddr` if no one holds it (keeping FUTEX_OWNER_DIED for the caller
/// to see, as Linux does), or queue for it, the word marked
/// FUTEX_WAITERS so its owner's unlock comes here. None: taken. EDEADLK if
/// the caller holds it; EAGAIN for a try while another does.
///
/// No priorities to inherit: threads have none yet (`docs/mn-threads.md`
/// step 5); what's kept is the protocol, an unlock handing the lock to
/// the first waiter.
fn lock_pi<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, key: Key, deadline: Option<Instant>, try_only: bool) -> Result<Option<FutexWait>, i64> {
    let tid = t.tid as u32 & FUTEX_TID_MASK;
    let mut q = lock(bucket(&key));
    let waiting = q.get(&key).is_some_and(|v| v.iter().any(|w| w.pi));
    let old = mem.update_u32(uaddr, &mut |v| match v & FUTEX_TID_MASK {
        0 => Some(tid | (v & FUTEX_OWNER_DIED) | if waiting { FUTEX_WAITERS } else { 0 }),
        o if o == tid || try_only => None,
        _ => (v & FUTEX_WAITERS == 0).then_some(v | FUTEX_WAITERS),
    })?;
    match old & FUTEX_TID_MASK {
        0 => return Ok(None),
        o if o == tid => return Err(errno::EDEADLK),
        _ if try_only => return Err(errno::EAGAIN),
        _ => {}
    }
    let wait = Arc::new(WaitState { woken: AtomicI64::new(-1), waker: t.waker.clone() });
    let me = Arc::new(Waiter { wait: wait.clone(), index: 0, bitset: BITSET_ALL, tid, pi: true, key: Mutex::new(key) });
    q.entry(key).or_default().push(me.clone());
    let poll = poll_for(p, &[Want { key, uaddr, val: 0, bitset: BITSET_ALL }], true);
    Ok(Some(FutexWait { wait, entries: vec![me], deadline, index: false, poll }))
}

/// FUTEX_UNLOCK_PI: the caller gives the lock up, to the first thread
/// waiting for it (the word names it, with FUTEX_WAITERS, as Linux leaves
/// it), or to no one. EPERM if the caller doesn't hold it.
fn unlock_pi<M: GuestMemory + ?Sized>(t: &Thread, mem: &M, uaddr: u64, key: Key) -> Result<i64, i64> {
    let tid = t.tid as u32 & FUTEX_TID_MASK;
    let mut q = lock(bucket(&key));
    let next = q.get(&key).and_then(|v| v.iter().position(|w| w.pi));
    let new = next.map_or(0, |i| q[&key][i].tid | FUTEX_WAITERS);
    let mut held = true;
    mem.update_u32(uaddr, &mut |v| {
        held = v & FUTEX_TID_MASK == tid;
        held.then_some(new)
    })?;
    if !held {
        return Err(errno::EPERM);
    }
    if let Some(i) = next {
        let queue = q.get_mut(&key).expect("the queue just looked at");
        let heir = queue.remove(i);
        if queue.is_empty() {
            q.remove(&key);
        }
        heir.wake();
    }
    Ok(0)
}

/// Thread `tid` died holding the robust futex at `uaddr`, if its word
/// still names it (Linux's `handle_futex_death`): the word gets
/// FUTEX_OWNER_DIED, and a waiter is woken to find it so (EOWNERDEAD from
/// its lock call); a PI futex goes to its first waiter, marked so. A
/// `pending` entry (the list's op in progress) with no owner wakes a
/// waiter too, in case the lock was let go just before the death.
fn owner_died<M: GuestMemory + ?Sized>(p: &Process, mem: &M, uaddr: u64, tid: u32, pi: bool, pending: bool) {
    if !uaddr.is_multiple_of(4) {
        return;
    }
    // Keyed as a shared futex: a robust mutex may be in shared memory, and
    // the waiters in other processes.
    let Ok(key) = key_of(p, mem, uaddr, false) else { return };
    let mut q = lock(bucket(&key));
    // A PI futex goes to its first waiter whether or not the list marks
    // the entry as PI (musl doesn't), as Linux hands a dead owner's PI
    // futexes on (`exit_pi_state_list`) whatever the list says.
    let heir = q.get(&key).and_then(|v| v.iter().position(|w| w.pi));
    let pi = pi || heir.is_some();
    let heir_tid = heir.map(|i| q[&key][i].tid);
    let Ok(old) = mem.update_u32(uaddr, &mut |v| {
        (v & FUTEX_TID_MASK == tid).then_some(match heir_tid {
            Some(h) => h | FUTEX_WAITERS | FUTEX_OWNER_DIED,
            None => (v & FUTEX_WAITERS) | FUTEX_OWNER_DIED,
        })
    }) else {
        return;
    };
    if old & FUTEX_TID_MASK != tid {
        if pending && !pi && old & FUTEX_TID_MASK == 0 {
            wake_in(&mut q, &key, 1, BITSET_ALL);
        }
        return;
    }
    if let Some(i) = heir {
        let queue = q.get_mut(&key).expect("the queue just looked at");
        let heir = queue.remove(i);
        if queue.is_empty() {
            q.remove(&key);
        }
        heir.wake();
    } else if !pi && old & FUTEX_WAITERS != 0 {
        wake_in(&mut q, &key, 1, BITSET_ALL);
    }
}

/// The size of a `struct robust_list_head` (`set_robust_list`'s length).
pub(super) const ROBUST_HEAD_SIZE: u64 = 24;

/// Thread `tid` is gone: each robust futex on its list (`set_robust_list`,
/// `head`) that it still holds is marked as its owner's death leaves it,
/// as Linux's `exit_robust_list` does. The list is in the guest's memory,
/// and may be anything: at most 2048 entries, and a bad pointer ends it.
pub(super) fn exit_robust_list<M: GuestMemory + ?Sized>(p: &Process, mem: &M, tid: i64, head: u64) {
    let read = |a: u64| mem.read_pod::<u64>(a).ok();
    let (Some(first), Some(offset), Some(pending)) = (read(head), read(head + 8), read(head + 16)) else { return };
    let offset = offset as i64;
    let tid = tid as u32 & FUTEX_TID_MASK;
    let (pending, pending_pi) = (pending & !1, pending & 1 != 0);
    let (mut entry, mut pi) = (first & !1, first & 1 != 0);
    let mut limit = 2048;
    while entry != head && entry != 0 {
        // The next first: the death may let another thread free this one.
        let next = read(entry);
        if entry != pending {
            owner_died(p, mem, entry.wrapping_add_signed(offset), tid, pi, false);
        }
        let Some(next) = next else { break };
        (entry, pi) = (next & !1, next & 1 != 0);
        limit -= 1;
        if limit == 0 {
            break;
        }
    }
    if pending != 0 {
        owner_died(p, mem, pending.wrapping_add_signed(offset), tid, pending_pi, true);
    }
}

/// A thread's exit wakes a waiter on its CLONE_CHILD_CLEARTID word
/// (`pthread_join`), keyed as a shared futex, as Linux wakes it.
pub(super) fn wake_cleared_tid<M: GuestMemory + ?Sized>(p: &Process, mem: &M, addr: u64) -> i64 {
    match key_of(p, mem, addr, false) {
        Ok(key) => wake(key, 1, BITSET_ALL),
        Err(_) => 0,
    }
}

/// futex2's flags for a futex: whether it's private. Only 32-bit words,
/// as Linux has so far; no NUMA.
fn futex2_private(flags: u64) -> Result<bool, i64> {
    if flags & !(FUTEX2_SIZE_MASK | FUTEX2_PRIVATE) != 0 || flags & FUTEX2_SIZE_MASK != FUTEX2_SIZE_U32 {
        return Err(errno::EINVAL);
    }
    Ok(flags & FUTEX2_PRIVATE != 0)
}

/// An absolute timeout on `clockid` (futex2 calls), or none.
fn futex2_deadline<M: GuestMemory + ?Sized>(mem: &M, timeout: u64, clockid: u64) -> Result<Option<Instant>, i64> {
    if timeout == 0 {
        return Ok(None);
    }
    let clock = match clockid as i32 as i64 as u64 {
        c @ (clock::REALTIME | clock::MONOTONIC) => c,
        _ => return Err(errno::EINVAL),
    };
    deadline(mem, timeout, Some(clock))
}

/// One `struct futex_waitv`: the value, address and flags of a futex.
fn read_waitv<M: GuestMemory + ?Sized>(p: &Process, mem: &M, at: u64) -> Result<Want, i64> {
    let val: u64 = mem.read_pod(at)?;
    let uaddr: u64 = mem.read_pod(at + 8)?;
    let flags: u32 = mem.read_pod(at + 16)?;
    let reserved: u32 = mem.read_pod(at + 20)?;
    let private = futex2_private(flags as u64)?;
    if reserved != 0 || val > u32::MAX as u64 || !uaddr.is_multiple_of(4) {
        return Err(errno::EINVAL);
    }
    Ok(Want { key: key_of(p, mem, uaddr, private)?, uaddr, val: val as u32, bitset: BITSET_ALL })
}

/// `futex_waitv(waiters, nr_futexes, flags, timeout, clockid)`: wait on up
/// to 128 futexes at once (Proton's fsync): the index
/// of the one that woke it.
#[allow(clippy::too_many_arguments)]
pub(super) fn waitv<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, waiters: u64, nr: u64, flags: u64, timeout: u64, clockid: u64) -> Result<i64, i64> {
    if flags != 0 || nr == 0 || nr > WAITV_MAX {
        return Err(errno::EINVAL);
    }
    let deadline = futex2_deadline(mem, timeout, clockid)?;
    let wants = (0..nr).map(|i| read_waitv(p, mem, waiters + i * 24)).collect::<Result<Vec<_>, _>>()?;
    let mut w = enqueue(p, t, mem, &wants, deadline)?;
    w.index = true;
    finish_wait(p, t, mem, w)
}

/// `futex_wait(uaddr, val, mask, flags, timeout, clockid)` (futex2).
#[allow(clippy::too_many_arguments)]
pub(super) fn wait2<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, uaddr: u64, val: u64, mask: u64, flags: u64, timeout: u64, clockid: u64) -> Result<i64, i64> {
    let private = futex2_private(flags)?;
    if !uaddr.is_multiple_of(4) || val > u32::MAX as u64 || mask > u32::MAX as u64 {
        return Err(errno::EINVAL);
    }
    let bitset = nonzero(mask)?;
    let deadline = futex2_deadline(mem, timeout, clockid)?;
    wait(p, t, mem, Want { key: key_of(p, mem, uaddr, private)?, uaddr, val: val as u32, bitset }, deadline)
}

/// `futex_wake(uaddr, mask, nr, flags)` (futex2).
pub(super) fn wake2<M: GuestMemory + ?Sized>(p: &Process, mem: &M, uaddr: u64, mask: u64, nr: u64, flags: u64) -> Result<i64, i64> {
    let private = futex2_private(flags)?;
    if !uaddr.is_multiple_of(4) || mask > u32::MAX as u64 {
        return Err(errno::EINVAL);
    }
    let bitset = nonzero(mask)?;
    let n = (nr as u32 as i32).max(0) as u64;
    if n == 0 {
        return Ok(0);
    }
    Ok(wake(key_of(p, mem, uaddr, private)?, n, bitset))
}

/// `futex_requeue(waiters, flags, nr_wake, nr_requeue)` (futex2): from
/// `waiters[0]`, whose word must be its value, to `waiters[1]`.
pub(super) fn requeue2<M: GuestMemory + ?Sized>(p: &Process, mem: &M, waiters: u64, flags: u64, nr_wake: u64, nr_requeue: u64) -> Result<i64, i64> {
    if flags != 0 {
        return Err(errno::EINVAL);
    }
    let from = read_waitv(p, mem, waiters)?;
    let to = read_waitv(p, mem, waiters + 24)?;
    requeue(mem, from.key, to.key, nr_wake as u32 as i32 as i64, nr_requeue as u32 as i32 as i64, Some((from.uaddr, from.val)))
}
