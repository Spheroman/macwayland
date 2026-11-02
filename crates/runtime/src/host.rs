//! Many Linux processes in one VM (docs/processes.md, step 3), behind
//! `MACWL_ONE_VM=1`.
//!
//! A host is one Mac process with one VM that runs every Linux process of a
//! `macwl-run` tree: the processes' tasks share its CPUs, their address
//! spaces share its frame pool (each with its own ASID), and fork copies
//! page tables inside the VM, copy on write. The host keeps the process
//! table ([`Table`]): pids, parents, process groups, sessions, zombies, stops,
//! and answers the dispatcher's questions about them ([`TableKernel`], the
//! `linux_abi::Kernel` of every process here) from memory: `wait4`,
//! `waitid`, `kill`, `setpgid` and the rest. Signals between processes go
//! through one routing function ([`Table::deliver`]).
//!
//! What stays for later steps: terminals and job control (step 4: `tcsetpgrp`
//! says ENOTTY here), clients joining a host (5), Mac programs (6: an
//! `execve` of one fails with ENOEXEC, and the host process never execs).
//! The table's entries are a kind ([`Kind`]) so that stand-ins for Mac
//! programs (6) and for processes in other hosts (9) fit in beside local
//! ones; pids come from a [`PidAllocator`] handing out ranges unique across
//! the user's hosts.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::{Read, Seek, Write},
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use linux_abi::{
    errno,
    syscall::{catch_host_signals, ChildInfo, CpuClock, Fork, HostSignalWait, Inbox, Kernel, ProcStatus, SigInfo, Thread, Waited, Waker, RUSAGE_SIZE},
    GuestMemory, Process,
};

use crate::{
    cputime::Times,
    flush_tlb_now, hv, load, memory::{Machine, Space}, place, run_cpu, sched::{Sched, Start, Task}, signal, Error, Exit, Host, Proc,
    Scope,
};

/// Why a task is set aside rather than run (see [`park`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// Its process is stopped.
    Stopped,
    /// It vforked this child, and waits until the child execs or exits.
    Vfork(i64),
}

/// pids and tids for a host: unique across every host of the user, so a
/// pid names one process wherever it's seen (docs/processes.md, section 5).
/// From step 9 a coordinator hands out the ranges; until then a file.
pub(crate) trait PidAllocator: Send + Sync {
    /// A pid (or tid) no one has, if there's one to be had.
    fn alloc(&self) -> Option<i64>;
    /// `pid` is free again.
    fn free(&self, pid: i64);
}

/// The first pid: above macOS's (99999), so a Linux pid handed to a Mac
/// program never names a Mac process.
const FIRST_PID: i64 = 100_000;
/// Linux's `pid_max` on big machines.
const PID_MAX: i64 = 4_194_304;
/// Pids come to a host in ranges of this many.
const RANGE: i64 = 4096;

/// Ranges of pids handed out through a per-user file
/// (`$TMPDIR/macwl-$UID/pids`, under `flock`): a line per range, its start
/// and the Mac pid of the host that has it. A range whose host is gone is
/// free again.
pub(crate) struct RangePids {
    file: PathBuf,
    state: Mutex<RangeState>,
}

struct RangeState {
    /// The starts of the ranges this host has, in the order it got them.
    ranges: Vec<i64>,
    /// Where the next pid is looked for: an index into `ranges`, and an
    /// offset in that range.
    at: (usize, i64),
    used: HashSet<i64>,
}

impl RangePids {
    pub fn new() -> std::io::Result<RangePids> {
        let dir = std::env::temp_dir().join(format!("macwl-{}", unsafe { libc::getuid() }));
        std::fs::create_dir_all(&dir)?;
        let pids = RangePids { file: dir.join("pids"), state: Mutex::new(RangeState { ranges: Vec::new(), at: (0, 0), used: HashSet::new() }) };
        let first = pids.claim()?;
        pids.lock().ranges.push(first);
        Ok(pids)
    }

    fn lock(&self) -> MutexGuard<'_, RangeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A range no live host has, recorded as this host's.
    fn claim(&self) -> std::io::Result<i64> {
        let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&self.file)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let alive = |owner: i32| owner > 0 && (unsafe { libc::kill(owner, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
        let mut taken: BTreeMap<i64, i32> = text
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
            })
            .filter(|&(_, owner)| alive(owner))
            .collect();
        let start = (0..)
            .map(|k| FIRST_PID + k * RANGE)
            .take_while(|&s| s + RANGE <= PID_MAX)
            .find(|s| !taken.contains_key(s))
            .ok_or_else(|| std::io::Error::other("every pid range is taken"))?;
        taken.insert(start, std::process::id() as i32);
        let out: String = taken.iter().map(|(s, o)| format!("{s} {o}\n")).collect();
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(out.as_bytes())?;
        Ok(start)
    }
}

impl Drop for RangePids {
    /// The host is done with its ranges (as it would be by exiting).
    fn drop(&mut self) {
        let ours = std::mem::take(&mut self.lock().ranges);
        let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).open(&self.file) else { return };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return;
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return;
        }
        let me = std::process::id().to_string();
        let kept: String = text
            .lines()
            .filter(|l| {
                let mut f = l.split_whitespace();
                let (start, owner) = (f.next().and_then(|s| s.parse::<i64>().ok()), f.next());
                !(owner == Some(me.as_str()) && start.is_some_and(|s| ours.contains(&s)))
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let _ = file.set_len(0).and_then(|()| file.rewind()).and_then(|()| file.write_all(kept.as_bytes()));
    }
}

impl PidAllocator for RangePids {
    fn alloc(&self) -> Option<i64> {
        let mut s = self.lock();
        // Each pid of each range in turn, so a pid isn't reused soon after
        // it's freed: past the last range, another range if most of this
        // host's are in use, else round to the first again.
        let mut looked = 0;
        loop {
            let (i, off) = s.at;
            if off >= RANGE {
                if i + 1 < s.ranges.len() {
                    s.at = (i + 1, 0);
                } else if s.used.len() as i64 * 4 >= s.ranges.len() as i64 * RANGE * 3
                    && let Ok(start) = self.claim()
                {
                    s.ranges.push(start);
                    s.at = (i + 1, 0);
                } else {
                    s.at = (0, 0);
                }
                continue;
            }
            let pid = s.ranges[i] + off;
            s.at.1 += 1;
            if s.used.insert(pid) {
                return Some(pid);
            }
            looked += 1;
            if looked > s.ranges.len() as i64 * RANGE {
                return None;
            }
        }
    }

    fn free(&self, pid: i64) {
        self.lock().used.remove(&pid);
    }
}

/// The process table of a host.
pub(crate) struct Table {
    state: Mutex<State>,
    pids: Box<dyn PidAllocator>,
    /// The host's parked tasks, and where tasks let run again by a call
    /// that can't place them itself (a SIGCONT, a vfork child's exec or
    /// exit) go, for the next CPU, worker or waiter to put on the run queue.
    parking: Arc<crate::park::Parking>,
    /// The process `macwl-run` was given: the host's exit status is its.
    first: i64,
    first_exit: Mutex<Option<Result<Exit, Error>>>,
    /// Processes that haven't finished: the host ends when none is left.
    live: AtomicUsize,
}

struct State {
    procs: BTreeMap<i64, Entry>,
    /// Threads other than main threads (whose tid is the pid): their
    /// process.
    tids: HashMap<i64, i64>,
}

/// A process in the table.
struct Entry {
    ppid: i64,
    pgid: i64,
    sid: i64,
    kind: Kind,
    children: BTreeSet<i64>,
    /// The signal its parent gets when it ends (SIGCHLD, as a rule).
    exit_signal: u64,
    /// It has exec'd since it was forked (setpgid on it: EACCES).
    execd: bool,
    /// A stop or continue `wait` hasn't reported yet: the wait status.
    report: Option<i32>,
    /// Its threads waiting in `wait4`/`waitid` for a child to change.
    waiters: Vec<Arc<Waker>>,
    /// CPU time of its reaped children (and theirs).
    children_cpu: Times,
    /// `PR_SET_CHILD_SUBREAPER`: orphaned descendants become its children
    /// rather than init's.
    subreaper: bool,
    /// The far ends of its pidfds' host sockets: dropped as it ends, which
    /// makes every pidfd for it read as ready.
    pidfds: Vec<std::os::fd::OwnedFd>,
    /// When it started, in ns since the machine booted.
    start_ns: u64,
}

/// What a table entry is. Steps 6 and 9 add stand-ins: a Mac program
/// standing in for a Linux process, a process in another host.
enum Kind {
    /// A process running in this host.
    Local(Arc<Proc>),
    /// Ended, waiting for its parent's `wait`: its wait status and the CPU
    /// time it and its reaped children used.
    Zombie { status: i32, cpu: Times, comm: Vec<u8> },
}

/// Where a signal goes (see [`Table::deliver`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target {
    Pid(i64),
    Group(i64),
    /// Every process but init and `except`.
    All { except: i64 },
    /// A thread: `tgkill` (with its process) or `tkill`.
    Thread { tgid: Option<i64>, tid: i64 },
}

const SIGKILL: u64 = 9;
const SI_USER: i32 = 0;
const SI_TKILL: i32 = -6;
const SIGCHLD: u64 = 17;
const SIGCONT: u64 = 18;
const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;
const CLD_STOPPED: i32 = 5;
const CLD_CONTINUED: i32 = 6;
/// The wait status of a continue.
const CONTINUED: i32 = 0xffff;

/// The Linux wait status of a process that ended this way.
fn wait_status(exit: &Result<Exit, Error>) -> i32 {
    match exit {
        Ok(Exit::Code(c)) => (c & 0xff) << 8,
        Ok(e) => e.signal().unwrap_or(9),
        // A runtime error ended it: as killed.
        Err(_) => 9,
    }
}

/// `waitid`'s code and status for a wait status.
fn child_info(status: i32) -> (i32, i32) {
    if status == CONTINUED {
        (CLD_CONTINUED, SIGCONT as i32)
    } else if status & 0xff == 0x7f {
        (CLD_STOPPED, status >> 8)
    } else if status & 0x7f == 0 {
        (CLD_EXITED, status >> 8)
    } else if status & 0x80 != 0 {
        (CLD_DUMPED, status & 0x7f)
    } else {
        (CLD_KILLED, status & 0x7f)
    }
}

/// What a wait is for.
#[derive(Debug, Clone, Copy)]
struct Want {
    exited: bool,
    stopped: bool,
    continued: bool,
    nohang: bool,
    /// Leave it to be waited for again (`WNOWAIT`).
    nowait: bool,
}

/// Which children a wait is for.
#[derive(Debug, Clone, Copy)]
enum Which {
    Any,
    Pid(i64),
    /// A group (0: the caller's).
    Group(i64),
}

impl Table {
    fn new(pids: Box<dyn PidAllocator>, first: i64, parking: Arc<crate::park::Parking>) -> Table {
        Table {
            state: Mutex::new(State { procs: BTreeMap::new(), tids: HashMap::new() }),
            pids,
            parking,
            first,
            first_exit: Mutex::new(None),
            live: AtomicUsize::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A process of the host, if it's running here.
    fn local(&self, pid: i64) -> Option<Arc<Proc>> {
        match &self.state().procs.get(&pid)?.kind {
            Kind::Local(p) => Some(p.clone()),
            Kind::Zombie { .. } => None,
        }
    }

    /// A new process `pid`, child of `ppid` (1: init), in the group and
    /// session given.
    fn insert(&self, pid: i64, ppid: i64, pgid: i64, sid: i64, proc: Arc<Proc>, exit_signal: u64) {
        let start_ns = proc.abi.started_ns;
        let mut s = self.state();
        s.procs.insert(
            pid,
            Entry {
                ppid,
                pgid,
                sid,
                kind: Kind::Local(proc),
                children: BTreeSet::new(),
                exit_signal,
                execd: false,
                report: None,
                waiters: Vec::new(),
                children_cpu: Times::default(),
                subreaper: false,
                pidfds: Vec::new(),
                start_ns,
            },
        );
        if let Some(parent) = s.procs.get_mut(&ppid) {
            parent.children.insert(pid);
        }
        self.live.fetch_add(1, Ordering::AcqRel);
    }

    /// A thread id for a new thread of `pid`.
    pub fn new_tid(&self, pid: i64) -> Option<i64> {
        let tid = self.pids.alloc()?;
        self.state().tids.insert(tid, pid);
        Some(tid)
    }

    /// Thread `tid` of `pid` has ended.
    pub fn thread_gone(&self, pid: i64, tid: i64) {
        if tid != pid && self.state().tids.remove(&tid).is_some() {
            self.pids.free(tid);
        }
    }

    /// `proc` has exec'd, from its thread `tid` (which becomes the main
    /// one).
    pub fn exec_done(&self, proc: &Proc, tid: i64) {
        let pid = proc.abi.pid;
        if let Some(e) = self.state().procs.get_mut(&pid) {
            e.execd = true;
        }
        self.thread_gone(pid, tid);
    }

    /// Reap zombie `pid` (its parent waited for it, or doesn't care): its
    /// pid is free again, and its CPU time goes to its parent's children.
    fn reap(&self, s: &mut State, pid: i64) {
        let Some(e) = s.procs.remove(&pid) else { return };
        let Kind::Zombie { cpu, .. } = e.kind else { panic!("reaping a process that runs") };
        if let Some(parent) = s.procs.get_mut(&e.ppid) {
            parent.children.remove(&pid);
            parent.children_cpu += cpu + e.children_cpu;
        }
        self.pids.free(pid);
    }

    /// The one way a signal reaches processes of the host: to a pid, a
    /// group, every process, or a thread, local ones in memory. (Stand-ins
    /// will be told by their own means: a Mac program's with a host `kill`,
    /// step 6; another host's through the coordinator, step 9.)
    pub fn deliver(&self, to: Target, info: SigInfo) -> Result<i64, i64> {
        let targets: Vec<(Arc<Proc>, Option<i64>)> = {
            let s = self.state();
            let local = |e: &Entry| match &e.kind {
                Kind::Local(p) => Some(p.clone()),
                Kind::Zombie { .. } => None,
            };
            match to {
                Target::Pid(1) => return Err(errno::EPERM), // init gets no signals
                Target::Pid(pid) => match s.procs.get(&pid) {
                    // A zombie takes a signal and does nothing with it.
                    Some(e) => local(e).map(|p| (p, None)).into_iter().collect(),
                    None => return Err(errno::ESRCH),
                },
                Target::Group(pgid) => {
                    let members: Vec<&Entry> = s.procs.values().filter(|e| e.pgid == pgid).collect();
                    if members.is_empty() {
                        return Err(errno::ESRCH);
                    }
                    members.into_iter().filter_map(local).map(|p| (p, None)).collect()
                }
                Target::All { except } => {
                    let all: Vec<(Arc<Proc>, Option<i64>)> =
                        s.procs.iter().filter(|&(&pid, _)| pid != except).filter_map(|(_, e)| local(e)).map(|p| (p, None)).collect();
                    if all.is_empty() {
                        return Err(errno::ESRCH);
                    }
                    all
                }
                Target::Thread { tgid, tid } => {
                    let pid = if s.procs.contains_key(&tid) { tid } else { *s.tids.get(&tid).ok_or(errno::ESRCH)? };
                    if tgid.is_some_and(|g| g != pid) {
                        return Err(errno::ESRCH);
                    }
                    let proc = s.procs.get(&pid).and_then(local).ok_or(errno::ESRCH)?;
                    vec![(proc, Some(tid))]
                }
            }
        };
        if info.signo == 0 {
            return Ok(0);
        }
        for (proc, tid) in targets {
            match info.signo {
                // Generating SIGCONT continues a stopped process, whatever
                // its disposition; SIGKILL wakes it, to end.
                SIGCONT => self.resume(&proc, true),
                SIGKILL => self.resume(&proc, false),
                _ => {}
            }
            proc.abi.post(tid, info);
        }
        Ok(0)
    }

    /// A stop signal's default action: `proc` stops (its tasks park as they
    /// come back to the guest), and its parent hears of it.
    fn stop(&self, proc: &Arc<Proc>, sig: u64) {
        if proc.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.child_changed(proc.abi.pid, ((sig as i32) << 8) | 0x7f, CLD_STOPPED, sig as i32);
        // Its threads come out of the guest, and park.
        proc.abi.wake_all_threads();
    }

    /// `proc` continues if it was stopped (`report`: and its parent hears
    /// of it, for SIGCONT).
    fn resume(&self, proc: &Arc<Proc>, report: bool) {
        if !proc.stopped.swap(false, Ordering::AcqRel) {
            return;
        }
        let parked = std::mem::take(&mut proc.parked().stopped);
        self.make_ready(parked);
        if report {
            self.child_changed(proc.abi.pid, CONTINUED, CLD_CONTINUED, SIGCONT as i32);
        }
    }

    /// Child `pid` stopped or continued: `wait` can report `status`, and its
    /// parent gets SIGCHLD (`code`, `si_status`), unless it said not to
    /// (SA_NOCLDSTOP).
    fn child_changed(&self, pid: i64, status: i32, code: i32, si_status: i32) {
        let (parent, waiters) = {
            let mut s = self.state();
            let Some(e) = s.procs.get_mut(&pid) else { return };
            e.report = Some(status);
            let ppid = e.ppid;
            match s.procs.get_mut(&ppid) {
                Some(p) => (local_of(p), std::mem::take(&mut p.waiters)),
                None => (None, Vec::new()),
            }
        };
        for w in waiters {
            w.wake();
        }
        if let Some(parent) = parent
            && parent.abi.child_policy().1
        {
            parent.abi.post(None, SigInfo::child(pid, code, si_status));
        }
    }

    fn make_ready(&self, tasks: Vec<(Task, Start)>) {
        self.parking.make_ready(tasks);
    }

    /// Wait (thread `t` of `p`) for a child in `which` to change as `want`
    /// says: its pid, wait status, and CPU time.
    fn wait(&self, p: &Process, t: &Thread, which: Which, want: Want) -> Result<Option<(i64, i32, Times)>, i64> {
        loop {
            {
                let mut s = self.state();
                let me = s.procs.get(&p.pid).ok_or(errno::ECHILD)?;
                let my_pgid = me.pgid;
                let kids: Vec<i64> = me
                    .children
                    .iter()
                    .copied()
                    .filter(|c| match which {
                        Which::Any => true,
                        Which::Pid(pid) => *c == pid,
                        Which::Group(0) => s.procs.get(c).is_some_and(|e| e.pgid == my_pgid),
                        Which::Group(g) => s.procs.get(c).is_some_and(|e| e.pgid == g),
                    })
                    .collect();
                if kids.is_empty() {
                    return Err(errno::ECHILD);
                }
                for c in kids {
                    let e = s.procs.get_mut(&c).unwrap();
                    let cpu = e.children_cpu
                        + match &e.kind {
                            Kind::Zombie { cpu, .. } => *cpu,
                            Kind::Local(proc) => proc.cpu.times(),
                        };
                    match e.kind {
                        Kind::Zombie { status, .. } if want.exited => {
                            if !want.nowait {
                                self.reap(&mut s, c);
                            }
                            return Ok(Some((c, status, cpu)));
                        }
                        Kind::Local(_) => {
                            let wanted = match e.report {
                                Some(CONTINUED) => want.continued,
                                Some(_) => want.stopped,
                                None => false,
                            };
                            if wanted {
                                let status = e.report.unwrap();
                                if !want.nowait {
                                    e.report = None;
                                }
                                return Ok(Some((c, status, cpu)));
                            }
                        }
                        Kind::Zombie { .. } => {}
                    }
                }
                if want.nohang {
                    return Ok(None);
                }
                let me = s.procs.get_mut(&p.pid).unwrap();
                if !me.waiters.iter().any(|w| Arc::ptr_eq(w, &t.waker)) {
                    me.waiters.push(t.waker.clone());
                }
            }
            // As any blocking call: a signal to act on, or the process
            // ending, ends the wait; the CPU wanted for another task parks
            // it (woken by the child's change, the call is made again).
            if p.signal_ready(t) || p.exiting() {
                return Err(errno::EINTR);
            }
            linux_abi::syscall::wait_woken(t, None)?;
        }
    }

    /// Route a signal that came to the host from the Mac side (a Ctrl-C in
    /// the terminal `macwl-run` was started in, a `kill` of the host): to
    /// the first process's group, as a terminal sends its foreground job;
    /// once the first has gone, to every process left.
    fn from_host(&self, sig: u64) {
        let info = SigInfo::user(sig, SI_USER, 0);
        let group = self.state().procs.get(&self.first).filter(|e| matches!(e.kind, Kind::Local(_))).map(|e| e.pgid);
        let _ = match group {
            Some(pgid) => self.deliver(Target::Group(pgid), info),
            None => self.deliver(Target::All { except: 0 }, info),
        };
    }
}

fn local_of(e: &Entry) -> Option<Arc<Proc>> {
    match &e.kind {
        Kind::Local(p) => Some(p.clone()),
        Kind::Zombie { .. } => None,
    }
}

/// The kernel of every process of a host: the process table.
#[derive(Debug)]
pub(crate) struct TableKernel(Weak<Table>);

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Table")
    }
}

impl TableKernel {
    fn table(&self) -> Result<Arc<Table>, i64> {
        self.0.upgrade().ok_or(errno::ESRCH)
    }
}

impl Kernel for TableKernel {
    fn getppid(&self, p: &Process) -> i64 {
        self.table().ok().and_then(|t| t.state().procs.get(&p.pid).map(|e| e.ppid)).unwrap_or(1)
    }

    fn getpgid(&self, p: &Process, pid: i64) -> Result<i64, i64> {
        let pid = if pid == 0 { p.pid } else { pid };
        self.table()?.state().procs.get(&pid).map(|e| e.pgid).ok_or(errno::ESRCH)
    }

    fn setpgid(&self, p: &Process, pid: i64, pgid: i64) -> Result<i64, i64> {
        if pgid < 0 {
            return Err(errno::EINVAL);
        }
        let table = self.table()?;
        let mut s = table.state();
        let pid = if pid == 0 { p.pid } else { pid };
        let pgid = if pgid == 0 { pid } else { pgid };
        let me = s.procs.get(&p.pid).ok_or(errno::ESRCH)?;
        let my_sid = me.sid;
        let target = s.procs.get(&pid).ok_or(errno::ESRCH)?;
        // Linux's rules: the caller or a child of its, not one that has
        // exec'd, in the caller's session, not a session leader; and the
        // group, if not its own, one in the same session.
        if pid != p.pid {
            if target.ppid != p.pid {
                return Err(errno::ESRCH);
            }
            if target.execd {
                return Err(errno::EACCES);
            }
            if target.sid != my_sid {
                return Err(errno::EPERM);
            }
        }
        if target.sid == pid {
            return Err(errno::EPERM);
        }
        if pgid != pid && !s.procs.values().any(|e| e.pgid == pgid && e.sid == my_sid) {
            return Err(errno::EPERM);
        }
        s.procs.get_mut(&pid).unwrap().pgid = pgid;
        Ok(0)
    }

    fn getsid(&self, p: &Process, pid: i64) -> Result<i64, i64> {
        let pid = if pid == 0 { p.pid } else { pid };
        self.table()?.state().procs.get(&pid).map(|e| e.sid).ok_or(errno::ESRCH)
    }

    fn setsid(&self, p: &Process) -> Result<i64, i64> {
        let table = self.table()?;
        let mut s = table.state();
        // A group leader can't: its group would straddle two sessions.
        if s.procs.values().any(|e| e.pgid == p.pid) {
            return Err(errno::EPERM);
        }
        let me = s.procs.get_mut(&p.pid).ok_or(errno::ESRCH)?;
        me.sid = p.pid;
        me.pgid = p.pid;
        Ok(p.pid)
    }

    fn kill(&self, p: &Process, target: i64, sig: u64) -> Result<i64, i64> {
        let table = self.table()?;
        let to = match target {
            0 => Target::Group(self.getpgid(p, 0)?),
            -1 => Target::All { except: p.pid },
            t if t < 0 => Target::Group(-t),
            t => Target::Pid(t),
        };
        table.deliver(to, SigInfo::user(sig, SI_USER, p.pid))
    }

    fn tgkill(&self, p: &Process, tgid: Option<i64>, tid: i64, sig: u64) -> Result<i64, i64> {
        self.table()?.deliver(Target::Thread { tgid, tid }, SigInfo::user(sig, SI_TKILL, p.pid))
    }

    fn wait4(&self, p: &Process, t: &Thread, pid: i64, options: u64) -> Result<Option<Waited>, i64> {
        const WNOHANG: u64 = 1;
        const WUNTRACED: u64 = 2;
        const WCONTINUED: u64 = 8;
        let which = match pid {
            -1 => Which::Any,
            0 => Which::Group(0),
            p if p < 0 => Which::Group(-p),
            p => Which::Pid(p),
        };
        let want = Want { exited: true, stopped: options & WUNTRACED != 0, continued: options & WCONTINUED != 0, nohang: options & WNOHANG != 0, nowait: false };
        let Some((pid, status, cpu)) = self.table()?.wait(p, t, which, want)? else { return Ok(None) };
        Ok(Some(Waited { pid, status, rusage: rusage_of(cpu, 0) }))
    }

    fn waitid(&self, p: &Process, t: &Thread, idtype: u64, id: u64, options: u64) -> Result<Option<ChildInfo>, i64> {
        const WNOHANG: u64 = 1;
        const WSTOPPED: u64 = 2;
        const WEXITED: u64 = 4;
        const WCONTINUED: u64 = 8;
        const WNOWAIT: u64 = 0x0100_0000;
        let which = match idtype {
            0 => Which::Any,
            1 => Which::Pid(id as i64),
            _ => Which::Group(id as i64),
        };
        let want = Want {
            exited: options & WEXITED != 0,
            stopped: options & WSTOPPED != 0,
            continued: options & WCONTINUED != 0,
            nohang: options & WNOHANG != 0,
            nowait: options & WNOWAIT != 0,
        };
        let Some((pid, status, _)) = self.table()?.wait(p, t, which, want)? else { return Ok(None) };
        let (code, status) = child_info(status);
        Ok(Some(ChildInfo { pid, uid: unsafe { libc::getuid() }, code, status }))
    }

    // Terminals and job control are step 4's: here, a process has no
    // controlling terminal yet.
    fn tcgetpgrp(&self, _p: &Process, _host_fd: std::os::fd::RawFd) -> Result<i64, i64> {
        Err(errno::ENOTTY)
    }

    fn tcsetpgrp(&self, _p: &Process, _host_fd: std::os::fd::RawFd, _pgrp: i64) -> Result<i64, i64> {
        Err(errno::ENOTTY)
    }

    fn stop(&self, p: &Process, sig: u64) {
        if let Ok(table) = self.table()
            && let Some(proc) = table.local(p.pid)
        {
            table.stop(&proc, sig);
        }
    }

    fn rusage(&self, p: &Process, who: i64) -> Result<[u8; RUSAGE_SIZE], i64> {
        let table = self.table()?;
        let s = table.state();
        let e = s.procs.get(&p.pid).ok_or(errno::ESRCH)?;
        let (cpu, rss) = match (who, &e.kind) {
            // RUSAGE_SELF and RUSAGE_THREAD (no per-thread figures yet).
            (0 | 1, Kind::Local(proc)) => (proc.cpu.times(), proc.space().lock().frames_in_use() * 4),
            (-1, _) => (e.children_cpu, 0),
            (0 | 1, Kind::Zombie { .. }) => (Times::default(), 0),
            _ => return Err(errno::EINVAL),
        };
        Ok(rusage_of(cpu, rss))
    }

    fn cpu_clock(&self, p: &Process, user_only: bool) -> CpuClock {
        let cpu = self.table().ok().and_then(|t| t.local(p.pid)).map(|proc| proc.cpu.clone()).unwrap_or_default();
        Box::new(move || {
            let t = cpu.times();
            std::time::Duration::from_nanos(if user_only { t.user } else { t.total() })
        })
    }

    fn umask_changed(&self, _p: &Process, _mask: u32) {
        // Applied by the runtime per process; the host's own stays 0.
    }

    fn pidfd(&self, _p: &Process, pid: i64) -> Result<std::os::fd::OwnedFd, i64> {
        let table = self.table()?;
        let mut s = table.state();
        let e = s.procs.get_mut(&pid).ok_or(errno::ESRCH)?;
        // A socket pair: its far end closes as the process ends, and the
        // near one (the pidfd's) reads as ready from then on.
        let (near, far) = std::os::unix::net::UnixStream::pair().map_err(|e| errno::from_io(&e))?;
        if matches!(e.kind, Kind::Local(_)) {
            e.pidfds.push(far.into());
        }
        Ok(near.into())
    }

    fn process_file(&self, _p: &Process, pid: i64, fd: u64) -> Result<Arc<linux_abi::fs::OpenFile>, i64> {
        // Any process of the session's user may (as ptrace's same-user rule).
        self.table()?.local(pid).ok_or(errno::ESRCH)?.abi.fds.get(fd)
    }

    fn set_child_subreaper(&self, p: &Process, on: bool) -> Result<(), i64> {
        let table = self.table()?;
        table.state().procs.get_mut(&p.pid).ok_or(errno::ESRCH)?.subreaper = on;
        Ok(())
    }

    fn shares_memory(&self) -> bool {
        true
    }

    fn pids(&self, _p: &Process) -> Vec<i64> {
        self.table().map(|t| t.state().procs.keys().copied().collect()).unwrap_or_default()
    }

    fn proc_status(&self, _p: &Process, pid: i64) -> Result<ProcStatus, i64> {
        let table = self.table()?;
        let s = table.state();
        let e = s.procs.get(&pid).ok_or(errno::ESRCH)?;
        let (state, threads, cpu, comm) = match &e.kind {
            Kind::Local(proc) => {
                let state = if proc.stopped.load(Ordering::Acquire) {
                    b'T'
                } else if proc.cpu.in_guest() {
                    b'R'
                } else {
                    b'S'
                };
                (state, proc.abi.thread_count().max(1) as u64, proc.cpu.times(), proc.abi.comm())
            }
            Kind::Zombie { cpu, comm, .. } => (b'Z', 0, *cpu, comm.clone()),
        };
        Ok(ProcStatus {
            comm,
            ppid: e.ppid,
            pgid: e.pgid,
            sid: e.sid,
            state,
            threads,
            user_ns: cpu.user,
            sys_ns: cpu.sys,
            children_user_ns: e.children_cpu.user,
            children_sys_ns: e.children_cpu.sys,
            start_ns: e.start_ns,
        })
    }

    fn with_process(&self, _p: &Process, pid: i64, f: &mut dyn FnMut(&Process, &dyn GuestMemory)) -> Result<(), i64> {
        let proc = self.table()?.local(pid).ok_or(errno::ESRCH)?;
        let space = proc.space.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or(errno::ESRCH)?;
        f(&proc.abi, &*space);
        Ok(())
    }

    fn process_cpu_time(&self, _p: &Process, pid: i64) -> Result<Option<std::time::Duration>, i64> {
        let proc = self.table()?.local(pid).ok_or(errno::EINVAL)?;
        Ok(Some(std::time::Duration::from_nanos(proc.cpu.times().total())))
    }

    fn child_subreaper(&self, p: &Process) -> Result<bool, i64> {
        Ok(self.table()?.state().procs.get(&p.pid).ok_or(errno::ESRCH)?.subreaper)
    }

    fn host_signals(&self) -> bool {
        false
    }

    fn mac_programs(&self) -> bool {
        // The host process never execs (docs/processes.md): Mac programs
        // come back as processes of their own at step 6.
        false
    }
}

/// A Linux `struct rusage` with `cpu`'s user and system time and `rss_kb`
/// of resident memory.
fn rusage_of(cpu: Times, rss_kb: u64) -> [u8; RUSAGE_SIZE] {
    let mut out = [0u8; RUSAGE_SIZE];
    for (at, ns) in [(0, cpu.user), (16, cpu.sys)] {
        out[at..at + 8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
        out[at + 8..at + 16].copy_from_slice(&((ns % 1_000_000_000 / 1000) as i64).to_le_bytes());
    }
    out[32..40].copy_from_slice(&(rss_kb as i64).to_le_bytes()); // ru_maxrss
    out
}

/// The Linux signals a host catches for itself, to route to its processes
/// ([`Table::from_host`]): what a terminal or a `kill` from the Mac side
/// sends. (SIGTSTP and friends stay the host's: a Ctrl-Z stops it whole,
/// as the Mac shell expects of a job, until step 4's relay.)
const FROM_HOST: [u64; 8] = [1, 2, 3, 10, 12, 14, 15, 28];

/// Run `elf` as the first process of a new host, and everything it starts,
/// to the end: the first process's exit, once every process has gone.
pub(crate) fn run(elf: &[u8], argv: &[&[u8]], envp: &[&[u8]], mut process: Process) -> Result<Exit, Error> {
    // A guest writing to a closed pipe or socket gets EPIPE; the host
    // process must not be killed by SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    signal::install_interrupt();
    let pids = RangePids::new()?;
    let pid = pids.alloc().ok_or_else(|| Error::Unexpected("no pid".into()))?;
    let sched = Arc::new(Sched::new());
    let parking = crate::park::Parking::new(sched.clone())?;
    let table = Arc::new(Table::new(Box::new(pids), pid, parking.clone()));
    process.pid = pid;
    process.set_kernel(Arc::new(TableKernel(Arc::downgrade(&table))));
    // Each process's umask is applied as it creates files; the host's own
    // would apply on top.
    let old_umask = unsafe { libc::umask(0) };
    route_host_signals(&table)?;

    profile_mark("load start");
    let machine = Machine::new()?;
    let program = load(&machine, elf, argv, envp, &process)?;
    process.set_arg_area(program.args, program.env);
    let vm = hv::Vm::new()?;
    let space = Arc::new(Space::new(program.space));
    {
        let mut s = space.lock();
        s.take_tlb_stale();
        s.map_into(&vm)?;
    }
    let thread = Thread::main(&process);
    let proc = Proc::new(process, space);
    // A process `macwl-run` starts is init's child, and leads a group and
    // session of its own.
    table.insert(pid, 1, pid, pid, proc.clone(), SIGCHLD);
    let host = Host { vm: &vm, machine: &machine, sched, parking, table: Some(table.clone()), exec_host: Mutex::new(None) };
    std::thread::scope(|scope| {
        linux_abi::host_threads::spawn_scoped(scope, "ticker", || host.sched.ticker()).expect("the host's own threads fit its budget");
        crate::park::run_waiters(&host, scope);
        let task = Task::new(thread, proc.clone());
        host.register(&task);
        // Only its tasks and the table hold it from now on, so that it goes
        // (and its files close) when it ends.
        drop(proc);
        if let Err(e) = run_cpu(&host, scope, (task, Start::Fresh { entry: program.entry, sp: program.sp }), true) {
            *table.first_exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(e));
            host.sched.stop();
        }
    });
    drop(host.parking.take_all());
    let exit = table.first_exit.lock().unwrap_or_else(|e| e.into_inner()).take();
    unsafe { libc::umask(old_umask) };
    exit.unwrap_or(Ok(Exit::Code(0)))
}

/// The host whose processes get the Mac side's signals: the one running
/// now (a test binary runs one after another).
static HOST_TABLE: Mutex<Weak<Table>> = Mutex::new(Weak::new());

/// Signals from the Mac side are the host's, to route ([`Table::from_host`]):
/// caught into an inbox of the host's own, which a thread (one per host
/// process) waits on.
fn route_host_signals(table: &Arc<Table>) -> Result<(), Error> {
    static STARTED: std::sync::OnceLock<Result<(), i64>> = std::sync::OnceLock::new();
    *HOST_TABLE.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(table);
    let started = STARTED.get_or_init(|| {
        let inbox = Arc::new(Inbox::default());
        inbox.receive_host_signals();
        let wait = HostSignalWait::new(&inbox)?;
        linux_abi::host_threads::spawn("host signals", move || {
            // The inbox lives as long as the host process: host signal
            // handlers find it through a raw pointer.
            let _inbox = inbox;
            loop {
                let got = wait.wait();
                let Some(table) = HOST_TABLE.lock().unwrap_or_else(|e| e.into_inner()).upgrade() else { continue };
                for sig in (1..=64).filter(|s| got & (1 << (s - 1)) != 0) {
                    table.from_host(sig);
                }
            }
        })
        .map_err(|_| linux_abi::errno::EAGAIN)?;
        // Only now that the thread (and the inbox it keeps) is there: the
        // handlers reach the inbox through a raw pointer.
        catch_host_signals(&FROM_HOST);
        Ok(())
    });
    started.map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e as i32)))
}

fn profile_mark(what: &str) {
    crate::profile::mark(what);
}

/// `clone` as fork, in the VM: a child process whose address space is a
/// copy-on-write copy of the parent's (or, for vfork, the parent's own),
/// with one task, the caller's registers (`ctx`) with x0 = 0, queued on the
/// host's CPUs. Returns its pid, or a Linux errno.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fork<'scope, 'env>(
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    table: &Arc<Table>,
    vcpu: &mut hv::Vcpu,
    proc: &Arc<Proc>,
    space: &Space,
    thread: &Thread,
    mut ctx: hv::UserContext,
    f: &Fork,
) -> Result<Result<i64, i64>, Error> {
    let Some(pid) = table.pids.alloc() else { return Ok(Err(errno::EAGAIN)) };
    let child_space = if f.vm {
        // The child runs in the parent's memory: until it execs or exits
        // (vfork), or for good (CLONE_VM alone).
        proc.space()
    } else {
        let forked = space.lock().fork();
        // The parent's pages are read-only now: its TLB entries go
        // (everywhere: its other threads run on), before the child runs.
        flush_tlb_now(vcpu)?;
        space.lock().flushed();
        match forked {
            Ok(child) => Arc::new(Space::new(child)),
            Err(_) => {
                table.pids.free(pid);
                return Ok(Err(errno::ENOMEM));
            }
        }
    };
    let (abi, child_thread) = proc.abi.fork_in_host(thread, pid);
    let child = Proc::new(abi, child_space.clone());
    if f.vfork {
        *child.vfork_parent.lock().unwrap_or_else(|e| e.into_inner()) = Some(proc.clone());
    }
    if let Some(addr) = f.parent_tid {
        let _ = GuestMemory::write(space, addr, &(pid as i32).to_le_bytes());
    }
    if let Some(addr) = f.child_tid {
        let _ = GuestMemory::write(&*child_space, addr, &(pid as i32).to_le_bytes());
    }
    let (pgid, sid) = {
        let s = table.state();
        let me = s.procs.get(&proc.abi.pid);
        (me.map_or(pid, |e| e.pgid), me.map_or(pid, |e| e.sid))
    };
    table.insert(pid, proc.abi.pid, pgid, sid, child.clone(), f.exit_signal);
    ctx.x[0] = 0;
    if f.stack != 0 {
        ctx.sp = f.stack;
    }
    if let Some(tls) = f.tls {
        ctx.tpidr = tls;
    }
    let task = Task::new(child_thread, child.clone());
    sh.register(&task);
    drop(child);
    place(sh, scope, task, Start::Resume(Box::new(ctx)));
    Ok(Ok(pid))
}

/// Set a task aside: its process is stopped (until continued), or it waits
/// for its vfork child. It runs on at once if what it would wait for has
/// already happened, or its process is ending.
pub(crate) fn park<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, task: Task, start: Start, why: Why) {
    let proc = task.proc.clone();
    {
        let mut parked = proc.parked();
        // (Ending sets this before it takes what's parked: one of the two
        // sees the other.)
        if !proc.abi.exiting() {
            match why {
                Why::Stopped if proc.stopped.load(Ordering::Acquire) => {
                    parked.stopped.push((task, start));
                    return;
                }
                Why::Stopped => {}
                Why::Vfork(child) => match parked.released.iter().position(|&c| c == child) {
                    Some(i) => {
                        parked.released.swap_remove(i);
                    }
                    None => {
                        parked.vfork.push((child, task, start));
                        return;
                    }
                },
            }
        }
    }
    place(sh, scope, task, start);
}

/// A vfork child has exec'd or ended: its parent's task runs on.
pub(crate) fn vfork_done(sh: &Host, proc: &Proc) {
    let Some(parent) = proc.vfork_parent.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let Some(table) = &sh.table else { return };
    let child = proc.abi.pid;
    let mut parked = parent.parked();
    match parked.vfork.iter().position(|(c, ..)| *c == child) {
        Some(i) => {
            let (_, task, start) = parked.vfork.swap_remove(i);
            drop(parked);
            table.make_ready(vec![(task, start)]);
        }
        None => parked.released.push(child),
    }
}

/// `proc` has ended and its last thread gone: it becomes a zombie for its
/// parent's `wait` (reaped at once if the parent is init, or doesn't care),
/// its children become init's, its parent gets its exit signal, and a vfork
/// parent runs on. The host ends with its last process.
pub(crate) fn finished(sh: &Host, table: &Table, proc: &Arc<Proc>) {
    let pid = proc.abi.pid;
    let exit = proc.exit().take().unwrap_or(Ok(Exit::Code(0)));
    let status = wait_status(&exit);
    if pid == table.first {
        *table.first_exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(exit);
    }
    vfork_done(sh, proc);
    let cpu = proc.cpu.times();
    let (parent, exit_signal, adopted, _pidfds) = {
        let mut s = table.state();
        let Some(e) = s.procs.get_mut(&pid) else { return };
        e.kind = Kind::Zombie { status, cpu, comm: proc.abi.comm() };
        let (ppid, exit_signal) = (e.ppid, e.exit_signal);
        let children = std::mem::take(&mut e.children);
        // Its pidfds read as ready from now on (once the lock is let go).
        let pidfds = std::mem::take(&mut e.pidfds);
        // Orphans go to the nearest ancestor that's a subreaper, else to
        // init, which reaps them (and those already zombies).
        let reaper = {
            let mut a = ppid;
            loop {
                match s.procs.get(&a) {
                    Some(ae) if a != 1 && ae.subreaper && matches!(ae.kind, Kind::Local(_)) => break a,
                    Some(ae) if a != 1 => a = ae.ppid,
                    _ => break 1,
                }
            }
        };
        let mut zombies = Vec::new();
        for c in children {
            let Some(ce) = s.procs.get_mut(&c) else { continue };
            ce.ppid = reaper;
            let zombie = matches!(ce.kind, Kind::Zombie { .. });
            if reaper == 1 {
                if zombie {
                    table.reap(&mut s, c);
                }
            } else {
                s.procs.get_mut(&reaper).unwrap().children.insert(c);
                if zombie {
                    zombies.push(c);
                }
            }
        }
        // A subreaper adopting zombies hears of them as their parent would
        // have (or reaps them at once, if it doesn't care).
        let adopted = match s.procs.get(&reaper).and_then(local_of) {
            Some(r) if !zombies.is_empty() => {
                let reaps = r.abi.child_policy().0;
                let mut told = Vec::new();
                for z in zombies {
                    if reaps {
                        table.reap(&mut s, z);
                    } else if let Some(Entry { kind: Kind::Zombie { status, .. }, exit_signal, .. }) = s.procs.get(&z) {
                        told.push((z, *status, *exit_signal));
                    }
                }
                let waiters = std::mem::take(&mut s.procs.get_mut(&reaper).unwrap().waiters);
                Some((r, told, waiters))
            }
            _ => None,
        };
        let parent = if ppid == 1 { None } else { s.procs.get(&ppid).and_then(local_of) };
        // Reaped at once by init, or by a parent that doesn't care (SIGCHLD
        // ignored, SA_NOCLDWAIT): decided before the table is let go, or the
        // parent's `wait` could find it a zombie in between and report it.
        if parent.as_ref().is_none_or(|p| p.abi.child_policy().0) {
            table.reap(&mut s, pid);
        }
        (parent, exit_signal, adopted, pidfds)
    };
    if let Some((reaper, told, waiters)) = adopted {
        for w in waiters {
            w.wake();
        }
        for (z, status, signo) in told {
            if signo != 0 {
                let (code, si_status) = child_info(status);
                let mut info = SigInfo::child(z, code, si_status);
                info.signo = signo;
                reaper.abi.post(None, info);
            }
        }
    }
    if let Some(parent) = parent {
        let waiters = {
            let mut s = table.state();
            s.procs.get_mut(&parent.abi.pid).map(|p| std::mem::take(&mut p.waiters)).unwrap_or_default()
        };
        for w in waiters {
            w.wake();
        }
        if exit_signal != 0 {
            let (code, si_status) = child_info(status);
            let mut info = SigInfo::child(pid, code, si_status);
            info.signo = exit_signal;
            parent.abi.post(None, info);
        }
    }
    if table.live.fetch_sub(1, Ordering::AcqRel) == 1 {
        // No process left: the host is done.
        sh.sched.stop();
    }
}
