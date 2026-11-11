//! A process's CPU time, user and system apart.
//!
//! User time is the time its threads spend in the guest: each run of a vCPU
//! for it is timed (two reads of the uptime clock, ~20 ns each), and a run
//! in progress counts as it goes, so a thread spinning without a system
//! call still shows (`CLOCK_PROCESS_CPUTIME_ID`, ITIMER_VIRTUAL). System
//! time is the rest of the CPU time its tasks take on a CPU (the host
//! thread's CPU time, which counts its vCPU's runs too, less those runs),
//! taken stock of at most every millisecond as the vCPU stops (reading a
//! thread's CPU time is a Mach call, too dear for every system call) and
//! at the end of a task's turn, and all of a worker's making its calls:
//! the runtime is its kernel.

use std::sync::atomic::{AtomicU64, Ordering};

/// The most CPUs a host has (one run in progress per CPU).
const MAX_CPUS: usize = 64;

/// User and system time, in ns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Times {
    pub user: u64,
    pub sys: u64,
}

impl Times {
    pub fn total(self) -> u64 {
        self.user + self.sys
    }
}

impl std::ops::Add for Times {
    type Output = Times;
    fn add(self, o: Times) -> Times {
        Times { user: self.user + o.user, sys: self.sys + o.sys }
    }
}

impl std::ops::AddAssign for Times {
    fn add_assign(&mut self, o: Times) {
        *self = *self + o;
    }
}

/// The uptime clock, in ns: cheap (no system call), and what a vCPU's runs
/// are timed with.
pub(crate) fn now_ns() -> u64 {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    }
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) }
}

/// The CPU time the calling host thread has used, in ns (a CPU's: its
/// vCPU's runs included).
pub(crate) fn thread_cpu_ns() -> u64 {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    }
    unsafe { clock_gettime_nsec_np(libc::CLOCK_THREAD_CPUTIME_ID) }
}

/// A CPU's stock of its current task's turn: when it was last taken (uptime
/// and the thread's CPU time) and the user time run since.
#[derive(Clone, Copy, Default)]
struct Stock {
    at: u64,
    cpu: u64,
    user: u64,
}

thread_local! {
    static STOCK: std::cell::Cell<Stock> = const { std::cell::Cell::new(Stock { at: 0, cpu: 0, user: 0 }) };
}

/// How often a CPU takes stock of system time while a task runs on.
const STOCK_EVERY: u64 = 1_000_000;

/// A task's turn on this CPU starts.
pub(crate) fn turn_starts() {
    STOCK.with(|s| s.set(Stock { at: now_ns(), cpu: thread_cpu_ns(), user: 0 }));
}

/// Count `cpu`'s system time since the last stock: at the end of a turn
/// (`end`), or as the vCPU stops if a millisecond has passed.
pub(crate) fn take_stock(cpu: &CpuTime, end: bool) {
    take_stock_at(cpu, end, now_ns());
}

fn take_stock_at(cpu: &CpuTime, end: bool, now: u64) {
    STOCK.with(|s| {
        let st = s.get();
        if !end && now.saturating_sub(st.at) < STOCK_EVERY {
            return;
        }
        let used = thread_cpu_ns();
        cpu.add_sys(used.saturating_sub(st.cpu).saturating_sub(st.user));
        s.set(Stock { at: now, cpu: used, user: 0 });
    });
}

/// A process's CPU time.
#[derive(Debug)]
pub(crate) struct CpuTime {
    user: AtomicU64,
    sys: AtomicU64,
    /// When the run in progress on each CPU (by slot) started for this
    /// process, or 0.
    running_since: [AtomicU64; MAX_CPUS],
    /// The most user time read so far: what's read never goes back (a read
    /// racing a run's end could count it twice, then once).
    read_user: AtomicU64,
}

impl Default for CpuTime {
    fn default() -> Self {
        CpuTime { user: AtomicU64::new(0), sys: AtomicU64::new(0), running_since: [const { AtomicU64::new(0) }; MAX_CPUS], read_user: AtomicU64::new(0) }
    }
}

impl CpuTime {
    /// A thread of the process starts running in the guest on the CPU in
    /// `slot`; returns the time, for [`CpuTime::left_guest`].
    pub fn entering_guest(&self, slot: usize) -> u64 {
        let t = now_ns();
        self.running_since[slot].store(t, Ordering::Release);
        t
    }

    /// That run, begun at `since`, is over.
    pub fn left_guest(&self, slot: usize, since: u64) {
        let now = now_ns();
        let ran = now.saturating_sub(since);
        self.user.fetch_add(ran, Ordering::AcqRel);
        self.running_since[slot].store(0, Ordering::Release);
        STOCK.with(|s| {
            let mut st = s.get();
            st.user += ran;
            s.set(st);
        });
        take_stock_at(self, false, now);
    }

    /// System time: work the runtime did for the process.
    pub fn add_sys(&self, ns: u64) {
        self.sys.fetch_add(ns, Ordering::Relaxed);
    }

    /// Whether one of its threads is running in the guest now.
    pub fn in_guest(&self) -> bool {
        self.running_since.iter().any(|s| s.load(Ordering::Relaxed) != 0)
    }

    /// Its user and system time now, runs in progress included.
    pub fn times(&self) -> Times {
        let now = now_ns();
        let running: u64 = self.running_since.iter().map(|s| s.load(Ordering::Acquire)).filter(|&s| s != 0).map(|s| now.saturating_sub(s)).sum();
        let user = self.user.load(Ordering::Acquire) + running;
        let user = self.read_user.fetch_max(user, Ordering::AcqRel).max(user);
        Times { user, sys: self.sys.load(Ordering::Relaxed) }
    }
}
