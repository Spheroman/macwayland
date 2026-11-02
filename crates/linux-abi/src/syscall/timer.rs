//! POSIX timers (`timer_create` and friends) and interval timers
//! (`setitimer`): a signal when a timer expires.
//!
//! macOS has no per-process timers like Linux's, so each armed timer is a
//! host thread that sleeps until its deadline. On expiry it queues the
//! signal (with Linux's timer `siginfo`: id, overrun, `sigev_value`) for the
//! process or one thread, in the process's [`Inbox`], then gets the
//! attention of that process's threads (and no other's): kicking their vCPUs
//! out of the guest, waking them asleep in the runtime, and interrupting
//! their blocking host calls.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use super::{
    kernel::CpuClock,
    misc,
    signal::{self, Inbox},
    Process, Thread,
};
use crate::{
    abi::Timespec,
    errno,
    memory::{GuestMemory, GuestMemoryExt},
};

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD_ID: i32 = 4;
const TIMER_ABSTIME: u64 = 1;
const SIGALRM: u64 = 14;
/// `si_code` for timer signals.
pub const SI_TIMER: i32 = -2;

#[derive(Debug)]
struct State {
    deadline: Option<Instant>,
    interval: Duration,
    /// Expiries since the signal last went out, beyond the first.
    overrun: i32,
    deleted: bool,
    /// A thread is sleeping for this timer.
    running: bool,
}

#[derive(Debug)]
pub(super) struct PosixTimer {
    id: i32,
    clock: u64,
    signo: u64,
    value: u64,
    notify: i32,
    tid: Option<i64>,
    state: Mutex<State>,
    wake: Condvar,
    /// Where its signals go: its process's.
    inbox: Arc<Inbox>,
}

/// A process's timers, by id.
#[derive(Debug, Default)]
pub(super) struct Timers {
    next: i32,
    timers: BTreeMap<i32, Arc<PosixTimer>>,
}

fn timers(p: &Process) -> std::sync::MutexGuard<'_, Timers> {
    p.timers.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn timer_create<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, clock: u64, sevp: u64, id_out: u64) -> Result<i64, i64> {
    misc::clock_now(clock)?;
    let mut ts = timers(p);
    let id = ts.next;
    let (value, signo, notify, tid) = if sevp == 0 {
        (id as u64, SIGALRM, SIGEV_SIGNAL, None)
    } else {
        let value: u64 = mem.read_pod(sevp)?;
        let signo: i32 = mem.read_pod(sevp + 8)?;
        let notify: i32 = mem.read_pod(sevp + 12)?;
        let tid: i32 = mem.read_pod(sevp + 16)?;
        match notify {
            SIGEV_NONE => (value, 0, notify, None),
            SIGEV_SIGNAL | SIGEV_THREAD_ID if (1..=64).contains(&signo) => {
                (value, signo as u64, notify, (notify == SIGEV_THREAD_ID).then_some(if tid == 0 { t.tid } else { tid as i64 }))
            }
            _ => return Err(errno::EINVAL),
        }
    };
    mem.write_pod(id_out, &id)?;
    ts.next += 1;
    let state = State { deadline: None, interval: Duration::ZERO, overrun: 0, deleted: false, running: false };
    let timer = PosixTimer { id, clock, signo, value, notify, tid, state: Mutex::new(state), wake: Condvar::new(), inbox: p.inbox.clone() };
    ts.timers.insert(id, Arc::new(timer));
    Ok(0)
}

fn get(p: &Process, id: u64) -> Result<Arc<PosixTimer>, i64> {
    timers(p).timers.get(&(id as i32)).cloned().ok_or(errno::EINVAL)
}

fn current(s: &State) -> [Timespec; 2] {
    let to_ts = |d: Duration| Timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 };
    let left = s.deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()).max(Duration::from_nanos(1)));
    [to_ts(s.interval), to_ts(left)]
}

pub(super) fn timer_settime<M: GuestMemory + ?Sized>(p: &Process, mem: &M, id: u64, flags: u64, new: u64, old: u64) -> Result<i64, i64> {
    let timer = get(p, id)?;
    let [interval, value]: [Timespec; 2] = mem.read_pod(new)?;
    let dur = |t: Timespec| -> Result<Duration, i64> {
        if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
            return Err(errno::EINVAL);
        }
        Ok(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
    };
    let (interval, value) = (dur(interval)?, dur(value)?);
    let mut s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    if old != 0 {
        mem.write_pod(old, &current(&s))?;
    }
    s.interval = interval;
    s.overrun = 0;
    s.deadline = if value.is_zero() {
        None
    } else if flags & TIMER_ABSTIME != 0 {
        Some(Instant::now() + value.saturating_sub(misc::clock_now(timer.clock)?))
    } else {
        Some(Instant::now() + value)
    };
    if s.deadline.is_some() && !s.running {
        s.running = true;
        let timer = timer.clone();
        std::thread::spawn(move || run(timer));
    }
    timer.wake.notify_all();
    Ok(0)
}

pub(super) fn timer_gettime<M: GuestMemory + ?Sized>(p: &Process, mem: &M, id: u64, out: u64) -> Result<i64, i64> {
    let timer = get(p, id)?;
    let s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    mem.write_pod(out, &current(&s))?;
    Ok(0)
}

pub(super) fn timer_getoverrun(p: &Process, id: u64) -> Result<i64, i64> {
    let timer = get(p, id)?;
    let s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    Ok(s.overrun as i64)
}

pub(super) fn timer_delete(p: &Process, id: u64) -> Result<i64, i64> {
    let timer = timers(p).timers.remove(&(id as i32)).ok_or(errno::EINVAL)?;
    timer.state.lock().unwrap_or_else(|e| e.into_inner()).deleted = true;
    timer.wake.notify_all();
    Ok(0)
}

/// A timer's thread: sleep until it expires, signal, repeat while armed.
fn run(timer: Arc<PosixTimer>) {
    let mut s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if s.deleted {
            s.running = false;
            return;
        }
        let Some(deadline) = s.deadline else {
            s.running = false;
            return;
        };
        let now = Instant::now();
        if now < deadline {
            s = timer.wake.wait_timeout(s, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
            continue;
        }
        // Expired: count the periods missed, re-arm, and signal.
        let mut expiries = 1;
        if s.interval.is_zero() {
            s.deadline = None;
        } else {
            let missed = ((now - deadline).as_nanos() / s.interval.as_nanos()) as u32;
            expiries += missed as i32;
            s.deadline = Some(deadline + s.interval * (missed + 1));
        }
        if timer.notify != SIGEV_NONE {
            let info = signal::SigInfo { signo: timer.signo, code: SI_TIMER, timer: Some((timer.id, expiries - 1, timer.value)), ..Default::default() };
            let mut fired = timer.inbox.fired.lock().unwrap_or_else(|e| e.into_inner());
            // Still pending from last time: count it as an overrun instead.
            if let Some((_, prev)) = fired.iter_mut().find(|(tid, i)| *tid == timer.tid && i.timer.is_some_and(|t| t.0 == timer.id)) {
                if let Some(t) = &mut prev.timer {
                    t.1 = t.1.saturating_add(expiries);
                }
            } else {
                fired.push((timer.tid, info));
            }
            s.overrun = expiries - 1;
            drop(fired);
            drop(s);
            signal::alert(&timer.inbox);
            s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

const SI_KERNEL: i32 = 0x80;

/// A process's interval timers (`setitimer`), by `which`: ITIMER_REAL
/// (wall time, SIGALRM), ITIMER_VIRTUAL (user CPU time, SIGVTALRM) and
/// ITIMER_PROF (user and system CPU time, SIGPROF). As on Linux, they
/// survive `execve`, and a forked child starts without them.
#[derive(Debug, Default)]
pub(super) struct ITimers([Option<Arc<ITimer>>; 3]);

pub(super) struct ITimer {
    signo: u64,
    clock: CpuClock,
    /// Whether the clock is CPU time, which can run faster than wall time
    /// when several threads run.
    cpu: bool,
    state: Mutex<IState>,
    wake: Condvar,
    inbox: Arc<Inbox>,
}

impl std::fmt::Debug for ITimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ITimer").field("signo", &self.signo).finish()
    }
}

#[derive(Debug, Default)]
struct IState {
    /// When it expires next, on its clock.
    deadline: Option<Duration>,
    interval: Duration,
    /// A thread is sleeping for this timer.
    running: bool,
}

/// `setitimer(which, new, old)` (`getitimer` is `new` = 0).
pub(super) fn setitimer<M: GuestMemory + ?Sized>(p: &Process, mem: &M, which: u64, new: u64, old: u64) -> Result<i64, i64> {
    if which > 2 {
        return Err(errno::EINVAL);
    }
    // `struct itimerval`: two timevals of two longs.
    let new = if new != 0 {
        let v: [i64; 4] = mem.read_pod(new)?;
        if v.iter().any(|&x| x < 0) || v[1] >= 1_000_000 || v[3] >= 1_000_000 {
            return Err(errno::EINVAL);
        }
        let tv = |s: i64, us: i64| Duration::new(s as u64, us as u32 * 1000);
        Some((tv(v[0], v[1]), tv(v[2], v[3])))
    } else {
        None
    };
    let timer = itimer(p, which as usize);
    let mut s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    let now = (timer.clock)();
    let left = s.deadline.map_or(Duration::ZERO, |d| d.saturating_sub(now).max(Duration::from_micros(1)));
    let prev = [s.interval.as_secs() as i64, s.interval.subsec_micros() as i64, left.as_secs() as i64, left.subsec_micros() as i64];
    if let Some((interval, value)) = new {
        s.interval = interval;
        s.deadline = (!value.is_zero()).then(|| now + value);
        if s.deadline.is_some() && !s.running {
            s.running = true;
            let timer = timer.clone();
            std::thread::spawn(move || run_itimer(timer));
        }
        timer.wake.notify_all();
    }
    drop(s);
    if old != 0 {
        mem.write_pod(old, &prev)?;
    }
    Ok(0)
}

/// The process's interval timer `which`, made on first use.
fn itimer(p: &Process, which: usize) -> Arc<ITimer> {
    let mut timers = p.itimers.lock().unwrap_or_else(|e| e.into_inner());
    timers.0[which]
        .get_or_insert_with(|| {
            let (signo, clock, cpu): (u64, CpuClock, bool) = match which {
                0 => {
                    let start = Instant::now();
                    (SIGALRM, Box::new(move || start.elapsed()), false)
                }
                1 => (26, p.kernel.cpu_clock(p, true), true),  // SIGVTALRM
                _ => (27, p.kernel.cpu_clock(p, false), true), // SIGPROF
            };
            Arc::new(ITimer { signo, clock, cpu, state: Mutex::default(), wake: Condvar::new(), inbox: p.inbox.clone() })
        })
        .clone()
}

/// An interval timer's thread: sleep until it expires, signal, repeat while
/// armed.
fn run_itimer(timer: Arc<ITimer>) {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
    let mut s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let Some(deadline) = s.deadline else {
            s.running = false;
            return;
        };
        let now = (timer.clock)();
        if now < deadline {
            // CPU time can run up to a second a second per CPU: look again
            // before it could have got there.
            let wait = if timer.cpu { ((deadline - now) / cpus).max(Duration::from_millis(1)) } else { deadline - now };
            s = timer.wake.wait_timeout(s, wait).unwrap_or_else(|e| e.into_inner()).0;
            continue;
        }
        // Late (a busy host): the next period counts from now, rather than
        // firing again at once.
        s.deadline = (!s.interval.is_zero()).then(|| (deadline + s.interval).max(now + Duration::from_micros(100)));
        {
            // A standard signal: pending once, however often it fires.
            let mut fired = timer.inbox.fired.lock().unwrap_or_else(|e| e.into_inner());
            if !fired.iter().any(|(tid, i)| tid.is_none() && i.signo == timer.signo && i.timer.is_none()) {
                fired.push((None, signal::SigInfo { signo: timer.signo, code: SI_KERNEL, ..Default::default() }));
            }
        }
        drop(s);
        signal::alert(&timer.inbox);
        s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
    }
}
