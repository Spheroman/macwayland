//! Signals: dispositions, the blocked mask, pending signals (with their
//! `siginfo`), the alternate stack, and which signal to deliver next.
//!
//! The dispatcher decides *what* happens; the core carries it out at the
//! points the guest gives control back (after a system call, a fault, or when
//! a host signal kicks the vCPU out), asking [`Process::next_delivery`]: a
//! default action that kills, or a handler, for which it builds a signal frame
//! on the guest stack and later undoes it in `rt_sigreturn`.
//!
//! Signals from outside arrive as host signals. For each signal the guest has
//! a handler for, a host handler is installed ([`Process::sync_host_signals`])
//! that marks it pending in the [`Inbox`] of the process host signals are
//! for ([`Process::receive_host_signals`]) and kicks the vCPU (the core's
//! [`set_kick`] hook). Ignored and default dispositions are the host's own, so
//! a default SIGINT kills and SIGTSTP stops the host process directly.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
};

use super::{futex::Waker, Process, Thread};
use crate::{
    abi::Timespec,
    errno,
    memory::{GuestMemory, GuestMemoryExt},
    wire::{Decoder, Encoder, Truncated},
};

const NSIG: u64 = 64;
const SIGKILL: u64 = 9;
const SIGPIPE: u64 = 13;
const SIGSTOP: u64 = 19;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SS_DISABLE: i32 = 2;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SA_RESTORER: u64 = 0x0400_0000;
/// `si_code`s.
pub const SI_USER: i32 = 0;
const SI_TKILL: i32 = -6;

/// What reaches a process from outside its system calls: host signals and
/// expired timers' signals, not yet taken into its pending sets, and the
/// signalfds and threads to wake for them. Each process has its own (several
/// will share a host once processes share a VM); timer threads keep it, and
/// host signal handlers reach it without locks.
#[derive(Debug)]
pub struct Inbox {
    /// Host signals (Linux numbering, bit n-1) caught for the process.
    host_pending: AtomicU64,
    /// Expired timers' signals: (thread, siginfo).
    pub(super) fired: Mutex<Vec<(Option<i64>, SigInfo)>>,
    /// Its signalfds' kqueues (their own copies), for host signal handlers
    /// to trigger: a fixed table, as a handler can't take a lock. -1 is free.
    pub(super) kqueues: [AtomicI32; 32],
    /// Its threads' wakers, for waking them all (a timer expired).
    wakers: Mutex<Vec<Weak<Waker>>>,
    /// Its registered threads: where signals for one of them go, and how to
    /// get each one's attention wherever it is (in the guest, in a host call
    /// on a CPU or a worker). Here rather than in the process, so that a
    /// timer's thread, which holds only the inbox, reaches its own process's
    /// threads and no one else's.
    threads: Mutex<BTreeMap<i64, super::ThreadHandle>>,
    /// Whether its threads' masks are mirrored onto their host threads
    /// ([`super::Kernel::host_signals`]).
    pub(super) mirror_host: AtomicBool,
}

impl Default for Inbox {
    fn default() -> Self {
        Inbox {
            host_pending: AtomicU64::new(0),
            fired: Mutex::default(),
            kqueues: [const { AtomicI32::new(-1) }; 32],
            wakers: Mutex::default(),
            threads: Mutex::default(),
            mirror_host: AtomicBool::new(true),
        }
    }
}

impl Inbox {
    /// A new waker for one of the process's threads, which [`alert`] reaches.
    pub(super) fn new_waker(&self) -> Arc<Waker> {
        let w = Arc::new(Waker::new());
        let mut all = self.wakers.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&w));
        w
    }

    pub(super) fn threads(&self) -> std::sync::MutexGuard<'_, BTreeMap<i64, super::ThreadHandle>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get the attention of every thread of the process, and only its: those
    /// asleep in the runtime (their wakers), in the guest or blocked in a
    /// host call (their own wake, which kicks their vCPU or interrupts their
    /// host thread).
    pub(super) fn wake_threads(&self) {
        for w in self.wakers.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(|w| w.upgrade()) {
            w.wake();
        }
        for h in self.threads().values() {
            h.waker.wake();
            (h.wake)();
        }
    }

    /// Take the host signals caught for this inbox (bit n-1 for Linux
    /// signal n).
    pub fn take_host_pending(&self) -> u64 {
        self.host_pending.swap(0, Ordering::AcqRel)
    }

    /// Host signals are for this inbox from now on: a host of many
    /// processes keeps one of its own and routes what lands in it (see
    /// [`super::HostSignalWait`]). It must live as long as the host process.
    pub fn receive_host_signals(self: &Arc<Self>) {
        HOST_TARGET.store(Arc::as_ptr(self).cast_mut(), Ordering::Release);
    }
}

/// Catch the Linux signals `signals` on the host (for whatever inbox
/// receives host signals), whatever any guest's disposition: for a host of
/// many processes, which keeps no process's dispositions on the host.
pub fn catch_host_signals(signals: &[u64]) {
    for &sig in signals {
        let Some(darwin) = to_darwin(sig) else { continue };
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = host_handler as *const () as libc::sighandler_t;
        unsafe { libc::sigaction(darwin, &sa, std::ptr::null_mut()) };
    }
}

/// The inbox host signals are for: the host's own (whose thread hands them
/// to the process table), or a process on its own's.
static HOST_TARGET: AtomicPtr<Inbox> = AtomicPtr::new(std::ptr::null_mut());

/// How the core gets a running vCPU to stop and look at pending signals.
static KICK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// How the core says the thread making a call is wanted elsewhere: its CPU
/// is needed for another thread, so a call waiting in a loop of its own
/// (`futex`) should give up with EINTR, for the core to make it again.
static MUST_YIELD: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Set the function blocking calls ask whether to give up (see
/// [`must_yield`]).
pub fn set_yield_check(check: fn() -> bool) {
    MUST_YIELD.store(check as *mut (), Ordering::Release);
}

/// Whether the core wants the calling host thread's call to give up.
pub(super) fn must_yield() -> bool {
    let check = MUST_YIELD.load(Ordering::Acquire);
    if check.is_null() {
        return false;
    }
    let check: fn() -> bool = unsafe { std::mem::transmute(check) };
    check()
}

/// Set the function host signal handlers call to interrupt the guest. It must
/// be async-signal-safe.
pub fn set_kick(kick: fn()) {
    KICK.store(kick as *mut (), Ordering::Release);
}

extern "C" fn host_handler(sig: libc::c_int) {
    // Safety: the target is cleared before its process goes, and the inbox
    // lives as long as that (or longer, held by a timer).
    let Some(inbox) = (unsafe { HOST_TARGET.load(Ordering::Acquire).as_ref() }) else { return };
    inbox.host_pending.fetch_or(bit(from_darwin(sig)), Ordering::AcqRel);
    super::signalfd::notify(inbox);
    let kick = KICK.load(Ordering::Acquire);
    if !kick.is_null() {
        let kick: fn() = unsafe { std::mem::transmute(kick) };
        kick();
    }
}

/// The kernel's `struct sigaction` on arm64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// `stack_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackT {
    pub sp: u64,
    pub flags: i32,
    pub _pad: u32,
    pub size: u64,
}

impl Default for StackT {
    fn default() -> Self {
        StackT { sp: 0, flags: SS_DISABLE, _pad: 0, size: 0 }
    }
}

impl StackT {
    pub fn enabled(&self) -> bool {
        self.flags & SS_DISABLE == 0
    }

    pub fn contains(&self, sp: u64) -> bool {
        self.enabled() && sp > self.sp && sp <= self.sp + self.size
    }
}

/// A `siginfo_t` (128 bytes): the fields we fill in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigInfo {
    pub signo: u64,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    /// `si_addr` for faults.
    pub addr: u64,
    /// For a timer's signal: its id, overrun count, and `sigev_value`.
    pub timer: Option<(i32, i32, u64)>,
    /// SIGCHLD's `si_status`: the child's exit code, or the signal that
    /// ended, stopped or continued it.
    pub status: i32,
}

impl SigInfo {
    /// From process `pid` (another, or ourselves, with `kill`).
    pub fn user(signo: u64, code: i32, pid: i64) -> Self {
        SigInfo { signo, code, pid: pid as i32, uid: unsafe { libc::getuid() }, ..Default::default() }
    }

    /// SIGCHLD from child `pid`: `code` CLD_EXITED, CLD_KILLED, CLD_STOPPED,
    /// ...
    pub fn child(pid: i64, code: i32, status: i32) -> Self {
        SigInfo { signo: 17, code, pid: pid as i32, uid: unsafe { libc::getuid() }, status, ..Default::default() }
    }

    /// A fault at `addr`.
    pub fn fault(signo: u64, code: i32, addr: u64) -> Self {
        SigInfo { signo, code, addr, ..Default::default() }
    }

    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..4].copy_from_slice(&(self.signo as i32).to_le_bytes());
        b[8..12].copy_from_slice(&self.code.to_le_bytes());
        if let Some((id, overrun, value)) = self.timer {
            b[16..20].copy_from_slice(&id.to_le_bytes());
            b[20..24].copy_from_slice(&overrun.to_le_bytes());
            b[24..32].copy_from_slice(&value.to_le_bytes());
        } else if self.addr != 0 {
            b[16..24].copy_from_slice(&self.addr.to_le_bytes());
        } else {
            b[16..20].copy_from_slice(&self.pid.to_le_bytes());
            b[20..24].copy_from_slice(&self.uid.to_le_bytes());
            b[24..28].copy_from_slice(&self.status.to_le_bytes());
        }
        b
    }
}

/// The first real-time signal (the kernel's SIGRTMIN; C libraries keep the
/// first few for themselves). Real-time signals queue: each one sent is
/// delivered. A standard signal is pending once, however often it's sent.
const FIRST_REALTIME: u64 = 32;

/// The most of one real-time signal queued at once (Linux bounds them by
/// RLIMIT_SIGPENDING, per user; this bounds the memory they take).
const MAX_QUEUED: usize = 1024;

/// Raised signals not yet delivered, with the details of each.
#[derive(Debug, Clone, Default)]
pub struct Pending {
    bits: u64,
    info: BTreeMap<u64, std::collections::VecDeque<SigInfo>>,
}

impl Pending {
    fn add(&mut self, info: SigInfo) {
        let sig = info.signo;
        self.bits |= bit(sig);
        let queued = self.info.entry(sig).or_default();
        // A timer's signal is queued once: expiries while it's pending count
        // as its overrun, as on Linux.
        if let Some((id, overrun, _)) = info.timer
            && let Some(prev) = queued.iter_mut().find(|i| i.timer.is_some_and(|t| t.0 == id))
        {
            if let Some(t) = &mut prev.timer {
                t.1 = t.1.saturating_add(overrun.saturating_add(1));
            }
            return;
        }
        if queued.is_empty() || (sig >= FIRST_REALTIME && queued.len() < MAX_QUEUED) {
            queued.push_back(info);
        }
    }

    /// Every signal pending, queued ones each, lowest number first (what a
    /// process takes with it to another host).
    pub(super) fn queued(&self) -> Vec<SigInfo> {
        let mut out = Vec::new();
        for sig in (1..=NSIG).filter(|&sig| self.bits & bit(sig) != 0) {
            match self.info.get(&sig).filter(|q| !q.is_empty()) {
                Some(q) => out.extend(q.iter().map(|i| SigInfo { timer: None, ..*i })),
                None => out.push(SigInfo { signo: sig, ..Default::default() }),
            }
        }
        out
    }

    /// Whether any signal in `set` is pending.
    pub(super) fn any(&self, set: u64) -> bool {
        self.bits & set != 0
    }

    /// Take the lowest-numbered signal in `ready`.
    pub(super) fn take(&mut self, ready: u64) -> Option<SigInfo> {
        let ready = self.bits & ready;
        if ready == 0 {
            return None;
        }
        let sig = ready.trailing_zeros() as u64 + 1;
        let queued = self.info.get_mut(&sig);
        let info = queued.and_then(|q| q.pop_front());
        // Pending still while more are queued (real-time signals).
        if self.info.get(&sig).is_none_or(|q| q.is_empty()) {
            self.bits &= !bit(sig);
            self.info.remove(&sig);
        }
        Some(info.unwrap_or(SigInfo { signo: sig, ..Default::default() }))
    }

    fn discard(&mut self, sig: u64) {
        self.bits &= !bit(sig);
        self.info.remove(&sig);
    }
}

/// The process's signal state, shared by its threads: dispositions and
/// signals sent to the process as a whole.
#[derive(Debug, Clone)]
pub struct Signals {
    /// Indexed by signal number - 1.
    actions: [SigAction; NSIG as usize],
    pub(super) pending: Pending,
    /// Signals some signalfd has watched: caught on the host even when
    /// their action is the default, so that they're pending to read rather
    /// than the host's to act on.
    signalfd_watched: u64,
    /// Dispositions changed since the host's were last set to match.
    host_stale: bool,
    /// The signals ignored now, by SIG_IGN or a default of ignore (bit n-1
    /// for signal n): kept as the dispositions change, as every blocking call
    /// asks ([`Process::signal_ready`]).
    ignored: u64,
}

impl Default for Signals {
    fn default() -> Self {
        let mut s = Signals { actions: [SigAction::default(); NSIG as usize], pending: Pending::default(), signalfd_watched: 0, host_stale: true, ignored: 0 };
        s.actions_changed();
        s
    }
}

/// One thread's signal state: its mask, alternate stack, and the signals sent
/// to it in particular (shared, so other threads can send them).
#[derive(Debug, Default)]
pub struct ThreadSignals {
    pub(super) blocked: u64,
    altstack: StackT,
    /// The mask to go back to once the signal that ends `rt_sigsuspend` is
    /// dealt with.
    suspend_saved: Option<u64>,
    pub(super) pending: Arc<Mutex<Pending>>,
    /// The job-control signals last blocked on the host thread (see
    /// [`Thread::sync_host_mask`]).
    host_blocked: u64,
}

impl ThreadSignals {
    /// A wait with a mask of its own (`rt_sigsuspend`, `ppoll`...) parks
    /// rather than end: the thread's own mask is back until it's made again.
    pub(super) fn end_wait_mask(&mut self) {
        if let Some(mask) = self.suspend_saved.take() {
            self.blocked = mask;
        }
    }

    /// A new thread's: its creator's mask, no alternate stack, nothing pending.
    pub(super) fn spawned(&self) -> Self {
        ThreadSignals { blocked: self.blocked, ..Default::default() }
    }
}

impl Signals {
    /// The dispositions changed: what's ignored now.
    fn actions_changed(&mut self) {
        self.ignored = (1..=NSIG)
            .filter(|&sig| match self.actions[sig as usize - 1].handler {
                SIG_IGN => true,
                SIG_DFL => !terminates_by_default(sig) && !stops_by_default(sig),
                _ => false,
            })
            .fold(0, |m, sig| m | bit(sig));
    }

    /// The dispositions, and the forking thread's mask and alternate stack.
    pub fn encode(&self, t: &ThreadSignals, e: &mut Encoder) {
        for a in &self.actions {
            e.u64(a.handler).u64(a.flags).u64(a.restorer).u64(a.mask);
        }
        e.u64(self.signalfd_watched);
        e.u64(t.blocked).u64(t.altstack.sp).i64(t.altstack.flags as i64).u64(t.altstack.size);
    }

    /// A forked child's: the parent's dispositions, the forking thread's mask
    /// and alternate stack; no pending signals (as on Linux).
    pub fn decode(d: &mut Decoder) -> Result<(Self, ThreadSignals), Truncated> {
        let mut s = Signals::default();
        for a in &mut s.actions {
            *a = SigAction { handler: d.u64()?, flags: d.u64()?, restorer: d.u64()?, mask: d.u64()? };
        }
        s.signalfd_watched = d.u64()?;
        s.actions_changed();
        let t = ThreadSignals {
            blocked: d.u64()?,
            altstack: StackT { sp: d.u64()?, flags: d.i64()? as i32, _pad: 0, size: d.u64()? },
            ..Default::default()
        };
        Ok((s, t))
    }

    /// `execve`: handlers go back to the default (the code they pointed to is
    /// gone); ignored signals stay ignored, the mask and pending signals are
    /// kept, and the alternate stack is dropped.
    pub fn reset_on_exec(&mut self, t: &mut ThreadSignals) {
        for a in &mut self.actions {
            if a.handler != SIG_IGN {
                *a = SigAction::default();
            }
        }
        t.altstack = StackT::default();
        self.host_stale = true;
        self.actions_changed();
    }

    /// Whether a signal raised now is dropped at once: ignored, and (for a
    /// thread's own signals) not blocked — it might be unignored before it's
    /// unblocked.
    /// A signalfd watches `set`: catch them on the host from now on.
    pub(super) fn watch_for_signalfd(&mut self, set: u64) {
        if set & !self.signalfd_watched != 0 {
            self.signalfd_watched |= set;
            self.host_stale = true;
        }
    }

    /// The signals ignored by SIG_IGN.
    pub(super) fn ignored_now(&self) -> u64 {
        self.ignored()
    }

    /// The signals with a handler.
    pub(super) fn caught(&self) -> u64 {
        (1..=NSIG).filter(|&sig| !matches!(self.actions[sig as usize - 1].handler, SIG_DFL | SIG_IGN)).fold(0, |m, sig| m | bit(sig))
    }

    /// The signals set to be ignored (bit n-1 for signal n).
    pub(super) fn ignored(&self) -> u64 {
        (1..=NSIG).filter(|&sig| self.actions[sig as usize - 1].handler == SIG_IGN).fold(0, |m, sig| m | bit(sig))
    }

    fn drops(&self, sig: u64, blocked: u64) -> bool {
        self.actions[sig as usize - 1].handler == SIG_IGN && blocked & bit(sig) == 0
    }

    fn take_host_pending(&mut self, inbox: &Inbox) {
        // Looked at first: a swap writes the line every thread of the
        // process reads, on every blocking call, though it's nearly always 0.
        if inbox.host_pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let from_host = inbox.host_pending.swap(0, Ordering::AcqRel);
        for sig in (1..=NSIG).filter(|&sig| from_host & bit(sig) != 0) {
            self.pending.add(SigInfo { signo: sig, code: SI_USER, ..Default::default() });
        }
    }
}

/// A signal arrived from outside a system call (a timer) for the process
/// with `inbox`: get its threads' attention, and no other process's (many
/// share a host: kicking every vCPU and interrupting every host thread, as a
/// host signal's handler does, would interrupt them all).
pub(super) fn alert(inbox: &Inbox) {
    super::signalfd::notify(inbox);
    inbox.wake_threads();
}

impl Process {
    /// Host signals are for this process from now on (the runtime's one
    /// process; a host of many will keep them for itself).
    pub fn receive_host_signals(&self) {
        HOST_TARGET.store(Arc::as_ptr(&self.inbox).cast_mut(), Ordering::Release);
    }

    /// Stop receiving host signals, if this process was (it's going).
    pub(super) fn stop_receiving_host_signals(&self) {
        let ours = Arc::as_ptr(&self.inbox).cast_mut();
        let _ = HOST_TARGET.compare_exchange(ours, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
    }

    /// A signal was made pending for the process: its signalfds' readers
    /// look.
    pub(super) fn signal_added(&self) {
        super::signalfd::notify(&self.inbox);
    }

    /// A signal for the process (`tid` None) or one of its threads, from
    /// outside its own calls: another process of the host, or the kernel
    /// (SIGCHLD). It waits in the inbox until a thread takes it in (as it
    /// returns to the guest, or in a wait for signals, which it ends), and
    /// the threads are woken to look. Dropped at once if ignored, as Linux
    /// does when it's sent.
    pub fn post(&self, tid: Option<i64>, info: SigInfo) {
        if self.signals().drops(info.signo, 0) {
            return;
        }
        self.discard_contrary(info.signo);
        self.inbox.fired.lock().unwrap_or_else(|e| e.into_inner()).push((tid, info));
        self.signal_added();
        match tid {
            Some(tid) => self.wake_thread(tid),
            None => self.wake_all_threads(),
        }
    }

    /// Whether thread `t` blocks or ignores `sig` (SIG_IGN): what Linux's
    /// terminal checks call ignored.
    pub fn blocks_or_ignores(&self, t: &Thread, sig: u64) -> bool {
        t.signals.blocked & bit(sig) != 0 || self.signals().actions[sig as usize - 1].handler == SIG_IGN
    }

    /// `sig` is being sent: as Linux does as it's generated, a SIGCONT
    /// discards pending stop signals, and a stop signal a pending SIGCONT
    /// (so a job continued just after it was told to stop runs on).
    fn discard_contrary(&self, sig: u64) {
        const SIGCONT: u64 = 18;
        let gone = match sig {
            SIGCONT => STOPS,
            19..=22 => bit(SIGCONT),
            _ => return,
        };
        self.inbox.fired.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, i)| bit(i.signo) & gone == 0);
        let mut s = self.signals();
        for sig in (1..=NSIG).filter(|&g| gone & bit(g) != 0) {
            s.pending.discard(sig);
            for h in self.inbox.threads().values() {
                h.pending.lock().unwrap_or_else(|e| e.into_inner()).discard(sig);
            }
        }
    }

    /// What SIGCHLD's disposition says about children: whether they're
    /// reaped as they end, rather than left for `wait` (SIG_IGN or
    /// SA_NOCLDWAIT), and whether their stops are reported with a SIGCHLD
    /// (not with SA_NOCLDSTOP).
    pub fn child_policy(&self) -> (bool, bool) {
        const SA_NOCLDSTOP: u64 = 1;
        const SA_NOCLDWAIT: u64 = 2;
        let a = self.signals().actions[17 - 1];
        (a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0, a.flags & SA_NOCLDSTOP == 0)
    }

    /// Take in signals that arrived from outside: host signals, and expired
    /// timers' (for the process, thread `tid`, or another thread).
    pub(super) fn take_external(&self, s: &mut Signals, ts: &ThreadSignals, tid: i64) {
        s.take_host_pending(&self.inbox);
        let fired = std::mem::take(&mut *self.inbox.fired.lock().unwrap_or_else(|e| e.into_inner()));
        for (target, info) in fired {
            match target {
                Some(t) if t == tid => ts.pending.lock().unwrap_or_else(|e| e.into_inner()).add(info),
                Some(t) => match self.thread_pending(t) {
                    Some(pending) => pending.lock().unwrap_or_else(|e| e.into_inner()).add(info),
                    None => s.pending.add(info),
                },
                None => s.pending.add(info),
            }
        }
    }
}

/// What the core should do with the next signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Its default action ends the process.
    Kill(u64),
    /// Run a handler.
    Handler(HandlerCall),
}

/// A handler to run, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerCall {
    pub info: SigInfo,
    pub handler: u64,
    /// Where it returns to (SA_RESTORER), or None for the runtime's own
    /// `rt_sigreturn` trampoline (what Linux's vDSO provides).
    pub restorer: Option<u64>,
    /// SA_ONSTACK: the alternate stack, if one is set up.
    pub altstack: Option<StackT>,
    /// The mask to restore when the handler returns.
    pub saved_mask: u64,
    /// SA_RESTART: a system call it interrupted should be restarted.
    pub restart: bool,
}

impl Process {
    pub(super) fn signals(&self) -> std::sync::MutexGuard<'_, Signals> {
        self.signals.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The signals in `set` (bit n-1 for signal n) start out ignored, as a
    /// program inherits SIG_IGN across `execve`: a client's process, those
    /// the Mac process that started the client ignored (`nohup`, a
    /// non-interactive shell's `&`).
    pub fn ignore_signals(&self, set: u64) {
        let mut s = self.signals();
        for sig in (1..=NSIG).filter(|&sig| set & bit(sig) != 0 && bit(sig) & UNBLOCKABLE == 0) {
            s.actions[sig as usize - 1].handler = SIG_IGN;
        }
        s.host_stale = true;
        s.actions_changed();
    }
}

impl Thread {
    /// The thread blocks `mask` (bit n-1 for signal n) from now on: a
    /// client's process starts with the mask the client was started with.
    pub fn set_blocked_signals(&mut self, mask: u64) {
        self.signals.blocked = mask & !UNBLOCKABLE;
    }
}

impl Process {
    /// The next signal for thread `t` to act on, if any: pending (for it or
    /// the process), not blocked, not ignored. For a handler, the thread's
    /// mask is updated as the handler runs with it.
    pub fn next_delivery(&self, t: &mut Thread) -> Option<Delivery> {
        let mut s = self.signals();
        self.take_external(&mut s, &t.signals, t.tid);
        let ts = &mut t.signals;
        loop {
            let ready = !ts.blocked;
            // The thread's own signals first, then the process's (as Linux).
            let next = ts.pending.lock().unwrap_or_else(|e| e.into_inner()).take(ready).or_else(|| s.pending.take(ready));
            let Some(info) = next else {
                if let Some(mask) = ts.suspend_saved.take() {
                    ts.blocked = mask;
                }
                return None;
            };
            let sig = info.signo;
            let action = s.actions[sig as usize - 1];
            match action.handler {
                SIG_IGN => continue,
                SIG_DFL if terminates_by_default(sig) => return Some(Delivery::Kill(sig)),
                SIG_DFL => {
                    if stops_by_default(sig) {
                        // Job control: the process stops, as Linux would.
                        // (Not holding its signals: stopping tells the
                        // parent, whose signals the kernel may take.)
                        drop(s);
                        self.kernel.stop(self, sig);
                        s = self.signals();
                    }
                    continue;
                }
                handler => {
                    let call = start_handler(&mut s, ts, sig, action, handler, info);
                    drop(s);
                    t.sync_host_mask();
                    return Some(Delivery::Handler(call));
                }
            }
        }
    }

    /// Whether a signal is waiting that thread `t` would act on now (so a
    /// blocking wait should give up with EINTR).
    pub fn signal_ready(&self, t: &Thread) -> bool {
        // One hold of the signals: every blocking call asks, several times.
        let mut s = self.signals();
        self.take_external(&mut s, &t.signals, t.tid);
        let pending = s.pending.bits | t.signals.pending.lock().unwrap_or_else(|e| e.into_inner()).bits | self.inbox.host_pending.load(Ordering::Acquire);
        pending & !t.signals.blocked & !self.ignoring(&s) != 0
    }

    /// Signals that would be ignored now, which (on Linux, where they're
    /// dropped as they're sent unless blocked) wake and interrupt nothing.
    /// Signals from other processes arrive in memory: a child's SIGCHLD at
    /// its default action mustn't end its parent's `wait4` with EINTR. (For
    /// a process on its own that mirrors the host, the host's own
    /// dispositions do that already.)
    fn ignoring(&self, s: &Signals) -> u64 {
        if self.inbox.mirror_host.load(Ordering::Relaxed) {
            return 0;
        }
        s.ignored
    }

    /// A fault thread `t` can't continue past (SIGSEGV, SIGBUS, SIGILL...): its
    /// handler if it has one and isn't blocking it, else the default — which
    /// kills, as Linux forces.
    pub fn fault(&self, t: &mut Thread, info: SigInfo) -> Delivery {
        let mut s = self.signals();
        let sig = info.signo;
        let action = s.actions[sig as usize - 1];
        if action.handler == SIG_DFL || action.handler == SIG_IGN || t.signals.blocked & bit(sig) != 0 {
            return Delivery::Kill(sig);
        }
        Delivery::Handler(start_handler(&mut s, &mut t.signals, sig, action, action.handler, info))
    }

    /// Make the host's signal dispositions match the guest's, if they changed
    /// (and the host process is this Linux process alone).
    pub fn sync_host_signals(&self) {
        if !self.inbox.mirror_host.load(Ordering::Relaxed) {
            return;
        }
        let mut s = self.signals();
        if !std::mem::take(&mut s.host_stale) {
            return;
        }
        for (sig, darwin) in LINUX_TO_DARWIN {
            if darwin == 0 || darwin == libc::SIGKILL || darwin == libc::SIGSTOP || sig == SIGPIPE {
                continue; // SIGPIPE stays ignored: the runtime raises it itself on EPIPE
            }
            let disposition = match s.actions[sig as usize - 1].handler {
                SIG_DFL if s.signalfd_watched & bit(sig) != 0 => host_handler as *const () as libc::sighandler_t,
                SIG_DFL => libc::SIG_DFL,
                SIG_IGN => libc::SIG_IGN,
                _ => host_handler as *const () as libc::sighandler_t,
            };
            // No SA_RESTART: a host call the signal interrupts returns EINTR,
            // and the guest sees EINTR or a restart as its own flags say.
            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            sa.sa_sigaction = disposition;
            unsafe { libc::sigaction(darwin, &sa, std::ptr::null_mut()) };
        }
    }
}

/// The signals the terminal sends a process that touches it from the
/// background: SIGTSTP, SIGTTIN, SIGTTOU.
const JOB_CONTROL: u64 = (1 << (20 - 1)) | (1 << (21 - 1)) | (1 << (22 - 1));
/// The stop signals: SIGSTOP and the job-control ones.
const STOPS: u64 = JOB_CONTROL | (1 << (19 - 1));

impl Thread {
    /// `rt_sigreturn`: the handler's done; back to the mask it saved.
    pub fn sigreturn(&mut self, mask: u64) {
        self.signals.blocked = mask & !UNBLOCKABLE;
        self.sync_host_mask();
    }

    /// Mirror the thread's mask for the job-control signals onto its host
    /// thread. The terminal driver sends those to the host process itself,
    /// and a guest that blocks one (bash does around `tcsetpgrp` in a child)
    /// must not be stopped by it.
    pub fn sync_host_mask(&mut self) {
        self.sync_host_mask_from(self.signals.host_blocked);
    }

    /// The thread moved to another host thread: set its host mask there.
    pub fn moved_host_thread(&mut self) {
        // Whatever this host thread had (another thread's mask, or none).
        self.sync_host_mask_from(u64::MAX);
    }

    fn sync_host_mask_from(&mut self, host_blocked: u64) {
        if !self.inbox.mirror_host.load(Ordering::Relaxed) {
            return;
        }
        self.signals.host_blocked = host_blocked;
        let want = self.signals.blocked & JOB_CONTROL;
        if want == self.signals.host_blocked {
            return;
        }
        let mut set: libc::sigset_t = 0;
        unsafe { libc::sigemptyset(&mut set) };
        for sig in [20u64, 21, 22] {
            if want & bit(sig) != 0 {
                unsafe { libc::sigaddset(&mut set, to_darwin(sig).unwrap()) };
            }
        }
        let mut cur: libc::sigset_t = 0;
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur);
            for sig in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                libc::sigdelset(&mut cur, sig);
            }
            for sig in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                if libc::sigismember(&set, sig) == 1 {
                    libc::sigaddset(&mut cur, sig);
                }
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &cur, std::ptr::null_mut());
        }
        self.signals.host_blocked = want;
    }

    /// The alternate signal stack.
    pub fn altstack(&self) -> StackT {
        self.signals.altstack
    }
}

/// The bookkeeping of entering a handler: the mask it runs with, and
/// SA_RESETHAND.
fn start_handler(s: &mut Signals, ts: &mut ThreadSignals, sig: u64, action: SigAction, handler: u64, info: SigInfo) -> HandlerCall {
    let saved_mask = ts.suspend_saved.take().unwrap_or(ts.blocked);
    ts.blocked |= action.mask;
    if action.flags & SA_NODEFER == 0 {
        ts.blocked |= bit(sig);
    }
    ts.blocked &= !UNBLOCKABLE;
    if action.flags & SA_RESETHAND != 0 {
        s.actions[sig as usize - 1] = SigAction::default();
        s.host_stale = true;
        s.actions_changed();
    }
    HandlerCall {
        info,
        handler,
        restorer: (action.flags & SA_RESTORER != 0 && action.restorer != 0).then_some(action.restorer),
        altstack: (action.flags & SA_ONSTACK != 0 && ts.altstack.enabled()).then_some(ts.altstack),
        saved_mask,
        restart: action.flags & SA_RESTART != 0,
    }
}

const LINUX_TO_DARWIN: [(u64, i32); 31] = [
    (1, libc::SIGHUP),
    (2, libc::SIGINT),
    (3, libc::SIGQUIT),
    (4, libc::SIGILL),
    (5, libc::SIGTRAP),
    (6, libc::SIGABRT),
    (7, libc::SIGBUS),
    (8, libc::SIGFPE),
    (9, libc::SIGKILL),
    (10, libc::SIGUSR1),
    (11, libc::SIGSEGV),
    (12, libc::SIGUSR2),
    (13, libc::SIGPIPE),
    (14, libc::SIGALRM),
    (15, libc::SIGTERM),
    (17, libc::SIGCHLD),
    (18, libc::SIGCONT),
    (19, libc::SIGSTOP),
    (20, libc::SIGTSTP),
    (21, libc::SIGTTIN),
    (22, libc::SIGTTOU),
    (23, libc::SIGURG),
    (24, libc::SIGXCPU),
    (25, libc::SIGXFSZ),
    (26, libc::SIGVTALRM),
    (27, libc::SIGPROF),
    (28, libc::SIGWINCH),
    (29, libc::SIGIO),
    (31, libc::SIGSYS),
    (16, 0), // SIGSTKFLT
    (30, 0), // SIGPWR
];

/// The Darwin signal for a Linux one, if there is one.
pub fn to_darwin(sig: u64) -> Option<i32> {
    LINUX_TO_DARWIN.iter().find(|&&(l, _)| l == sig).map(|&(_, d)| d).filter(|&d| d != 0)
}

/// The Linux signal for a Darwin one. SIGEMT and SIGINFO, which Linux
/// lacks, come out as SIGTERM: a death by an unknown signal still reads as a
/// death by a signal.
pub fn from_darwin(sig: i32) -> u64 {
    LINUX_TO_DARWIN.iter().find(|&&(_, d)| d == sig && d != 0).map_or(15, |&(l, _)| l)
}

pub(super) fn bit(sig: u64) -> u64 {
    1 << (sig - 1)
}

/// Signals that can never be blocked.
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

pub(super) fn rt_sigaction<M: GuestMemory + ?Sized>(p: &Process, mem: &M, sig: u64, act: u64, old: u64, setsize: u64) -> Result<i64, i64> {
    if setsize != 8 || !(1..=NSIG).contains(&sig) {
        return Err(errno::EINVAL);
    }
    if act != 0 && (sig == SIGKILL || sig == SIGSTOP) {
        return Err(errno::EINVAL);
    }
    let new = if act != 0 { Some(mem.read_pod::<SigAction>(act)?) } else { None };
    let mut s = p.signals();
    if old != 0 {
        mem.write_pod(old, &s.actions[sig as usize - 1])?;
    }
    if let Some(mut new) = new {
        new.mask &= !UNBLOCKABLE;
        s.actions[sig as usize - 1] = new;
        // Pending signals now ignored are discarded.
        if new.handler == SIG_IGN || (new.handler == SIG_DFL && !terminates_by_default(sig) && !stops_by_default(sig)) {
            s.pending.discard(sig);
        }
        s.host_stale = true;
        s.actions_changed();
    }
    Ok(0)
}

pub(super) fn rt_sigprocmask<M: GuestMemory + ?Sized>(t: &mut Thread, mem: &M, how: u64, set: u64, old: u64, setsize: u64) -> Result<i64, i64> {
    if setsize != 8 {
        return Err(errno::EINVAL);
    }
    let ts = &mut t.signals;
    let cur = ts.blocked;
    if set != 0 {
        let s: u64 = mem.read_pod(set)?;
        ts.blocked = match how {
            0 => cur | s,  // SIG_BLOCK
            1 => cur & !s, // SIG_UNBLOCK
            2 => s,        // SIG_SETMASK
            _ => return Err(errno::EINVAL),
        } & !UNBLOCKABLE;
    }
    if old != 0 {
        mem.write_pod(old, &cur)?;
    }
    Ok(0)
}

pub(super) fn sigaltstack<M: GuestMemory + ?Sized>(t: &mut Thread, mem: &M, ss: u64, old: u64) -> Result<i64, i64> {
    let new = if ss != 0 { Some(mem.read_pod::<StackT>(ss)?) } else { None };
    if old != 0 {
        mem.write_pod(old, &t.signals.altstack)?;
    }
    if let Some(new) = new {
        const MINSIGSTKSZ: u64 = 5120; // arm64
        if new.flags & !SS_DISABLE != 0 {
            return Err(errno::EINVAL);
        }
        if new.flags & SS_DISABLE == 0 && new.size < MINSIGSTKSZ {
            return Err(errno::ENOMEM);
        }
        t.signals.altstack = new;
    }
    Ok(0)
}

/// Default action of each signal: true if it terminates the process.
fn terminates_by_default(sig: u64) -> bool {
    // SIGCHLD, SIGCONT, SIGURG, SIGWINCH are ignored; SIGSTOP/TSTP/TTIN/TTOU
    // stop.
    !matches!(sig, 17 | 18 | 19 | 20 | 21 | 22 | 23 | 28)
}

fn stops_by_default(sig: u64) -> bool {
    matches!(sig, 19..=22)
}

/// `kill(target, sig)`: to this process, the signal is made pending, for a
/// thread to deliver as its call returns; to anything else (another process,
/// a group, every process), it's the kernel's to route.
pub(super) fn kill(p: &Process, target: i64, sig: u64) -> Result<i64, i64> {
    if sig > NSIG {
        return Err(errno::EINVAL);
    }
    if target != p.pid {
        return p.kernel.kill(p, target, sig);
    }
    kill_self(p, sig)
}

/// `kill` of the caller's own process.
pub(super) fn kill_self(p: &Process, sig: u64) -> Result<i64, i64> {
    if sig != 0 {
        p.discard_contrary(sig);
        let mut s = p.signals();
        if !s.drops(sig, 0) {
            s.pending.add(SigInfo::user(sig, SI_USER, p.pid));
            drop(s);
            p.signal_added();
        }
    }
    Ok(0)
}

/// `tkill(tid, sig)` / `tgkill(tgid, tid, sig)`: to one thread.
pub(super) fn tkill(p: &Process, t: &mut Thread, tgid: Option<i64>, tid: i64, sig: u64) -> Result<i64, i64> {
    if sig > NSIG || tid <= 0 || tgid.is_some_and(|g| g <= 0) {
        return Err(errno::EINVAL);
    }
    if tgid.is_some_and(|g| g != p.pid) {
        return p.kernel.tgkill(p, tgid, tid, sig); // another process's thread
    }
    let target = if tid == t.tid { Some(t.signals.pending.clone()) } else { p.thread_pending(tid) };
    let Some(pending) = target else {
        return if tgid.is_none() { p.kernel.tgkill(p, None, tid, sig) } else { Err(errno::ESRCH) };
    };
    if sig != 0 && !p.signals().drops(sig, if tid == t.tid { t.signals.blocked } else { 0 }) {
        pending.lock().unwrap_or_else(|e| e.into_inner()).add(SigInfo::user(sig, SI_TKILL, p.pid));
        p.signal_added();
        if tid != t.tid {
            p.wake_thread(tid);
        }
    }
    Ok(0)
}

/// Raise `sig` in thread `t`, from the kernel (SIGPIPE on EPIPE).
pub(super) fn raise(t: &Thread, sig: u64) {
    t.signals.pending.lock().unwrap_or_else(|e| e.into_inner()).add(SigInfo { signo: sig, code: 0x80 /* SI_KERNEL */, ..Default::default() });
}

/// `rt_sigsuspend`: wait with `mask` blocked until a signal arrives that's
/// let through, then fail with EINTR; the original mask comes back once that
/// signal is dealt with ([`Process::next_delivery`]).
pub(super) fn rt_sigsuspend<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, mask: u64, size: u64) -> Result<i64, i64> {
    if size != 8 {
        return Err(errno::EINVAL);
    }
    let new: u64 = mem.read_pod(mask)?;
    let tid = t.tid;
    let ts = &mut t.signals;
    ts.suspend_saved = Some(ts.blocked);
    ts.blocked = new & !UNBLOCKABLE;
    loop {
        let ready = {
            let mut s = p.signals();
            p.take_external(&mut s, &t.signals, tid);
            (s.pending.bits | t.signals.pending.lock().unwrap_or_else(|e| e.into_inner()).bits) & !t.signals.blocked & !p.ignoring(&s)
        };
        if ready != 0 || p.exiting() {
            return Err(errno::EINTR);
        }
        // Whoever sends the thread a signal wakes it (a host signal too:
        // the core wakes the threads it's for).
        super::block::wait_woken(t, None)?;
    }
}

/// Wait with `mask` (`epoll_pwait`, `ppoll`): the thread's mask until the
/// signal that ends the wait is dealt with, as in `rt_sigsuspend`.
pub(super) fn wait_with_mask(t: &mut Thread, mask: u64) {
    let ts = &mut t.signals;
    ts.suspend_saved = Some(ts.blocked);
    ts.blocked = mask & !UNBLOCKABLE;
}

/// `rt_sigpending`: what's pending for the thread or the process.
pub(super) fn rt_sigpending<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, set: u64, size: u64) -> Result<i64, i64> {
    if size != 8 {
        return Err(errno::EINVAL);
    }
    let bits = p.signals().pending.bits | t.signals.pending.lock().unwrap_or_else(|e| e.into_inner()).bits | p.inbox.host_pending.load(Ordering::Acquire);
    mem.write_pod(set, &bits)?;
    Ok(0)
}

/// `rt_sigtimedwait`: take a pending signal in `set` (without running its
/// handler), waiting for one up to `timeout` (null: forever).
pub(super) fn rt_sigtimedwait<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, set: u64, info: u64, timeout: u64, size: u64) -> Result<i64, i64> {
    if size != 8 {
        return Err(errno::EINVAL);
    }
    let set = mem.read_pod::<u64>(set)? & !UNBLOCKABLE;
    let deadline = if timeout == 0 {
        None
    } else {
        let ts: Timespec = mem.read_pod(timeout)?;
        if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
            return Err(errno::EINVAL);
        }
        Some(std::time::Instant::now() + std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    };
    let tid = t.tid;
    loop {
        let (taken, other) = {
            let mut s = p.signals();
            p.take_external(&mut s, &t.signals, tid);
            let mut own = t.signals.pending.lock().unwrap_or_else(|e| e.into_inner());
            let taken = own.take(set).or_else(|| s.pending.take(set));
            (taken, (s.pending.bits | own.bits) & !t.signals.blocked)
        };
        if let Some(got) = taken {
            if info != 0 {
                mem.write(info, &got.to_bytes())?;
            }
            return Ok(got.signo as i64);
        }
        // Another signal, one with a handler to run, ends the wait too.
        if other != 0 || p.exiting() {
            return Err(errno::EINTR);
        }
        if !super::block::wait_woken(t, deadline)? {
            return Err(errno::EAGAIN);
        }
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    fn sig(signo: u64, pid: i32) -> SigInfo {
        SigInfo { signo, pid, ..Default::default() }
    }

    #[test]
    fn realtime_signals_queue_standard_ones_dont() {
        let mut p = Pending::default();
        for pid in 1..=3 {
            p.add(sig(34, pid));
            p.add(sig(10, pid));
        }
        let all = bit(34) | bit(10);
        // Lowest number first; SIGUSR1 once, with the first sender's details.
        assert_eq!(p.take(all).map(|i| (i.signo, i.pid)), Some((10, 1)));
        for pid in 1..=3 {
            assert!(p.any(bit(34)), "still pending while more are queued");
            assert_eq!(p.take(all).map(|i| (i.signo, i.pid)), Some((34, pid)), "in the order sent");
        }
        assert!(!p.any(all));
        assert!(p.take(all).is_none());
    }

    #[test]
    fn a_pending_timer_signal_counts_expiries_as_overrun() {
        let mut p = Pending::default();
        let timer = |id: i32, overrun: i32| SigInfo { signo: 34, code: -2, timer: Some((id, overrun, 0)), ..Default::default() };
        p.add(timer(1, 0));
        p.add(timer(2, 0));
        p.add(timer(1, 0)); // timer 1 again while its signal waits
        p.add(timer(1, 2)); // and again, three expiries late
        let first = p.take(bit(34)).unwrap();
        assert_eq!(first.timer.map(|t| (t.0, t.1)), Some((1, 4)), "1 + (1 + 2) expiries folded in");
        assert_eq!(p.take(bit(34)).unwrap().timer.map(|t| t.0), Some(2), "another timer's signal queued on its own");
        assert!(!p.any(bit(34)));
    }

    #[test]
    fn realtime_queue_is_bounded() {
        let mut p = Pending::default();
        for pid in 0..(MAX_QUEUED as i32 + 10) {
            p.add(sig(40, pid));
        }
        let mut n = 0;
        while p.take(bit(40)).is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_QUEUED);
    }
}
