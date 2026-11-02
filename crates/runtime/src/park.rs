//! Parked tasks: blocked threads that cost no host thread
//! (docs/processes.md, step 3a).
//!
//! A system call that would block, on a CPU wanted for another task, doesn't
//! sleep in the host: `linux_abi` says what it waits for (host fds and the
//! events, a deadline) and returns PARK (see `linux_abi::syscall::block`),
//! and the task is set aside here, by its location's id, its registers
//! saved. The host has a kqueue: each parked task's files are registered in
//! it (one-shot, through a `dup` of each, so that every registration is a
//! knote of its own however many tasks wait for one file), with its
//! deadline as a timer, all with the task's id. A few waiter threads wait
//! in it ([`run_waiters`]) and make a task runnable as soon as one of them
//! fires. The call is then made again on a CPU (its dups closed, which takes
//! their knotes out).
//!
//! Waits in memory (a futex, a child in the process table, a signal) have
//! nothing to register but the deadline: their wakers reach the thread's
//! waker, whose unpark hook (installed here, [`Parking::adopt`]) takes the
//! task out and queues it directly, without the waiter threads.
//!
//! Whoever takes a task out of its shard owns it: the hook, a waiter thread,
//! or the end of the host. A wake that finds no task parked triggers the
//! waker's own event instead, under the shard's lock; a task parking looks
//! for that event under the same lock before it goes in, so a wake can't
//! fall between the call's last look and the park.
//!
//! (Not the thread's waker kqueue nested in the host's, watching what it
//! has registered: macOS refuses (EINVAL) to add a kqueue holding another
//! kqueue to one that's watched, and an epoll set holding eventfds, which
//! are kqueues, is just that.)

use std::{
    collections::HashMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
    time::Instant,
};

use linux_abi::syscall::kevent_now;

use crate::{
    sched::{Job, Placed, Sched, Start, Task},
    Proc,
};

/// User events in the host's kqueue: tasks to place (with a CPU to start,
/// say), the host ending, a host signal (1:1). Tasks' timers are idents
/// from their ids, which start well above these.
const PLACE: usize = 1;
const STOP: usize = 2;
const HOST_SIGNAL: usize = 3;
const FIRST_ID: u64 = 1 << 16;

const SHARDS: usize = 32;

/// What a CPU, worker or waiter thread is to do with a task let run again by
/// someone who couldn't do it themselves (no thread scope to start a CPU
/// or worker in).
pub(crate) enum Ready {
    /// Put it on the run queue (or on a new CPU).
    Place(Task, Start),
    /// The run queue made room for a new CPU for it: start one.
    NewCpu(Task, Start),
    /// The host is ending: its thread is gone.
    Gone(Task),
    /// Its wait can't be watched here (a file the host's kqueue refuses):
    /// a worker makes the call.
    Offload(Job),
}

/// A parked task, and the dups of the files it waits for (closed as it's
/// taken out, which takes their registrations out of the kqueue).
struct Parked {
    task: Task,
    start: Start,
    dups: Vec<OwnedFd>,
    timer: bool,
}

/// A host's parked tasks and the kqueue that watches them.
pub(crate) struct Parking {
    kq: OwnedFd,
    shards: [Mutex<HashMap<u64, Parked>>; SHARDS],
    sched: Arc<Sched>,
    ready: Mutex<Vec<Ready>>,
    any_ready: AtomicBool,
    /// 1:1: the process host signals are for, whose threads a host signal
    /// (or SIGCHLD, for a parked `wait4`) wakes: a parked one has no host
    /// thread for it to land on.
    host_signals_for: Mutex<Option<Weak<Proc>>>,
}

/// The kqueue of the host whose waiters a host signal's handler tells (the
/// host running now; a test binary runs several, one after another).
static HOST_KQ: AtomicI32 = AtomicI32::new(-1);

fn kev(ident: usize, filter: i16, flags: u16, fflags: u32, data: isize, udata: u64) -> libc::kevent {
    libc::kevent { ident, filter, flags, fflags, data, udata: udata as usize as *mut libc::c_void }
}

/// One change to the kqueue (through `kevent64`, as every kqueue of the
/// runtime's: see `linux_abi::syscall::kevent_now`). Its error, if any.
fn change(kq: i32, ev: libc::kevent) -> Result<(), i32> {
    let mut out = [ev];
    let mut ev = ev;
    ev.flags |= libc::EV_RECEIPT;
    match kevent_now(kq, &[ev], &mut out) {
        n if n < 0 => Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL)),
        1 if out[0].flags & libc::EV_ERROR != 0 && out[0].data != 0 => Err(out[0].data as i32),
        _ => Ok(()),
    }
}

fn trigger(kq: i32, ident: usize) {
    kevent_now(kq, &[kev(ident, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0, 0)], &mut []);
}

/// From a host signal's handler: the waiters wake the threads it's for
/// (async-signal-safe: one `kevent64`, no allocation).
pub(crate) fn host_signal_arrived() {
    let kq = HOST_KQ.load(Ordering::Acquire);
    if kq >= 0 {
        trigger(kq, HOST_SIGNAL);
    }
}

/// A new id for a task's location (never reused, so a stale event names no
/// one).
pub(crate) fn new_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(FIRST_ID);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Parking {
    pub fn new(sched: Arc<Sched>) -> std::io::Result<Arc<Parking>> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) };
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        // STOP stays triggered once it is (no EV_CLEAR): every waiter sees it.
        for (ident, flags) in [(PLACE, libc::EV_ADD | libc::EV_CLEAR), (STOP, libc::EV_ADD), (HOST_SIGNAL, libc::EV_ADD | libc::EV_CLEAR)] {
            change(kq.as_raw_fd(), kev(ident, libc::EVFILT_USER, flags, 0, 0, 0)).map_err(std::io::Error::from_raw_os_error)?;
        }
        HOST_KQ.store(kq.as_raw_fd(), Ordering::Release);
        sched.set_stop_kq(kq.as_raw_fd());
        Ok(Arc::new(Parking {
            kq,
            shards: std::array::from_fn(|_| Mutex::default()),
            sched,
            ready: Mutex::default(),
            any_ready: AtomicBool::new(false),
            host_signals_for: Mutex::new(None),
        }))
    }

    fn shard(&self, id: u64) -> MutexGuard<'_, HashMap<u64, Parked>> {
        self.shards[id as usize % SHARDS].lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 1:1: host signals are for `proc`'s threads; and a SIGCHLD to the
    /// host wakes them too (a `wait4` for a host child parks on it).
    pub fn host_signals_for(&self, proc: &Arc<Proc>) {
        *self.host_signals_for.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(proc));
        let _ = change(self.kq.as_raw_fd(), kev(libc::SIGCHLD as usize, libc::EVFILT_SIGNAL, libc::EV_ADD, 0, 0, 0));
    }

    /// Make `task`'s thread one this host can unpark: its waker's hook.
    /// Not parked, the hook triggers the waker's event (`trigger`) under the
    /// shard's lock, which [`Parking::park`] looks for under the same lock.
    pub fn adopt(self: &Arc<Self>, task: &Task) {
        let me = Arc::downgrade(self);
        let id = task.location.id;
        task.thread.waker.set_unpark(Box::new(move |trigger: &dyn Fn()| match me.upgrade() {
            Some(p) => p.unpark_or(id, trigger),
            None => trigger(),
        }));
    }

    /// Set `task` aside until what its call waits for comes (or its unpark
    /// hook is called): `start` is how it carries on (the call made again),
    /// with `watch`, the host fds and poll events it waits for, and its
    /// `deadline`.
    pub fn park(&self, task: Task, start: Start, watch: &[(i32, i16)], deadline: Option<Instant>) {
        let id = task.location.id;
        let kq = self.kq.as_raw_fd();
        let mut shard = self.shard(id);
        // Woken since the call last looked (and before this lock): back to
        // a CPU, to look again.
        if task.thread.waker.take_wake() {
            drop(shard);
            self.requeue(task, start);
            return;
        }
        let mut dups = Vec::new();
        let mut failed = false;
        for &(fd, events) in watch {
            if fd < 0 {
                continue;
            }
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                failed = true;
                break;
            }
            let dup = unsafe { OwnedFd::from_raw_fd(dup) };
            for (want, filter) in [(libc::POLLIN | libc::POLLRDNORM, libc::EVFILT_READ), (libc::POLLOUT | libc::POLLWRNORM, libc::EVFILT_WRITE)] {
                if events & want != 0 && change(kq, kev(dup.as_raw_fd() as usize, filter, libc::EV_ADD | libc::EV_ONESHOT, 0, 0, id)).is_err() {
                    failed = true;
                }
            }
            dups.push(dup);
        }
        if failed {
            // A file the host's kqueue won't take (the thread's own kqueue
            // took it, so rarely): a worker makes the call.
            drop(shard);
            drop(dups);
            match start {
                Start::Retry { ctx, nr, args, .. } => self.push(Ready::Offload(Job { task, ctx, nr, args })),
                start => self.requeue(task, start),
            }
            return;
        }
        let timer = match deadline {
            Some(d) => {
                let ns = d.saturating_duration_since(Instant::now()).as_nanos().clamp(1, isize::MAX as u128) as isize;
                change(kq, kev(id as usize, libc::EVFILT_TIMER, libc::EV_ADD | libc::EV_ONESHOT, libc::NOTE_NSECONDS | libc::NOTE_CRITICAL, ns, id)).is_ok()
            }
            None => false,
        };
        shard.insert(id, Parked { task, start, dups, timer });
    }

    /// The task parked as `id` runs again, if it's still parked (false if
    /// not: someone else took it, or it isn't parked).
    pub fn unpark(&self, id: u64) -> bool {
        let Some(parked) = self.shard(id).remove(&id) else { return false };
        self.resume(id, parked);
        true
    }

    /// [`Parking::unpark`], or else `trigger` (the thread's waker event),
    /// under the shard's lock: see [`Parking::adopt`].
    fn unpark_or(&self, id: u64, trigger: &dyn Fn()) {
        let parked = {
            let mut shard = self.shard(id);
            let parked = shard.remove(&id);
            if parked.is_none() {
                trigger();
            }
            parked
        };
        if let Some(parked) = parked {
            self.resume(id, parked);
        }
    }

    fn resume(&self, id: u64, parked: Parked) {
        let Parked { task, start, dups, timer } = parked;
        // Its files' registrations go with their dups; its timer, if it
        // hasn't fired, now (it would wake a later park for nothing).
        drop(dups);
        if timer {
            let _ = change(self.kq.as_raw_fd(), kev(id as usize, libc::EVFILT_TIMER, libc::EV_DELETE, 0, 0, 0));
        }
        self.requeue(task, start);
    }

    fn requeue(&self, task: Task, start: Start) {
        match self.sched.add(task, start) {
            Placed::Queued => {}
            Placed::NewCpu(task, start) => self.push(Ready::NewCpu(task, start)),
            Placed::Dropped(task) => self.push(Ready::Gone(task)),
        }
    }

    fn push(&self, ready: Ready) {
        self.ready.lock().unwrap_or_else(|e| e.into_inner()).push(ready);
        self.any_ready.store(true, Ordering::Release);
        trigger(self.kq.as_raw_fd(), PLACE);
    }

    /// Tasks let run again (a SIGCONT, a vfork child's exec), for whichever
    /// thread of the host next looks ([`Parking::take_ready`]).
    pub fn make_ready(&self, tasks: Vec<(Task, Start)>) {
        if tasks.is_empty() {
            return;
        }
        self.ready.lock().unwrap_or_else(|e| e.into_inner()).extend(tasks.into_iter().map(|(t, s)| Ready::Place(t, s)));
        self.any_ready.store(true, Ordering::Release);
        trigger(self.kq.as_raw_fd(), PLACE);
    }

    /// What's been let run again since the last look.
    pub fn take_ready(&self) -> Vec<Ready> {
        if !self.any_ready.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        std::mem::take(&mut *self.ready.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Every task still parked, taken out (the host is ending).
    pub fn take_all(&self) -> Vec<Task> {
        let mut all = Vec::new();
        for shard in &self.shards {
            all.extend(shard.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, p)| p.task));
        }
        all
    }

    /// Wait for events in the host's kqueue: the ids of tasks to make
    /// runnable, whether the host is ending, whether a host signal (or
    /// SIGCHLD) came.
    fn wait(&self, out: &mut [libc::kevent]) -> (Vec<u64>, bool, bool) {
        // Asleep in `poll` on the kqueue, then its events taken: a thread
        // asleep in `kevent` itself takes ~8 µs longer to wake (measured,
        // wakebench), every time.
        let mut pf = libc::pollfd { fd: self.kq.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pf, 1, -1) };
        let n = kevent_now(self.kq.as_raw_fd(), &[], out);
        let (mut ids, mut stop, mut signal) = (Vec::new(), false, false);
        for ev in &out[..n.max(0) as usize] {
            match (ev.filter, ev.ident) {
                (libc::EVFILT_USER, STOP) => stop = true,
                (libc::EVFILT_USER, HOST_SIGNAL) | (libc::EVFILT_SIGNAL, _) => signal = true,
                (libc::EVFILT_USER, _) => {}
                _ => ids.push(ev.udata as usize as u64),
            }
        }
        (ids, stop, signal)
    }
}

impl Drop for Parking {
    fn drop(&mut self) {
        let _ = HOST_KQ.compare_exchange(self.kq.as_raw_fd(), -1, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Tell the waiters the host is ending (from [`Sched::stop`]).
pub(crate) fn stop(kq: i32) {
    trigger(kq, STOP);
}

/// How many waiter threads a host has (MACWL_WAITERS=n to change).
fn waiters() -> usize {
    std::env::var("MACWL_WAITERS").ok().and_then(|n| n.parse().ok()).unwrap_or(2).clamp(1, 16)
}

/// Start the host's waiter threads: each waits in the host's kqueue, makes
/// tasks runnable as their waits end, places what's ready, and wakes a 1:1
/// process's threads for a host signal, until the host ends.
pub(crate) fn run_waiters<'scope, 'env>(sh: &'scope crate::Host<'env>, scope: &'scope crate::Scope<'scope, 'env>) {
    for _ in 0..waiters() {
        scope.spawn(move || {
            let mut out: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
            loop {
                let (ids, stop, signal) = sh.parking.wait(&mut out);
                for id in ids {
                    sh.parking.unpark(id);
                }
                if signal {
                    let proc = sh.parking.host_signals_for.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(Weak::upgrade);
                    if let Some(proc) = proc {
                        proc.abi.wake_all_threads();
                    }
                }
                crate::place_ready(sh, scope);
                if stop {
                    return;
                }
            }
        });
    }
}
