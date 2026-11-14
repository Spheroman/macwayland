//! The host threads a macwl-run process may have, counted and capped.
//!
//! macOS lets a process have up to `kern.num_taskthreads` (12288) threads,
//! far more than the runtime should ever need: a thread per blocked guest
//! call or per timer would let a guest make host threads at will, and each
//! costs memory and makes signalling the process slower. So the runtime
//! keeps far below that limit, two ways:
//!
//! - **A budget.** Every thread the runtime starts takes a [`HostThread`]
//!   from it ([`reserve`], or [`spawn`] and [`spawn_scoped`], which do it
//!   for you) and gives it back when it ends. When none is left, starting one
//!   fails, and the caller does without: a call waits for a worker that's
//!   busy, say.
//! - **A watchdog.** [`check`] counts the threads the process really has,
//!   whoever made them, and ends the process if there are more than the
//!   ceiling. The scheduler's ticker calls it every few milliseconds.
//!
//! `MACWL_MAX_HOST_THREADS=n` lowers the budget (tests); the ceiling is half
//! as much again. Neither can be raised past the defaults.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The most host threads the runtime starts in one process.
pub const MAX: usize = 1024;

/// The least the budget can be lowered to: a host's own threads (up to 18
/// CPUs on this Mac, waiters, the ticker, a few more) always fit.
pub const MIN: usize = 32;

static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The budget: [`MAX`], or less with MACWL_MAX_HOST_THREADS.
pub fn budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("MACWL_MAX_HOST_THREADS").ok().and_then(|n| n.parse().ok()).map_or(MAX, |n: usize| n.clamp(MIN, MAX))
    })
}

/// The most threads the process may really have before [`check`] ends it:
/// the budget, half as much again for what the runtime doesn't start itself
/// (the main thread, Rust's and the system's own), and never more than
/// 1536.
pub fn ceiling() -> usize {
    budget() + budget() / 2
}

/// One of the budget's threads, given back when dropped (when the thread
/// it was taken for ends: move it in).
#[derive(Debug)]
pub struct HostThread(());

impl Drop for HostThread {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Take a thread from the budget, or None if it's spent.
pub fn reserve() -> Option<HostThread> {
    let budget = budget();
    LIVE.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < budget).then_some(n + 1)).ok().map(|_| HostThread(()))
}

/// How many of the budget's threads are taken.
pub fn live() -> usize {
    LIVE.load(Ordering::Acquire)
}

/// Start a thread on the budget. Fails with `WouldBlock` if it's spent.
pub fn spawn<F, T>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    check();
    let held = reserve().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::WouldBlock, "host thread budget spent"))?;
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let _held = held;
        f()
    })
}

/// [`spawn`] in a scope.
pub fn spawn_scoped<'scope, 'env, F, T>(scope: &'scope std::thread::Scope<'scope, 'env>, name: &str, f: F) -> std::io::Result<std::thread::ScopedJoinHandle<'scope, T>>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    check();
    let held = reserve().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::WouldBlock, "host thread budget spent"))?;
    std::thread::Builder::new().name(name.into()).spawn_scoped(scope, move || {
        let _held = held;
        f()
    })
}

/// How many threads this process really has, from the kernel.
pub fn actual() -> Option<usize> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let got = unsafe { libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    (got == size).then_some(info.pti_threadnum as usize)
}

/// The process's thread count, if it's over `ceiling`.
pub fn over(ceiling: usize) -> Option<usize> {
    actual().filter(|&n| n > ceiling)
}

/// The watchdog: end the process at once if it has more threads than the
/// [`ceiling`]. Cheap enough (one `proc_pidinfo`) to call every tick.
pub fn check() {
    if let Some(n) = over(ceiling()) {
        // Not a Linux error the guest could handle: the runtime has lost
        // track of its threads, and stopping beats carrying on without a
        // bound.
        eprintln!("macwl-run: {n} host threads, over the ceiling of {}; stopping", ceiling());
        unsafe { libc::_exit(70) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is the process's: tests that use it take turns.
    static BUDGET_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn counts_this_process() {
        let _turn = BUDGET_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let n = actual().expect("proc_pidinfo");
        assert!(n >= 1);
        let held: Vec<_> = (0..4).map(|_| spawn("t", || std::thread::sleep(std::time::Duration::from_millis(200))).unwrap()).collect();
        assert!(actual().unwrap() >= n + 4 - 1, "four more threads are seen");
        for h in held {
            h.join().unwrap();
        }
    }

    #[test]
    fn watchdog_sees_threads_made_outside_the_budget() {
        let before = actual().unwrap();
        let stop = std::sync::Arc::new(std::sync::Barrier::new(21));
        let plain: Vec<_> = (0..20)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    stop.wait();
                })
            })
            .collect();
        assert!(over(before + 10).is_some(), "20 plain threads put it over a ceiling 10 above");
        assert!(over(before + 1000).is_none());
        stop.wait();
        for t in plain {
            t.join().unwrap();
        }
    }

    #[test]
    fn reserve_gives_back() {
        let _turn = BUDGET_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let before = live();
        let a = reserve().unwrap();
        assert_eq!(live(), before + 1);
        drop(a);
        assert_eq!(live(), before);
    }
}
