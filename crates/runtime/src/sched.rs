//! Guest threads as tasks on CPUs (see `docs/mn-threads.md`).
//!
//! A [`Cpu`] is a vCPU and the host thread that owns it (Hypervisor.framework
//! ties a vCPU to the thread that made it); a [`Task`] is a guest thread.
//! A host (a VM: one Linux process in 1:1 mode, many by default,
//! docs/processes.md) has a CPU per physical core at most, shared by the
//! tasks of all its processes, made as tasks need them and kept until it
//! ends: each takes the next task from the run queue
//! ([`Sched`]) when its own exits, is preempted, or blocks. Tasks beyond the
//! CPUs wait their turn: a ticker preempts a task that has run a whole slice
//! while others wait (round robin, for now).
//!
//! A system call that would block, while another task waits for a CPU,
//! **parks** its task (see `park`: it waits in a kqueue, or in memory, with
//! no host thread), and the CPU takes the next task; when the wait is over,
//! the task waits for a CPU again and makes the call there. A task that was
//! already blocked in a call on its CPU when others came to wait is
//! **evicted**: its call is interrupted, and parks. Calls with nothing a
//! kqueue can watch (`flock`, a FIFO's `open`) go to a **worker** (a host
//! thread without a vCPU) instead, from a bounded pool.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use linux_abi::syscall::Thread;

use crate::{
    hv::{UserContext, Vcpu, Vm},
    signal, Proc,
};

/// How a task starts (or carries on) on a CPU.
pub(crate) enum Start {
    /// A freshly loaded program: at its entry point, with this stack.
    Fresh { entry: u64, sp: u64 },
    /// From these registers: a new thread (as `clone` made it), a fork
    /// child, a preempted task.
    Resume(Box<UserContext>),
    /// Back from a system call a worker made for it: these registers (as
    /// the call left them, at the instruction after the `svc`), and the
    /// call's result.
    AfterCall { ctx: Box<UserContext>, nr: u64, arg0: u64, ret: i64 },
    /// Back from a parked wait: these registers (at the instruction after
    /// the `svc`), and the call to make again, with what its wait had.
    Retry { ctx: Box<UserContext>, nr: u64, args: [u64; 6], park: linux_abi::syscall::Park },
}

/// A system call for a worker to make, for a task that gave up its CPU.
pub(crate) struct Job {
    pub task: Task,
    pub ctx: Box<UserContext>,
    pub nr: u64,
    pub args: [u64; 6],
}

/// How long a task runs before one that's waiting gets the CPU.
const SLICE: Duration = Duration::from_millis(10);

/// The most CPUs Hypervisor.framework allows a process (and the size of the
/// host signal tables).
const MAX_CPUS: usize = 64;

/// A CPU's task has used its slice: switch at the next chance (the ticker
/// sets it, then kicks the vCPU out). Indexed by signal slot.
static PREEMPT: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Whether the task on the CPU in `slot` is to give the CPU up (and clear
/// the request).
pub(crate) fn take_preempt(slot: usize) -> bool {
    PREEMPT[slot].swap(false, Ordering::AcqRel)
}

/// The CPU's task is blocked in a system call on it, one a worker could make
/// instead (so the CPU can be taken back). Indexed by signal slot.
static INLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// The CPU's task is to give it up: its call was interrupted for that.
static EVICT: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// The task on the CPU in `slot` starts (true) or ends a system call that
/// may block on it.
pub(crate) fn set_inline(slot: usize, on: bool) {
    if on {
        EVICT[slot].store(false, Ordering::Release);
    }
    // SeqCst, with the queue's: a CPU marks itself and then looks for tasks
    // waiting, the queue takes a task and then looks for CPUs to take back;
    // one of the two sees the other.
    INLINE[slot].store(on, Ordering::SeqCst);
}

/// Whether the CPU in `slot` was taken back from its task's call (and clear
/// that).
pub(crate) fn take_evict(slot: usize) -> bool {
    EVICT[slot].swap(false, Ordering::AcqRel)
}

thread_local! {
    /// The signal slot of the CPU this host thread is (None: not a CPU).
    static THIS_CPU: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The run queue of this CPU's host.
    static THIS_SCHED: std::cell::RefCell<Option<Arc<Sched>>> = const { std::cell::RefCell::new(None) };
}

/// For `linux_abi`'s blocking calls: whether the call on this host thread
/// should leave its CPU rather than sleep on it (park): it's a CPU, and
/// tasks wait for one, or this one is being taken back.
pub(crate) fn should_yield() -> bool {
    let Some(slot) = THIS_CPU.with(|cpu| cpu.get()) else { return false };
    EVICT[slot].load(Ordering::Acquire) || THIS_SCHED.with(|s| s.borrow().as_ref().is_some_and(|s| s.others_waiting()))
}

/// The most workers a host has: they're for calls with nothing a kqueue can
/// watch, which are few; more wait their turn.
const MAX_WORKERS: usize = 256;

/// Host threads kept for the host's own besides its CPUs and waiters, out
/// of the budget workers may use. There are eight kinds today (the ticker,
/// timers, reclaiming, host signals, the parent's requests, receipts, and
/// the relay's two), each one at a time: twice that, so one starting as
/// another ends, or a kind added later, never finds the budget spent by
/// idle workers.
const RESERVED: usize = 16;

/// The most workers a host may have: [`MAX_WORKERS`], or less if the host
/// thread budget (linux_abi::host_threads) is lower, so that what workers
/// take never leaves a CPU or the host's own threads without one. (Idle
/// workers stay, holding theirs.) At least two, so blocked calls always
/// move.
fn max_workers(max_cpus: usize) -> usize {
    let own = max_cpus + crate::park::waiters() + RESERVED;
    linux_abi::host_threads::budget().saturating_sub(own).clamp(2, MAX_WORKERS)
}

/// How often an idle ticker still checks the process's thread count.
const WATCHDOG: Duration = Duration::from_millis(100);

/// How many CPUs a process gets: one per physical core (at most 64, the
/// most Hypervisor.framework allows); MACWL_CPUS=n for fewer (tests).
fn max_cpus() -> usize {
    let cores = {
        let mut n: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let ok = unsafe { libc::sysctlbyname(c"hw.physicalcpu".as_ptr(), (&raw mut n).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
        if ok && n > 0 { n as usize } else { MAX_CPUS }
    };
    std::env::var("MACWL_CPUS").ok().and_then(|n| n.parse().ok()).unwrap_or(cores).clamp(1, MAX_CPUS)
}

/// A host's run queue and CPUs, for the tasks of every process it runs.
pub(crate) struct Sched {
    state: Mutex<State>,
    /// Idle CPUs wait on it for a task; the ticker, for work to watch.
    changed: Condvar,
    /// Idle workers wait on it for a job.
    jobs_changed: Condvar,
    /// Tasks waiting for a CPU (`ready.len()`, readable without the lock:
    /// each system call that may block asks).
    waiting: AtomicUsize,
    /// The host's parking kqueue, told when the host ends (-1: none).
    stop_kq: AtomicI32,
}

struct State {
    ready: VecDeque<(Task, Start)>,
    /// Calls waiting for a worker, and the workers there are and idle.
    jobs: VecDeque<Job>,
    workers: usize,
    idle_workers: usize,
    /// CPUs started and not yet gone.
    cpus: usize,
    /// Of those, waiting for a task.
    idle: usize,
    max_cpus: usize,
    /// See [`max_workers`].
    max_workers: usize,
    /// The process is ending: CPUs leave once their task is done.
    stopping: bool,
    /// When each CPU's task started its slice (by signal slot).
    since: [Option<Instant>; MAX_CPUS],
}

/// Where [`Sched::offload`] put a call.
pub(crate) enum Offloaded {
    /// For an idle worker to take.
    Queued,
    /// It needs a new worker: start one with it.
    NewWorker(Job),
    /// The process is ending: the task won't run again.
    Dropped(Task),
}

/// Where [`Sched::add`] put a task.
pub(crate) enum Placed {
    /// On the run queue, for a CPU to take.
    Queued,
    /// It needs a new CPU: start one with it.
    NewCpu(Task, Start),
    /// The process is ending: it won't run.
    Dropped(Task),
}

impl Sched {
    /// A run queue for a process whose first task runs on the calling
    /// thread's CPU.
    pub fn new() -> Self {
        Sched {
            state: Mutex::new(State {
                ready: VecDeque::new(),
                jobs: VecDeque::new(),
                workers: 0,
                idle_workers: 0,
                cpus: 1,
                idle: 0,
                max_cpus: max_cpus(),
                max_workers: max_workers(max_cpus()),
                stopping: false,
                since: [None; MAX_CPUS],
            }),
            changed: Condvar::new(),
            jobs_changed: Condvar::new(),
            waiting: AtomicUsize::new(0),
            stop_kq: AtomicI32::new(-1),
        }
    }

    /// Tell `kq`'s waiters when the host ends.
    pub fn set_stop_kq(&self, kq: i32) {
        self.stop_kq.store(kq, Ordering::Release);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a task is waiting for a CPU (so a call that would block
    /// should give this one up).
    pub fn others_waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst) > 0
    }

    /// `task` goes on the run queue. With no CPU to take it, one is taken
    /// back from a task blocked in a call on it.
    fn enqueue(&self, s: &mut State, task: Task, start: Start) {
        s.ready.push_back((task, start));
        self.waiting.store(s.ready.len(), Ordering::SeqCst);
        self.changed.notify_all();
        Self::evict(s);
    }

    /// With tasks waiting and no CPU free, take one back from a task
    /// blocked in a call on it: interrupt the call (again, if it was taken
    /// back already: the interrupt can land just before a call sleeps, and
    /// be lost).
    fn evict(s: &State) {
        if s.ready.is_empty() || s.idle > 0 || s.cpus < s.max_cpus {
            return;
        }
        let blocked = (0..MAX_CPUS).filter(|&slot| s.since[slot].is_some() && INLINE[slot].load(Ordering::SeqCst));
        let mut wanted = s.ready.len();
        for slot in blocked {
            if wanted == 0 {
                break;
            }
            EVICT[slot].store(true, Ordering::Release);
            signal::interrupt_slot(slot);
            wanted -= 1;
        }
    }

    fn dequeue(&self, s: &mut State) -> Option<(Task, Start)> {
        let next = s.ready.pop_front();
        self.waiting.store(s.ready.len(), Ordering::SeqCst);
        next
    }

    /// A task wants a CPU (new, preempted, or back from a worker): an idle
    /// one takes it, else a new CPU if there's room for one, else it waits
    /// its turn. Idle CPUs count only for as many tasks as they can take:
    /// two tasks added before an idle CPU wakes need two CPUs.
    pub fn add(&self, task: Task, start: Start) -> Placed {
        let mut s = self.lock();
        if s.stopping {
            return Placed::Dropped(task);
        }
        if s.ready.len() >= s.idle && s.cpus < s.max_cpus {
            s.cpus += 1;
            return Placed::NewCpu(task, start);
        }
        self.enqueue(&mut s, task, start);
        Placed::Queued
    }

    /// A CPU couldn't be made (no vCPU to be had): its task waits, and no
    /// more CPUs are tried.
    pub fn no_cpu(&self, task: Task, start: Start) {
        let mut s = self.lock();
        s.cpus -= 1;
        s.max_cpus = s.cpus.max(1);
        self.enqueue(&mut s, task, start);
    }

    /// The next task for the CPU in `slot`, waiting for one; None once the
    /// process is ending.
    pub fn next(&self, slot: usize) -> Option<(Task, Start)> {
        let mut s = self.lock();
        loop {
            if s.stopping {
                return None;
            }
            if let Some(next) = self.dequeue(&mut s) {
                s.since[slot] = Some(Instant::now());
                return Some(next);
            }
            s.idle += 1;
            s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
            s.idle -= 1;
        }
    }

    /// The CPU in `slot` starts running a task it already has (the first).
    pub fn running(&self, slot: usize) {
        self.lock().since[slot] = Some(Instant::now());
    }

    /// The CPU in `slot` is done with its task.
    pub fn done(&self, slot: usize) {
        self.lock().since[slot] = None;
        PREEMPT[slot].store(false, Ordering::Release);
    }

    /// A CPU leaves (the process is ending).
    pub fn cpu_gone(&self) {
        self.lock().cpus -= 1;
    }

    /// The process is ending: CPUs and workers leave, the ticker stops.
    /// Returns the tasks that were waiting (their threads end).
    pub fn stop(&self) -> Vec<Task> {
        let mut s = self.lock();
        s.stopping = true;
        self.changed.notify_all();
        self.jobs_changed.notify_all();
        let kq = self.stop_kq.load(Ordering::Acquire);
        if kq >= 0 {
            crate::park::stop(kq);
        }
        let mut tasks: Vec<Task> = s.ready.drain(..).map(|(t, _)| t).collect();
        tasks.extend(s.jobs.drain(..).map(|j| j.task));
        self.waiting.store(0, Ordering::Release);
        tasks
    }

    /// The waiting tasks of process `pid`, taken off the queue (`execve`
    /// and `exit_group` end them).
    pub fn drain(&self, pid: i64) -> Vec<Task> {
        let mut s = self.lock();
        let (mut tasks, mut kept) = (Vec::new(), VecDeque::new());
        for (task, start) in s.ready.drain(..) {
            if task.proc.abi.pid == pid {
                tasks.push(task);
            } else {
                kept.push_back((task, start));
            }
        }
        s.ready = kept;
        self.waiting.store(s.ready.len(), Ordering::SeqCst);
        let (mine, others): (VecDeque<Job>, VecDeque<Job>) = s.jobs.drain(..).partition(|j| j.task.proc.abi.pid == pid);
        s.jobs = others;
        tasks.extend(mine.into_iter().map(|j| j.task));
        tasks
    }

    /// A call for a worker: an idle one takes it, else a new worker must be
    /// started with it.
    pub fn offload(&self, job: Job) -> Offloaded {
        let mut s = self.lock();
        if s.stopping {
            return Offloaded::Dropped(job.task);
        }
        // As for CPUs: an idle worker counts once, however many jobs come
        // before it wakes. (Left queued behind a job that blocks for good,
        // one would never be made.)
        if s.jobs.len() >= s.idle_workers && s.workers < s.max_workers {
            s.workers += 1;
            return Offloaded::NewWorker(job);
        }
        s.jobs.push_back(job);
        self.jobs_changed.notify_one();
        Offloaded::Queued
    }

    /// A worker [`offload`](Self::offload) asked for couldn't be started
    /// (no host thread left in the budget): its job waits for one that is.
    pub fn worker_unstarted(&self, job: Job) {
        let mut s = self.lock();
        s.workers -= 1;
        s.jobs.push_back(job);
        self.jobs_changed.notify_one();
    }

    /// The next call for a worker, waiting for one; None once the process
    /// is ending.
    pub fn next_job(&self) -> Option<Job> {
        let mut s = self.lock();
        loop {
            if s.stopping {
                s.workers -= 1;
                return None;
            }
            if let Some(job) = s.jobs.pop_front() {
                return Some(job);
            }
            s.idle_workers += 1;
            s = self.jobs_changed.wait(s).unwrap_or_else(|e| e.into_inner());
            s.idle_workers -= 1;
        }
    }

    /// Preempt tasks that have run a whole slice while others wait, until
    /// the process ends. Run on a host thread of its own.
    pub fn ticker(&self) {
        let mut s = self.lock();
        while !s.stopping {
            // The watchdog on the process's real thread count, whoever made
            // them (linux_abi::host_threads).
            linux_abi::host_threads::check();
            if s.ready.is_empty() {
                // Nothing waits: nothing to preempt for. (Awake now and then
                // for the watchdog.)
                s = self.changed.wait_timeout(s, WATCHDOG).unwrap_or_else(|e| e.into_inner()).0;
                continue;
            }
            let now = Instant::now();
            for (slot, since) in s.since.iter().enumerate() {
                // In the guest, past its slice: preempted. (In a call, the
                // kick does nothing until it's back.)
                if since.is_some_and(|t| now.duration_since(t) >= SLICE) && !INLINE[slot].load(Ordering::Acquire) {
                    PREEMPT[slot].store(true, Ordering::Release);
                    signal::exit_slot(slot);
                }
            }
            // Blocked in a call while others wait: taken back (again).
            Self::evict(&s);
            s = self.changed.wait_timeout(s, SLICE / 2).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// A vCPU, run by the host thread that made it, with its slot in the tables
/// host signal handlers use to reach it ([`signal::add_vcpu`]).
pub(crate) struct Cpu<'vm> {
    pub vcpu: Vcpu<'vm>,
    pub slot: usize,
}

impl<'vm> Cpu<'vm> {
    /// A CPU on this host thread, or why there's none: no vCPU to be had
    /// (Hypervisor.framework allows 64 a process), or no slot left.
    pub fn new(vm: &'vm Vm, sched: &Arc<Sched>) -> Result<Self, crate::Error> {
        let vcpu = Vcpu::new(vm)?;
        let slot = signal::add_vcpu(vcpu.id()).ok_or_else(|| crate::Error::Unexpected("no vCPU slot left".into()))?;
        THIS_CPU.with(|cpu| cpu.set(Some(slot)));
        THIS_SCHED.with(|s| *s.borrow_mut() = Some(sched.clone()));
        linux_abi::syscall::set_yield_check(should_yield);
        Ok(Cpu { vcpu, slot })
    }
}

impl Drop for Cpu<'_> {
    fn drop(&mut self) {
        // Out of the tables before the vCPU goes (it's dropped after this).
        THIS_CPU.with(|cpu| cpu.set(None));
        THIS_SCHED.with(|s| *s.borrow_mut() = None);
        signal::remove_vcpu(self.slot);
    }
}

/// Where a task is, for whoever needs its attention (a signal for it, the
/// process ending): an atomic, as wakers run on other threads.
#[derive(Debug)]
pub(crate) struct Location {
    at: AtomicU64,
    /// Its id while it's parked (see `park`).
    pub id: u64,
}

/// Nowhere to reach: not running, not in a host call.
const NOWHERE: u64 = u64::MAX;
/// In a call on a worker: the rest is the worker's host thread (a
/// `pthread_t`, a pointer, well below this bit).
const WORKER: u64 = 1 << 62;

impl Location {
    /// In a system call on this (worker) host thread.
    pub fn set_worker(&self) {
        self.at.store(WORKER | unsafe { libc::pthread_self() } as u64, Ordering::Release);
    }

    /// Running on the CPU in signal slot `slot` (in the guest, or in a
    /// system call on that CPU's host thread).
    pub fn set_running(&self, slot: usize) {
        self.at.store(slot as u64, Ordering::Release);
    }

    pub fn set_nowhere(&self) {
        self.at.store(NOWHERE, Ordering::Release);
    }

    /// Get the task's attention: out of the guest, and out of whatever
    /// host call it's blocked in.
    pub fn wake(&self) {
        match self.at.load(Ordering::Acquire) {
            NOWHERE => {}
            worker if worker & WORKER != 0 => unsafe {
                libc::pthread_kill((worker & !WORKER) as libc::pthread_t, signal::INTERRUPT);
            },
            slot => signal::wake_slot(slot as usize),
        }
    }

    /// How other threads get the task's attention, wherever it is.
    pub fn waker(self: &Arc<Self>) -> Box<dyn Fn() + Send + Sync> {
        let location = self.clone();
        Box::new(move || location.wake())
    }
}

/// A guest thread, its process, and where it is.
pub(crate) struct Task {
    pub thread: Thread,
    pub proc: Arc<Proc>,
    pub location: Arc<Location>,
    /// The CPU (signal slot) it last ran on: if that CPU hasn't run another
    /// task since, its vCPU still has this one's registers.
    pub last_cpu: Option<usize>,
}

impl Task {
    pub fn new(thread: Thread, proc: Arc<Proc>) -> Self {
        Task { thread, proc, location: Arc::new(Location { at: AtomicU64::new(NOWHERE), id: crate::park::new_id() }), last_cpu: None }
    }
}
