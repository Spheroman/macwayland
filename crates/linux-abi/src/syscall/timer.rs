//! POSIX timers (`timer_create` and friends): a signal when a timer expires.
//!
//! macOS has no per-process timers like Linux's, so each armed timer is a
//! host thread that sleeps until its deadline. On expiry it queues the
//! signal (with Linux's timer `siginfo`: id, overrun, `sigev_value`) for the
//! process or one thread, then gets the guest's attention the way a host
//! signal does: kicking the vCPUs, waking threads asleep in the runtime, and
//! interrupting a blocking host call.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use super::{misc, signal, Process, Thread};
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

/// Expired timers' signals, waiting to be taken into a process's (or a
/// thread's) pending set: (thread, siginfo).
pub(super) static FIRED: Mutex<Vec<(Option<i64>, signal::SigInfo)>> = Mutex::new(Vec::new());

/// How the core breaks every guest thread out of a blocking host call.
static INTERRUPT: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Set the function that breaks every guest thread out of a blocking host
/// call, for timers to get a sleeping thread's attention.
pub fn set_interrupt(interrupt: fn()) {
    INTERRUPT.store(interrupt as *mut (), Ordering::Release);
}

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
    ts.timers.insert(id, Arc::new(PosixTimer { id, clock, signo, value, notify, tid, state: Mutex::new(state), wake: Condvar::new() }));
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
            let mut fired = FIRED.lock().unwrap_or_else(|e| e.into_inner());
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
            signal::alert();
            s = timer.state.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Whether a timer's signal is waiting to be taken in.
pub(super) fn any_fired() -> bool {
    !FIRED.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// Interrupt blocking host calls in this process, if the core said how.
pub(super) fn interrupt_host() {
    let interrupt = INTERRUPT.load(Ordering::Acquire);
    if !interrupt.is_null() {
        let interrupt: fn() = unsafe { std::mem::transmute(interrupt) };
        interrupt();
    }
}
