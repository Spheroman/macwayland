//! Many Linux processes in one VM (docs/processes.md, step 3): the only
//! mode since step 8.
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
//! Terminals are the table's too (step 4): each pty's session and
//! foreground group, keyboard signals, SIGTTIN/SIGTTOU and hangups, with
//! Linux's rules; a host started in a Mac terminal relays it (`relay`).
//!
//! A host can also serve clients (step 5, `serve`): `macwl-run`s started
//! from outside that join it, each standing in on the Mac for the process
//! it asked for.
//!
//! Mac programs (step 6, `mac`): the host process never execs. An
//! `execve` of a Mac program spawns it as a process of its own (by an
//! agent), and the Linux process stays in the table as its stand-in
//! ([`Kind::Mac`]): signals to it are host `kill`s of the Mac program, and
//! the agents' reports of it stopping, continuing and ending are its own.
//! Processes in other hosts (step 9) will be another kind of entry; pids
//! come from a [`PidAllocator`] handing out ranges unique across the
//! user's hosts.

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

use crate::mac::{self, Change};

use linux_abi::{
    errno,
    fs::OpenFile,
    syscall::{catch_host_signals, ChildInfo, CpuClock, Fork, HostExec, HostSignalWait, Inbox, Kernel, MacPrograms, ProcStatus, SigInfo, Thread, TtyOp, Waited, Waker, RUSAGE_SIZE},
    GuestMemory, Process,
};

mod fed;
mod serve;
pub use fed::run_adopted;
pub(crate) use serve::{serve, ALREADY_SERVING};

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
    /// The ranges it hands pids out of (start, end), for other hosts to
    /// know which pids are this host's.
    fn ranges(&self) -> Vec<(i64, i64)> {
        Vec::new()
    }
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
        let dir = crate::coord::dir();
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

/// The ranges of the pid file whose hosts are running: each range's start,
/// and its host's Mac pid.
pub(crate) fn range_owners() -> Vec<(i64, i32)> {
    let text = std::fs::read_to_string(crate::coord::dir().join("pids")).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .filter(|&(_, owner): &(i64, i32)| owner > 0 && (unsafe { libc::kill(owner, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)))
        .collect()
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

    fn ranges(&self) -> Vec<(i64, i64)> {
        self.lock().ranges.iter().map(|&s| (s, s + RANGE)).collect()
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
    /// The relay between the Mac terminal `macwl-run` was started in and
    /// the pty its first process has instead, if it was started in one.
    relay: std::sync::OnceLock<Arc<crate::relay::Relay>>,
    /// A host serving clients (`--serve`): what it's told of their
    /// processes, and of the host having no process left.
    service: std::sync::OnceLock<Arc<serve::Notify>>,
    /// The agents that start its Mac programs (`mac`): one per pty that is
    /// a Linux session's terminal, and one (`None`) for sessions without.
    agents: Mutex<HashMap<Option<u32>, Arc<mac::Agent>>>,
    /// Held while a stand-in's reports are handled, so that they're
    /// handled in the order they came.
    mac_order: Mutex<()>,
    /// Its link to the user's coordinator (step 9), which ends with it.
    link: std::sync::OnceLock<crate::coord::Link>,
    /// The host its first process moved here from, where that process's
    /// parent is (step 9).
    parent_peer: std::sync::OnceLock<Arc<fed::Peer>>,
    /// Every host it's linked to: its parent host, and those its
    /// processes moved to (step 9).
    peers: Mutex<Vec<Arc<fed::Peer>>>,
}

/// The most terminals with an agent of their own at once; Mac programs of
/// sessions past it go to the agent without a terminal.
const MAX_TTY_AGENTS: usize = 64;

/// What a process stands in for, once it has exec'd a program that runs
/// elsewhere: a Mac program (docs/processes.md, section 7), or a Linux
/// program in a host of its own (step 9).
#[derive(Debug)]
struct StandIn {
    of: Elsewhere,
    stopped: bool,
    /// Its `/proc` cmdline and its name.
    cmdline: Vec<u8>,
    comm: Vec<u8>,
    /// Reports of it that came before the process's tasks had all gone
    /// (handled once they have).
    pending: Vec<Change>,
    /// Files kept open here while it runs elsewhere (a terminal's master it
    /// took along, which would hang up as its last reference here went).
    _held: Vec<Arc<OpenFile>>,
}

#[derive(Debug)]
enum Elsewhere {
    /// A Mac program: its Mac pid, its Mac process group (its agent's for
    /// the process's Linux group), and the pty its agent has as its
    /// controlling terminal (the Mac driver sends the keyboard's signals to
    /// it itself).
    Mac { pid: libc::pid_t, pgid: libc::pid_t, tty: Option<u32> },
    /// The same process, moved to another host (federated), which reports
    /// it as an agent reports a Mac program.
    Host(Arc<fed::Peer>),
}

impl StandIn {
    /// Its Mac program's pid, if it stands in for one.
    fn mac_pid(&self) -> Option<libc::pid_t> {
        match self.of {
            Elsewhere::Mac { pid, .. } => Some(pid),
            Elsewhere::Host(_) => None,
        }
    }
}

struct State {
    procs: BTreeMap<i64, Entry>,
    /// Threads other than main threads (whose tid is the pid): their
    /// process.
    tids: HashMap<i64, i64>,
    /// The host's terminals (ptys, by number) that sessions have as their
    /// controlling terminal, or that were opened here.
    ttys: HashMap<u32, Tty>,
    /// Mac programs' pids: the processes standing in for them.
    mac_pids: HashMap<libc::pid_t, i64>,
    /// Agents' reports of Mac programs not yet in the table (a spawn's
    /// answer and the program's first change cross).
    early: HashMap<libc::pid_t, Vec<Change>>,
}

/// A terminal's job control: the session it's the controlling terminal
/// of, and that session's foreground process group (Linux's `tty->ctrl`).
#[derive(Debug, Default)]
struct Tty {
    sid: Option<i64>,
    pgrp: Option<i64>,
    /// The last byte written to its master was a VLNEXT, which quotes the
    /// next one (so a Ctrl-V Ctrl-C is a character, not a SIGINT).
    quoted: bool,
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
    /// Its controlling terminal, a pty's number (Linux's `signal->tty`):
    /// inherited across fork, gone with `setsid`.
    ctty: Option<u32>,
    /// A Mac process stands as its parent in its session: the shell that
    /// started `macwl-run` in a terminal (through the relay), which
    /// continues it when it stops. Its group is never orphaned.
    mac_parent: bool,
    /// A client (a `macwl-run` joining a `--serve` host) stands in for it
    /// on the Mac: it hears when the process stops, continues and ends.
    client: bool,
    /// That client's Mac pid, for listings.
    client_pid: Option<libc::pid_t>,
    /// What it stands in for (a Mac program, or itself in another host),
    /// from its spawn until it ends.
    stand: Option<StandIn>,
    /// Its parent is in another host (it moved here: step 9), which hears
    /// of it stopping, continuing and ending, and keeps its zombie.
    remote_parent: bool,
}

/// What a table entry is. Step 9 adds another: a process in another host.
enum Kind {
    /// A process running in this host.
    Local(Arc<Proc>),
    /// A stand-in: the process exec'd a program that runs elsewhere (its
    /// `Entry::stand`), a Mac process of its own or a host of its own; its
    /// tasks and memory have gone. The CPU time it used before.
    StandIn { cpu: Times },
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

const SIGHUP: u64 = 1;
const SIGKILL: u64 = 9;
const SIGSTOP: u64 = 19;
const SIGWINCH: u64 = 28;
const SI_USER: i32 = 0;
/// What the kernel sends (terminals' signals, orphaned groups' hangups).
const SI_KERNEL: i32 = 0x80;
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
            state: Mutex::new(State { procs: BTreeMap::new(), tids: HashMap::new(), ttys: HashMap::new(), mac_pids: HashMap::new(), early: HashMap::new() }),
            pids,
            parking,
            first,
            first_exit: Mutex::new(None),
            live: AtomicUsize::new(0),
            relay: std::sync::OnceLock::new(),
            service: std::sync::OnceLock::new(),
            agents: Mutex::new(HashMap::new()),
            mac_order: Mutex::new(()),
            link: std::sync::OnceLock::new(),
            parent_peer: std::sync::OnceLock::new(),
            peers: Mutex::new(Vec::new()),
        }
    }

    /// Link to the user's coordinator, now or once one runs.
    pub(crate) fn link(self: &Arc<Self>) {
        match crate::coord::Link::start(self) {
            Ok(link) => drop(self.link.set(link)),
            Err(e) => eprintln!("macwl-run: no link to the coordinator: {e}"),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A process of the host, if it's running here.
    fn local(&self, pid: i64) -> Option<Arc<Proc>> {
        local_of(self.state().procs.get(&pid)?)
    }

    /// A new process `pid`, child of `ppid` (1: init), in the group and
    /// session given.
    #[allow(clippy::too_many_arguments)]
    fn insert(&self, pid: i64, ppid: i64, pgid: i64, sid: i64, ctty: Option<u32>, proc: Arc<Proc>, exit_signal: u64) {
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
                ctty,
                mac_parent: false,
                client: false,
                client_pid: None,
                stand: None,
                remote_parent: false,
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
        self.deliver_from(to, info, None, None)
    }

    /// [`Table::deliver`], for a signal pty `keyboard`'s keys raised (if
    /// they did): Mac programs whose agent has that terminal got it from
    /// the Mac driver already. `from`: it came from that linked host,
    /// which isn't told again (step 9).
    fn deliver_from(&self, to: Target, info: SigInfo, keyboard: Option<u32>, from: Option<&Arc<fed::Peer>>) -> Result<i64, i64> {
        // Mac programs standing in for processes: by pid, or their Mac
        // group (which reaches the Mac processes they started too).
        let mut macs: BTreeSet<libc::pid_t> = BTreeSet::new();
        // Processes moved to other hosts, by pid: told there.
        let mut remote: Vec<(Arc<fed::Peer>, i64, Option<i64>)> = Vec::new();
        // Not here: the linked host that has it, asked.
        let mut elsewhere: Option<Target> = None;
        let targets: Vec<(Arc<Proc>, Option<i64>)> = {
            let s = self.state();
            // A process's own (a stand-in's Mac program), or (in a group,
            // `whole`) its Mac group's; a moved process's, by its host (in
            // a group, the group's signal reaches its host anyway).
            let mut local = |pid: i64, e: &Entry, whole: bool| {
                match e.stand.as_ref().map(|m| &m.of) {
                    Some(&Elsewhere::Mac { pid: mac, pgid, tty }) => {
                        if keyboard.is_none() || tty != keyboard {
                            // Never 0 or 1, which kill() would read as "every".
                            macs.insert(if whole && pgid > 1 { -pgid } else { mac });
                        }
                        return None;
                    }
                    Some(Elsewhere::Host(peer)) => {
                        if !whole {
                            remote.push((peer.clone(), pid, None));
                        }
                        return None;
                    }
                    None => {}
                }
                local_of(e)
            };
            match to {
                Target::Pid(1) => return Err(errno::EPERM), // init gets no signals
                Target::Pid(pid) => match s.procs.get(&pid) {
                    // A zombie takes a signal and does nothing with it.
                    Some(e) => local(pid, e, false).map(|p| (p, None)).into_iter().collect(),
                    None => {
                        elsewhere = Some(to);
                        Vec::new()
                    }
                },
                Target::Group(pgid) => {
                    let members: Vec<(&i64, &Entry)> = s.procs.iter().filter(|(_, e)| e.pgid == pgid).collect();
                    if members.is_empty() && !self.reached(pgid, from) {
                        return Err(errno::ESRCH);
                    }
                    members.into_iter().filter_map(|(&pid, e)| local(pid, e, true)).map(|p| (p, None)).collect()
                }
                Target::All { except } => {
                    let mut all = Vec::new();
                    let mut any = false;
                    for (&pid, e) in s.procs.iter().filter(|&(&pid, _)| pid != except) {
                        any = true;
                        all.extend(local(pid, e, true).map(|p| (p, None)));
                    }
                    if !any && !self.has_peers(from) {
                        return Err(errno::ESRCH);
                    }
                    all
                }
                Target::Thread { tgid, tid } => {
                    let pid = if s.procs.contains_key(&tid) { Some(tid) } else { s.tids.get(&tid).copied() };
                    match pid {
                        None => {
                            elsewhere = Some(to);
                            Vec::new()
                        }
                        Some(pid) => {
                            if tgid.is_some_and(|g| g != pid) {
                                return Err(errno::ESRCH);
                            }
                            let e = s.procs.get(&pid).ok_or(errno::ESRCH)?;
                            if e.stand.is_some() {
                                local(pid, e, false);
                                // A moved process's thread: its host knows it.
                                if let Some(r) = remote.last_mut() {
                                    r.2 = Some(tid);
                                }
                                Vec::new()
                            } else {
                                vec![(local_of(e).ok_or(errno::ESRCH)?, Some(tid))]
                            }
                        }
                    }
                }
            }
        };
        // A pid (or tid) of another host's: that host says how it went.
        if let Some(to) = elsewhere {
            return self.route(to, info, from);
        }
        if info.signo == 0 {
            return Ok(0);
        }
        // Signals Darwin has no number for (real-time ones) don't reach a
        // Mac program.
        if let Some(sig) = linux_abi::syscall::signal_to_darwin(info.signo) {
            for target in macs {
                unsafe { libc::kill(target, sig) };
            }
        }
        for (peer, pid, tid) in remote {
            peer.signal(pid, tid, info);
        }
        // A group or everyone: the linked hosts that reach them too.
        match to {
            Target::Group(pgid) => self.fan_out_group(pgid, info, keyboard, from),
            Target::All { except } => self.fan_out_all(except, info, from),
            _ => {}
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
        // The job-control stops do nothing to a process in an orphaned
        // group: no one in its session could continue it (SIGSTOP still
        // stops it).
        if sig != SIGSTOP {
            let s = self.state();
            if let Some(e) = s.procs.get(&proc.abi.pid)
                && orphaned(&s, e.pgid, 0)
            {
                return;
            }
        }
        if proc.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.child_changed(proc.abi.pid, ((sig as i32) << 8) | 0x7f, CLD_STOPPED, sig as i32);
        // Its threads come out of the guest, and park.
        proc.abi.wake_all_threads();
        // The process `macwl-run` was started for stopped (Ctrl-Z): the Mac
        // shell that started it sees its job stop too.
        if proc.abi.pid == self.first
            && let Some(relay) = self.relay.get()
        {
            relay.first_stopped();
        }
        // So does the shell of a client standing in for it.
        self.tell_client(proc.abi.pid, || serve::Event::Stopped(proc.abi.pid, sig));
    }

    /// Tell the client standing in for `pid`, if one does, of what
    /// `event` makes.
    fn tell_client(&self, pid: i64, event: impl FnOnce() -> serve::Event) {
        if let Some(service) = self.service.get()
            && self.state().procs.get(&pid).is_some_and(|e| e.client)
        {
            service.post(event());
        }
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
        self.tell_client(proc.abi.pid, || serve::Event::Continued(proc.abi.pid));
    }

    /// Child `pid` stopped or continued: `wait` can report `status`, and its
    /// parent gets SIGCHLD (`code`, `si_status`), unless it said not to
    /// (SA_NOCLDSTOP).
    fn child_changed(&self, pid: i64, status: i32, code: i32, si_status: i32) {
        // Its parent is in another host: that host hears of it.
        let remote = self.tell_remote_parent(pid, || if status == CONTINUED { Change::Continued } else { Change::Stopped(((status >> 8) & 0xff) as u64) });
        if remote {
            return;
        }
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
                            Kind::Zombie { cpu, .. } | Kind::StandIn { cpu } => *cpu,
                            Kind::Local(proc) => proc.cpu.times(),
                        };
                    match e.kind {
                        Kind::Zombie { status, .. } if want.exited => {
                            if !want.nowait {
                                self.reap(&mut s, c);
                            }
                            return Ok(Some((c, status, cpu)));
                        }
                        Kind::Local(_) | Kind::StandIn { .. } => {
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
        // With a relay, the Mac terminal's resize and hangup are the
        // relay pty's.
        if let Some(relay) = self.relay.get() {
            match sig {
                SIGWINCH => return relay.resize(self),
                SIGHUP => return self.hangup(relay.n),
                _ => {}
            }
        }
        // A serving host has no terminal of its own: its clients' resizes
        // come from them.
        if sig == SIGWINCH && self.service.get().is_some() {
            return;
        }
        let info = SigInfo::user(sig, SI_USER, 0);
        let group = self.state().procs.get(&self.first).filter(|e| matches!(e.kind, Kind::Local(_) | Kind::StandIn { .. })).map(|e| e.pgid);
        let _ = match group {
            Some(pgid) => self.deliver(Target::Group(pgid), info),
            None => self.deliver(Target::All { except: 0 }, info),
        };
    }
}

impl Table {
    /// A signal from the kernel to process group `pgid` (a terminal's, an
    /// orphaned group's hangup); none if the group has gone.
    fn signal_group(&self, pgid: i64, sig: u64) {
        let _ = self.deliver(Target::Group(pgid), SigInfo::user(sig, SI_KERNEL, 0));
    }

    /// Pty `n`'s master has closed (or the Mac terminal its relay stood for
    /// has gone): the terminal hangs up. As Linux's `tty_vhangup`: its
    /// session no longer has it, and the session's leader gets SIGHUP and
    /// SIGCONT (a shell then hangs its jobs up).
    pub fn hangup(&self, n: u32) {
        // Its agent goes (once its Mac programs have).
        self.agents.lock().unwrap_or_else(|e| e.into_inner()).remove(&Some(n));
        let leaders: Vec<i64> = {
            let mut s = self.state();
            let Some(tty) = s.ttys.remove(&n) else { return };
            let mut leaders = Vec::new();
            for (&pid, e) in s.procs.iter_mut() {
                if e.ctty == Some(n) {
                    e.ctty = None;
                }
                if tty.sid == Some(e.sid) && e.sid == pid && matches!(e.kind, Kind::Local(_) | Kind::StandIn { .. }) {
                    leaders.push(pid);
                }
            }
            leaders
        };
        self.advertise();
        for pid in leaders {
            for sig in [SIGHUP, SIGCONT] {
                let _ = self.deliver(Target::Pid(pid), SigInfo::user(sig, SI_KERNEL, 0));
            }
        }
    }

    /// Session leader `pid` gives up its controlling terminal, by
    /// `TIOCNOTTY` (`exiting` false) or by ending: the terminal's foreground
    /// group gets SIGHUP (and SIGCONT, for TIOCNOTTY), and no one in the
    /// session has a controlling terminal any more (Linux's
    /// `disassociate_ctty`).
    fn disassociate(&self, s: &mut State, pid: i64, exiting: bool) -> Vec<(i64, u64)> {
        let Some(n) = s.procs.get(&pid).and_then(|e| e.ctty) else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(tty) = s.ttys.get_mut(&n) {
            if let Some(fg) = tty.pgrp {
                out.push((fg, SIGHUP));
                if !exiting {
                    out.push((fg, SIGCONT));
                }
            }
            tty.sid = None;
            tty.pgrp = None;
        }
        for e in s.procs.values_mut().filter(|e| e.sid == pid) {
            e.ctty = None;
        }
        out
    }

    /// The process group of the session's foreground on pty `n`, and
    /// whether a VLNEXT quotes the next byte.
    fn foreground(&self, n: u32) -> Option<(i64, bool)> {
        let s = self.state();
        let tty = s.ttys.get(&n)?;
        Some((tty.pgrp?, tty.quoted))
    }

    /// `data` typed at pty `n`, whose master is `master`: written, with the
    /// signals its characters raise sent to the foreground group as each is
    /// written. None: no session has the terminal in the foreground, or
    /// ISIG is off, and it's just written.
    pub fn type_keys(&self, n: u32, master: i32, data: &[u8]) -> Option<Result<i64, i64>> {
        let (_, quoted) = self.foreground(n)?;
        let keys = linux_abi::syscall::tty::Keyboard::of(master).ok()?;
        if !keys.isig {
            return None;
        }
        let (r, quoted) = linux_abi::syscall::tty::write_keys(master, data, &keys, quoted, |sig| {
            // The group in the foreground as the byte is read.
            if let Some((fg, _)) = self.foreground(n) {
                let _ = self.deliver_from(Target::Group(fg), SigInfo::user(sig, SI_KERNEL, 0), Some(n), None);
            }
        });
        if let Some(tty) = self.state().ttys.get_mut(&n) {
            tty.quoted = quoted;
        }
        Some(r)
    }

    /// Pty `n` is the controlling terminal of `pid`'s session from now on,
    /// with `pid`'s group in the foreground (Linux's `proc_set_tty`).
    fn set_ctty(s: &mut State, pid: i64, n: u32) {
        let Some(e) = s.procs.get_mut(&pid) else { return };
        e.ctty = Some(n);
        let (sid, pgid) = (e.sid, e.pgid);
        let tty = s.ttys.entry(n).or_default();
        tty.sid = Some(sid);
        tty.pgrp = Some(pgid);
    }

    /// Pty `n` is the first process's controlling terminal (the relay's),
    /// with it as the session leader and its group in the foreground.
    pub fn first_ctty(&self, n: u32) {
        let mut s = self.state();
        Table::set_ctty(&mut s, self.first, n);
        // The Mac shell's job: it stops (Ctrl-Z) and is continued (`fg`) as
        // a child of that shell's would be.
        if let Some(e) = s.procs.get_mut(&self.first) {
            e.mac_parent = true;
        }
    }

    /// Continue the first process's group (the Mac shell continued its
    /// job: `fg`, `bg`).
    pub fn continue_first(&self) {
        let pgid = self.state().procs.get(&self.first).map(|e| e.pgid);
        if let Some(pgid) = pgid {
            self.signal_group(pgid, SIGCONT);
        }
    }

    /// `tcsetpgrp` on pty `n` by a process of session `sid`: group `pgrp`
    /// is its foreground, if `n` is the session's terminal and the group
    /// is one of the session's (here or in a linked host). Another host's
    /// terminal is that host's to change (asked of it, but not of `from`,
    /// which asked).
    pub(super) fn set_foreground(&self, n: u32, pgrp: i64, sid: i64, from: Option<&Arc<fed::Peer>>) -> Result<i64, i64> {
        {
            let mut s = self.state();
            let Some(tty_sid) = s.ttys.get(&n).map(|t| t.sid) else {
                drop(s);
                return match self.remote_tty(n) {
                    Some((owner_sid, _)) if owner_sid == sid && from.is_none() => self.remote_foreground(n, pgrp, sid),
                    _ => Err(errno::ENOTTY),
                };
            };
            if tty_sid != Some(sid) {
                return Err(errno::ENOTTY);
            }
            // The group's session: a member's, else (as Linux) that of the
            // process with that pid; or a linked host's group's.
            let local = s.procs.values().find(|e| e.pgid == pgrp).or_else(|| s.procs.get(&pgrp)).map(|e| e.sid);
            match local {
                Some(g) if g != sid => return Err(errno::EPERM),
                Some(_) => {}
                None if self.reached_in(pgrp, sid) => {}
                None if self.reached(pgrp, None) => return Err(errno::EPERM),
                None => return Err(errno::ESRCH),
            }
            s.ttys.entry(n).or_default().pgrp = Some(pgrp);
        }
        // Linked hosts with members of its session hear of it before the
        // caller goes on (and continues the job, say), on the links its
        // signals take too; so do its Mac programs.
        self.advertise();
        self.follow_tty(n);
        Ok(0)
    }

    /// The foreground group of pty `n`, if a session has it.
    pub fn tty_foreground(&self, n: u32) -> Option<i64> {
        self.foreground(n).map(|(fg, _)| fg)
    }

    /// The Mac terminal pty `n` (whose master is `master`) stands for is
    /// `ws` now: so is the pty, and its foreground group hears of a change.
    pub fn resize_tty(&self, n: u32, master: i32, ws: libc::winsize) {
        let old = linux_abi::syscall::tty::winsize(master);
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
        if old.is_none_or(|o| (o.ws_row, o.ws_col, o.ws_xpixel, o.ws_ypixel) != (ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel))
            && let Some(fg) = self.tty_foreground(n)
        {
            let _ = self.deliver(Target::Group(fg), SigInfo::user(SIGWINCH, SI_KERNEL, 0));
        }
    }
}

impl Table {
    /// The agent that starts Mac programs for a session with pty `tty` as
    /// its terminal (None: without one), started if it isn't yet.
    fn agent_for(self: &Arc<Self>, tty: Option<u32>) -> Result<Arc<mac::Agent>, i64> {
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(agent) = agents.get(&tty) {
            return Ok(agent.clone());
        }
        if tty.is_some() && agents.len() >= MAX_TTY_AGENTS {
            return Err(errno::EAGAIN);
        }
        let weak: Weak<Table> = Arc::downgrade(self);
        let agent = Arc::new(mac::Agent::start(tty, weak).map_err(|e| errno::from_io(&e))?);
        // Its terminal's foreground, to follow from the start (with the
        // agents held: a `tcsetpgrp` meanwhile says so after this).
        if let Some(n) = tty
            && let Some(fg) = self.tty_foreground(n)
        {
            agent.foreground(fg);
        }
        agents.insert(tty, agent.clone());
        Ok(agent)
    }

    /// Pty `n`'s foreground group changed: its agent's Mac foreground
    /// follows.
    fn follow_tty(&self, n: u32) {
        let agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(agent) = agents.get(&Some(n))
            && let Some(fg) = self.tty_foreground(n)
        {
            agent.foreground(fg);
        }
    }

    /// `proc`'s thread with signal mask `blocked` exec'd Mac program `exec`:
    /// start it, as a Mac process of its own, and make `proc` its stand-in
    /// (once its tasks have gone: see [`finished`]). The Linux errno it
    /// couldn't be started with, for `execve` to return.
    pub fn start_mac(self: &Arc<Self>, proc: &Proc, blocked: u64, exec: &HostExec) -> Result<(), i64> {
        let pid = proc.abi.pid;
        let (pgid, ctty) = self.state().procs.get(&pid).map(|e| (e.pgid, e.ctty)).ok_or(errno::ESRCH)?;
        let spawn = linux_abi::syscall::mac_spawn(&proc.abi, blocked, exec)?;
        // Its session's terminal's agent; past the most terminals with one,
        // the agent without a terminal.
        let agent = match ctty {
            Some(n) => self.agent_for(Some(n)).or_else(|_| self.agent_for(None))?,
            None => self.agent_for(None)?,
        };
        let (mac_pid, mac_pgid) = agent.spawn(&spawn, pgid)?;
        drop(spawn);
        let cmdline: Vec<u8> = exec.argv.iter().flat_map(|a| a.iter().copied().chain([0])).collect();
        let name = exec.path.as_bytes().rsplit(|&b| b == b'/').next().unwrap_or_default();
        let comm = name[..name.len().min(15)].to_vec();
        let mut s = self.state();
        let pending = s.early.remove(&mac_pid).unwrap_or_default();
        s.mac_pids.insert(mac_pid, pid);
        if let Some(e) = s.procs.get_mut(&pid) {
            // It has exec'd: setpgid on it is EACCES from now on.
            e.execd = true;
            e.stand = Some(StandIn { of: Elsewhere::Mac { pid: mac_pid, pgid: mac_pgid, tty: agent.tty }, stopped: false, cmdline, comm, pending, _held: Vec::new() });
        }
        Ok(())
    }

    /// What a Mac program's change does to the process standing in for it
    /// (which has become a stand-in: its tasks have gone).
    fn stand_in_event(&self, pid: i64, change: Change) {
        match change {
            Change::Stopped(sig) => {
                let first_time = {
                    let mut s = self.state();
                    match s.procs.get_mut(&pid).and_then(|e| e.stand.as_mut()) {
                        Some(m) => !std::mem::replace(&mut m.stopped, true),
                        None => return,
                    }
                };
                if !first_time {
                    return;
                }
                self.child_changed(pid, ((sig as i32) << 8) | 0x7f, CLD_STOPPED, sig as i32);
                // As a local process's stop: the Mac shell that started the
                // host, or the client standing in for it, sees its job stop.
                if pid == self.first
                    && let Some(relay) = self.relay.get()
                {
                    relay.first_stopped();
                }
                self.tell_client(pid, || serve::Event::Stopped(pid, sig));
            }
            Change::Continued => {
                {
                    let mut s = self.state();
                    match s.procs.get_mut(&pid).and_then(|e| e.stand.as_mut()) {
                        Some(m) => m.stopped = false,
                        None => return,
                    }
                }
                self.child_changed(pid, CONTINUED, CLD_CONTINUED, SIGCONT as i32);
                self.tell_client(pid, || serve::Event::Continued(pid));
            }
            Change::Ended { status, user_ns, sys_ns } => {
                let (cpu, comm) = {
                    let mut s = self.state();
                    let Some(e) = s.procs.get_mut(&pid) else { return };
                    let Some(m) = e.stand.take() else { return };
                    let before = match e.kind {
                        Kind::StandIn { cpu } => cpu,
                        _ => Times::default(),
                    };
                    if let Some(mac) = m.mac_pid() {
                        s.mac_pids.remove(&mac);
                    }
                    (before + Times { user: user_ns, sys: sys_ns }, m.comm)
                };
                let exit = if status & 0x7f == 0 { Exit::Code(status >> 8) } else { Exit::Signal(status & 0x7f) };
                self.tell_client(pid, || serve::Event::Exited(pid, Ok(exit)));
                if pid == self.first {
                    *self.first_exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(exit));
                }
                self.ended(pid, status, cpu, comm);
            }
        }
    }
}

impl Table {
    /// `macwl-run --ps`: the host's processes, as `ps -ef` would show them
    /// inside, and the Mac pid of each one's client.
    pub(crate) fn listing(&self) -> String {
        let rows: Vec<_> = {
            let s = self.state();
            s.procs
                .iter()
                .map(|(&pid, e)| {
                    let (state, cpu, proc, comm) = match &e.kind {
                        Kind::Local(p) => {
                            let state = if p.stopped.load(Ordering::Acquire) {
                                'T'
                            } else if p.cpu.in_guest() {
                                'R'
                            } else {
                                'S'
                            };
                            (state, p.cpu.times(), Some(p.clone()), p.abi.comm())
                        }
                        Kind::Zombie { cpu, comm, .. } => ('Z', *cpu, None, comm.clone()),
                        // A stand-in: what macOS says of its Mac program (a
                        // moved process's host lists it itself).
                        Kind::StandIn { cpu } => {
                            let m = e.stand.as_ref().expect("a stand-in's program");
                            let (user, sys, _) = m.mac_pid().and_then(crate::mac::usage).unwrap_or_default();
                            (if m.stopped { 'T' } else { 'S' }, *cpu + crate::cputime::Times { user, sys }, None, m.comm.clone())
                        }
                    };
                    // What stands for it on the Mac: a Mac program (marked
                    // `*`), or the host it moved to (`@`).
                    let mac = e.stand.as_ref().filter(|_| matches!(e.kind, Kind::StandIn { .. })).map(|m| {
                        let mark = match &m.of {
                            Elsewhere::Mac { pid, .. } => format!("{pid}*"),
                            Elsewhere::Host(peer) => format!("{}@", peer.mac_pid),
                        };
                        (m.mac_pid(), m.cmdline.clone(), mark)
                    });
                    (pid, e.ppid, e.pgid, e.sid, e.ctty, state, cpu, proc, comm, mac, e.client_pid)
                })
                .collect()
        };
        let mut out = format!("{:>7} {:>7} {:>7} {:>7} {:<7} {:<4} {:>8} {:>8} {:>6}  CMD\n", "PID", "PPID", "PGID", "SID", "TTY", "STAT", "TIME", "RSS", "CLIENT");
        for (pid, ppid, pgid, sid, ctty, state, cpu, proc, comm, mac, client_pid) in rows {
            // The command line from the process's memory, as /proc's
            // cmdline; a zombie's name in brackets.
            let (cmd, rss_kb) = match proc.map(|p| (p.abi.arg_area(), p.space())) {
                Some(([a0, a1, ..], space)) => {
                    let mut args = vec![0u8; a1.saturating_sub(a0).min(4096) as usize];
                    let args = if linux_abi::GuestMemory::read(&*space, a0, &mut args).is_ok() { args } else { Vec::new() };
                    let cmd: Vec<u8> = args.strip_suffix(b"\0").unwrap_or(&args).iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
                    let cmd = if cmd.is_empty() { format!("[{}]", String::from_utf8_lossy(&comm)) } else { String::from_utf8_lossy(&cmd).into_owned() };
                    (cmd, linux_abi::GuestMemory::resident_pages(&*space) * 4)
                }
                None => match &mac {
                    Some((mac_pid, cmdline, _)) => {
                        let cmd: Vec<u8> = cmdline.strip_suffix(b"\0").unwrap_or(cmdline).iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
                        (String::from_utf8_lossy(&cmd).into_owned(), mac_pid.and_then(crate::mac::usage).map_or(0, |u| u.2 / 1024))
                    }
                    None => (format!("[{}]", String::from_utf8_lossy(&comm)), 0),
                },
            };
            let secs = cpu.total() / 1_000_000_000;
            let time = format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
            let tty = ctty.map_or_else(|| "?".to_string(), |n| format!("pts/{n}"));
            // The Mac process standing for it: its client's, or the Mac
            // program it stands in for (marked so).
            let client = match (client_pid, &mac) {
                (Some(p), _) => p.to_string(),
                (None, Some((_, _, mark))) => mark.clone(),
                (None, None) => "-".to_string(),
            };
            out.push_str(&format!("{pid:>7} {ppid:>7} {pgid:>7} {sid:>7} {tty:<7} {state:<4} {time:>8} {rss_kb:>8} {client:>6}  {cmd}\n"));
        }
        out
    }
}

impl Table {
    /// What `/proc/<pid>` says of process `pid` of this host's;
    /// `with_cmdline`: its command line too, from its memory (for another
    /// host, which can't read it).
    pub(super) fn status_of(&self, pid: i64, with_cmdline: bool) -> Result<ProcStatus, i64> {
        let s = self.state();
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
            Kind::StandIn { cpu } => {
                let m = e.stand.as_ref().expect("a stand-in's program");
                let (user, sys, _) = m.mac_pid().and_then(mac::usage).unwrap_or_default();
                (if m.stopped { b'T' } else { b'S' }, 1, *cpu + Times { user, sys }, m.comm.clone())
            }
        };
        // A stand-in's command line and memory: its program's.
        let (cmdline, rss_pages) = match (&e.stand, &e.kind) {
            (Some(m), Kind::StandIn { .. }) => (Some(m.cmdline.clone()), m.mac_pid().and_then(mac::usage).map_or(0, |u| u.2 / 4096)),
            (_, Kind::Local(proc)) if with_cmdline => {
                let space = proc.space();
                (Some(cmdline_of(proc, &space)), linux_abi::GuestMemory::resident_pages(&*space))
            }
            _ => (None, 0),
        };
        Ok(ProcStatus {
            cmdline,
            rss_pages,
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
            tty: e.ctty,
            tpgid: e.ctty.and_then(|n| s.ttys.get(&n)).and_then(|t| t.pgrp).unwrap_or(-1),
        })
    }
}

/// A local process's command line, from its memory: `cmdline`'s
/// NUL-separated strings.
fn cmdline_of(proc: &Proc, space: &Space) -> Vec<u8> {
    let [a0, a1, ..] = proc.abi.arg_area();
    let mut args = vec![0u8; a1.saturating_sub(a0).min(4096) as usize];
    if linux_abi::GuestMemory::read(space, a0, &mut args).is_ok() { args } else { Vec::new() }
}

impl mac::MacEvents for Table {
    fn mac_changed(&self, mac_pid: libc::pid_t, change: Change) {
        let _order = self.mac_order.lock().unwrap_or_else(|e| e.into_inner());
        let pid = {
            let mut s = self.state();
            let Some(&pid) = s.mac_pids.get(&mac_pid) else {
                s.early.entry(mac_pid).or_default().push(change);
                return;
            };
            pid
        };
        self.stand_in_report(pid, change);
    }
}

impl Table {
    /// A report of the program process `pid` stands in for, from another
    /// host it moved to (step 9).
    pub(super) fn stand_in_changed(&self, pid: i64, change: Change) {
        let _order = self.mac_order.lock().unwrap_or_else(|e| e.into_inner());
        self.stand_in_report(pid, change);
    }

    /// A report for stand-in `pid`, with the reports' order held: what it
    /// does to the stand-in, or (its tasks still ending) once they have.
    fn stand_in_report(&self, pid: i64, change: Change) {
        {
            let mut s = self.state();
            match s.procs.get_mut(&pid) {
                Some(e) if !matches!(e.kind, Kind::StandIn { .. }) => {
                    if let Some(m) = &mut e.stand {
                        m.pending.push(change);
                    }
                    return;
                }
                Some(_) => {}
                None => return,
            }
        }
        self.stand_in_event(pid, change);
    }
}

/// Whether process group `pgid` is orphaned, not counting process `ignore`
/// (Linux's `will_become_orphaned_pgrp`): none of its members that are
/// running has a parent in another group of the same session, who could
/// continue it if it stopped. Init isn't such a parent; the Mac shell a
/// relayed first process stands in for is.
fn orphaned(s: &State, pgid: i64, ignore: i64) -> bool {
    !s.procs.iter().any(|(&pid, e)| {
        e.pgid == pgid
            && pid != ignore
            && matches!(e.kind, Kind::Local(_) | Kind::StandIn { .. })
            && (e.mac_parent || e.remote_parent || (e.ppid != 1 && s.procs.get(&e.ppid).is_some_and(|parent| parent.pgid != pgid && parent.sid == e.sid)))
    })
}

/// Whether a process of group `pgid` is stopped.
fn has_stopped(s: &State, pgid: i64) -> bool {
    s.procs.values().any(|e| {
        e.pgid == pgid && (matches!(&e.kind, Kind::Local(p) if p.stopped.load(Ordering::Acquire)) || e.stand.as_ref().is_some_and(|m| m.stopped))
    })
}

fn local_of(e: &Entry) -> Option<Arc<Proc>> {
    match &e.kind {
        Kind::Local(p) => Some(p.clone()),
        Kind::Zombie { .. } | Kind::StandIn { .. } => None,
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
        let table = self.table()?;
        let here = table.state().procs.get(&pid).map(|e| e.pgid);
        // Another host's process: as its host says (step 9).
        match here {
            Some(v) => Ok(v),
            None => table.federated_status(pid, None).map(|s| s.pgid),
        }
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
        // (A group in the session may have its members in another host.)
        if pgid != pid && !s.procs.values().any(|e| e.pgid == pgid && e.sid == my_sid) && !table.reached_in(pgid, my_sid) {
            return Err(errno::EPERM);
        }
        let e = s.procs.get_mut(&pid).unwrap();
        e.pgid = pgid;
        let (remote, sid) = (e.remote_parent, e.sid);
        drop(s);
        table.group_changed(pid, remote, pgid, sid);
        Ok(0)
    }

    fn getsid(&self, p: &Process, pid: i64) -> Result<i64, i64> {
        let pid = if pid == 0 { p.pid } else { pid };
        let table = self.table()?;
        let here = table.state().procs.get(&pid).map(|e| e.sid);
        // Another host's process: as its host says (step 9).
        match here {
            Some(v) => Ok(v),
            None => table.federated_status(pid, None).map(|s| s.sid),
        }
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
        // A new session has no controlling terminal.
        me.ctty = None;
        let remote = me.remote_parent;
        drop(s);
        table.group_changed(p.pid, remote, p.pid, p.pid);
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
        let table = self.table()?;
        let Some((pid, status, cpu)) = table.wait(p, t, which, want)? else { return Ok(None) };
        // A group may have gone with the child reaped.
        table.advertise();
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
        let table = self.table()?;
        let Some((pid, status, _)) = table.wait(p, t, which, want)? else { return Ok(None) };
        table.advertise();
        let (code, status) = child_info(status);
        Ok(Some(ChildInfo { pid, uid: unsafe { libc::getuid() }, code, status }))
    }

    // Terminals: the host's ptys are no Mac process's controlling
    // terminal; their sessions and foreground groups are the table's, with
    // Linux's rules (drivers/tty/tty_jobctrl.c).

    fn tcgetpgrp(&self, p: &Process, file: &OpenFile) -> Result<i64, i64> {
        let id = file.tty().ok_or(errno::ENOTTY)?;
        let table = self.table()?;
        let s = table.state();
        // Through the slave, only the caller's own controlling terminal says.
        if !id.master && s.procs.get(&p.pid).and_then(|e| e.ctty) != Some(id.n) {
            return Err(errno::ENOTTY);
        }
        // (Another host's terminal: as that host last said.)
        Ok(s.ttys.get(&id.n).and_then(|t| t.pgrp).or_else(|| table.remote_tty(id.n).and_then(|t| t.1)).unwrap_or(0))
    }

    fn tcsetpgrp(&self, p: &Process, t: &Thread, file: &OpenFile, pgrp: i64) -> Result<i64, i64> {
        let id = file.tty().ok_or(errno::ENOTTY)?;
        // From the background: SIGTTOU (bash blocks it in a child, which
        // then goes ahead).
        self.tty_access(p, t, file, TtyOp::Change).map_err(|e| if e == errno::EIO { errno::ENOTTY } else { e })?;
        let table = self.table()?;
        let my_sid = {
            let s = table.state();
            let me = s.procs.get(&p.pid).ok_or(errno::ESRCH)?;
            if me.ctty != Some(id.n) {
                return Err(errno::ENOTTY);
            }
            me.sid
        };
        if pgrp < 0 {
            return Err(errno::EINVAL);
        }
        table.set_foreground(id.n, pgrp, my_sid, None)
    }

    fn tcgetsid(&self, p: &Process, file: &OpenFile) -> Result<i64, i64> {
        let id = file.tty().ok_or(errno::ENOTTY)?;
        let table = self.table()?;
        let s = table.state();
        if !id.master && s.procs.get(&p.pid).and_then(|e| e.ctty) != Some(id.n) {
            return Err(errno::ENOTTY);
        }
        s.ttys.get(&id.n).and_then(|t| t.sid).or_else(|| table.remote_tty(id.n).map(|t| t.0)).ok_or(errno::ENOTTY)
    }

    fn set_ctty(&self, p: &Process, file: &OpenFile, _steal: bool) -> Result<i64, i64> {
        let id = file.tty().ok_or(errno::ENOTTY)?;
        let table = self.table()?;
        let mut s = table.state();
        let me = s.procs.get(&p.pid).ok_or(errno::ESRCH)?;
        let (leader, sid, ctty) = (me.sid == p.pid, me.sid, me.ctty);
        let owner = s.ttys.get(&id.n).and_then(|t| t.sid);
        // Already this session's.
        if leader && owner == Some(sid) {
            return Ok(0);
        }
        // A session leader without one; and not another session's (taking
        // it needs CAP_SYS_ADMIN, which no process here has).
        if !leader || ctty.is_some() || owner.is_some() {
            return Err(errno::EPERM);
        }
        if !readable(file) {
            return Err(errno::EPERM);
        }
        Table::set_ctty(&mut s, p.pid, id.n);
        drop(s);
        table.advertise();
        Ok(0)
    }

    fn drop_ctty(&self, p: &Process, file: &OpenFile) -> Result<i64, i64> {
        let id = file.tty().ok_or(errno::ENOTTY)?;
        let table = self.table()?;
        let signals = {
            let mut s = table.state();
            let me = s.procs.get(&p.pid).ok_or(errno::ESRCH)?;
            if id.master || me.ctty != Some(id.n) {
                return Err(errno::ENOTTY);
            }
            let signals = if me.sid == p.pid { table.disassociate(&mut s, p.pid, false) } else { Vec::new() };
            if let Some(me) = s.procs.get_mut(&p.pid) {
                me.ctty = None;
            }
            signals
        };
        table.advertise();
        for (pgid, sig) in signals {
            table.signal_group(pgid, sig);
        }
        Ok(0)
    }

    fn tty_opened(&self, p: &Process, file: &OpenFile, noctty: bool) {
        let Some(id) = file.tty() else { return };
        let Ok(table) = self.table() else { return };
        if id.master {
            // A new pty (its number may be an old one's): its hangup comes
            // as the last reference to its master goes.
            table.state().ttys.remove(&id.n);
            table.agents.lock().unwrap_or_else(|e| e.into_inner()).remove(&Some(id.n));
            let weak = Arc::downgrade(&table);
            file.on_close(Box::new(move || {
                if let Some(table) = weak.upgrade() {
                    table.hangup(id.n);
                }
            }));
            return;
        }
        // A session leader without a controlling terminal opening one no
        // session has, for reading, without O_NOCTTY, gets it.
        let mut s = table.state();
        let Some(me) = s.procs.get(&p.pid) else { return };
        let owned = s.ttys.get(&id.n).is_some_and(|t| t.sid.is_some());
        if !noctty && me.sid == p.pid && me.ctty.is_none() && !owned && readable(file) {
            Table::set_ctty(&mut s, p.pid, id.n);
        }
    }

    fn keeps_ttys(&self) -> bool {
        true
    }

    fn ctty_path(&self, p: &Process) -> Option<Result<Vec<u8>, i64>> {
        let ctty = self.table().ok().and_then(|t| t.state().procs.get(&p.pid).and_then(|e| e.ctty));
        Some(ctty.map(|n| format!("/dev/ttys{n:03}").into_bytes()).ok_or(errno::ENXIO))
    }

    fn tty_access(&self, p: &Process, t: &Thread, file: &OpenFile, op: TtyOp) -> Result<bool, i64> {
        let Some(id) = file.tty() else { return Ok(true) };
        let table = self.table()?;
        // Only a process outside the foreground group of the terminal that
        // is its controlling one.
        let pgid = {
            let s = table.state();
            let Some(me) = s.procs.get(&p.pid) else { return Ok(true) };
            if me.ctty != Some(id.n) {
                return Ok(true);
            }
            // (Another host's terminal: as that host last said.)
            let fg = match s.ttys.get(&id.n) {
                Some(t) => t.pgrp,
                None => table.remote_tty(id.n).and_then(|t| t.1),
            };
            match fg {
                Some(fg) if fg != me.pgid => me.pgid,
                _ => return Ok(true),
            }
        };
        let tostop = op == TtyOp::Write && linux_abi::syscall::tty::tostop(file.host.as_raw_fd());
        if op == TtyOp::Write && !tostop {
            return Ok(true);
        }
        let sig = if op == TtyOp::Read { linux_abi::syscall::tty::SIGTTIN } else { linux_abi::syscall::tty::SIGTTOU };
        let ignored = p.blocks_or_ignores(t, sig);
        let orphaned = orphaned(&table.state(), pgid, 0);
        match linux_abi::syscall::tty::background(op, tostop, ignored, orphaned)? {
            None => Ok(true),
            // Its whole group stops; the call is made again once it's
            // continued (or the signal's handler has run, with SA_RESTART).
            Some(sig) => {
                table.signal_group(pgid, sig);
                Err(errno::EINTR)
            }
        }
    }

    fn write_master(&self, _p: &Process, file: &OpenFile, data: &[u8]) -> Option<Result<i64, i64>> {
        let id = file.tty()?;
        self.table().ok()?.type_keys(id.n, file.host.as_raw_fd(), data)
    }

    fn winsize_changed(&self, _p: &Process, file: &OpenFile) {
        let Some(id) = file.tty() else { return };
        if let Ok(table) = self.table()
            && let Some(fg) = table.tty_foreground(id.n)
        {
            table.signal_group(fg, SIGWINCH);
        }
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
            (0 | 1, Kind::StandIn { cpu }) => (*cpu, 0),
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
        if matches!(e.kind, Kind::Local(_) | Kind::StandIn { .. }) {
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
        // Every host's linked to this one too (step 9).
        self.table().map(|t| t.federated_pids(None).into_iter().collect()).unwrap_or_default()
    }

    fn proc_status(&self, _p: &Process, pid: i64) -> Result<ProcStatus, i64> {
        let table = self.table()?;
        let local = table.state().procs.get(&pid).map(|e| !matches!(&e.stand, Some(StandIn { of: Elsewhere::Host(_), .. })));
        match local {
            Some(true) => table.status_of(pid, false),
            // Another host's (a moved process's is its host's to say).
            _ => table.federated_status(pid, None),
        }
    }

    fn with_process(&self, _p: &Process, pid: i64, f: &mut dyn FnMut(&Process, &dyn GuestMemory)) -> Result<(), i64> {
        let proc = self.table()?.local(pid).ok_or(errno::ESRCH)?;
        let space = proc.space();
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

    fn mac_programs(&self) -> MacPrograms {
        // The host process never execs (docs/processes.md): a Mac program
        // is a process of its own, which this one stands in for.
        if linux_abi::syscall::mac_programs_allowed() { MacPrograms::StandIn } else { MacPrograms::Refused }
    }
}

/// Whether `file` was opened for reading (Linux gives a controlling
/// terminal only through such a file).
fn readable(file: &OpenFile) -> bool {
    let fl = unsafe { libc::fcntl(file.host.as_raw_fd(), libc::F_GETFL) };
    fl >= 0 && fl & libc::O_ACCMODE != libc::O_WRONLY
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
/// sends. With a relay, Ctrl-C and Ctrl-Z are bytes on the relay's pty and
/// SIGWINCH and SIGHUP are its resize and hangup. (A SIGTSTP from a Mac
/// `kill` stays the host's: it stops whole, as a Mac job.)
const FROM_HOST: [u64; 8] = [1, 2, 3, 10, 12, 14, 15, 28];

/// Run `elf` as the first process of a new host, and everything it starts,
/// to the end: the first process's exit, once every process has gone.
pub(crate) fn run(elf: &[u8], argv: &[&[u8]], envp: &[&[u8]], process: Process) -> Result<Exit, Error> {
    let pids = RangePids::new()?;
    let pid = pids.alloc().ok_or_else(|| Error::Unexpected("no pid".into()))?;
    let thread = Thread::main(&process);
    run_first(Box::new(pids), pid, elf, argv, envp, process, thread, First::Started)
}

/// How a host's first process came to it.
pub(crate) enum First {
    /// `macwl-run` started it: init's child, leading a group and session of
    /// its own, with the Mac terminal it was started in relayed to it.
    Started,
    /// It moved here from another host as it exec'd (step 9): its parent,
    /// group, session and terminal are the ones it had there, its parent
    /// is in that host (`peer`), and `loaded` is told whether its program
    /// could be loaded (an errno for its `execve`).
    Adopted { ppid: i64, pgid: i64, sid: i64, ctty: Option<u32>, peer: Arc<fed::Peer>, link: std::os::unix::net::UnixStream, loaded: Box<dyn FnOnce(Result<(), i64>)> },
}

/// Run `elf` as the host's first process, `pid`, with `thread` its first
/// thread, until every process has gone: its exit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_first(
    pids: Box<dyn PidAllocator>,
    pid: i64,
    elf: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    mut process: Process,
    mut thread: Thread,
    first: First,
) -> Result<Exit, Error> {
    // Its first thread's tid is its pid.
    thread.tid = pid;
    // A guest writing to a closed pipe or socket gets EPIPE; the host
    // process must not be killed by SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    signal::install_interrupt();
    let sched = Arc::new(Sched::new());
    let parking = crate::park::Parking::new(sched.clone())?;
    let table = Arc::new(Table::new(pids, pid, parking.clone()));
    process.pid = pid;
    process.set_kernel(Arc::new(TableKernel(Arc::downgrade(&table))));
    // Each process's umask is applied as it creates files; the host's own
    // would apply on top.
    let old_umask = unsafe { libc::umask(0) };
    route_host_signals(&table)?;
    table.link();
    // Started in a Mac terminal: the first process's terminal is a pty of
    // the host's, relayed to it.
    let relay = match first {
        First::Started => crate::relay::Relay::new(&process).map(Arc::new),
        First::Adopted { .. } => None,
    };

    profile_mark("load start");
    let machine = Machine::new()?;
    let loaded = load(&machine, elf, argv, envp, &process);
    let (insert, adopted) = match first {
        First::Started => ((1, pid, pid, None), None),
        First::Adopted { ppid, pgid, sid, ctty, peer, link, loaded: tell } => {
            tell(loaded.as_ref().map(|_| ()).map_err(crate::exec_errno));
            ((ppid, pgid, sid, ctty), Some((peer, link)))
        }
    };
    let program = loaded?;
    process.set_arg_area(program.args, program.env);
    let vm = hv::Vm::new()?;
    let space = Arc::new(Space::new(program.space));
    {
        let mut s = space.lock();
        s.take_tlb_stale();
        s.map_into(&vm)?;
    }
    let proc = Proc::new(process, space);
    // A process `macwl-run` starts is init's child, and leads a group and
    // session of its own; one that moved here keeps its own.
    let (ppid, pgid, sid, ctty) = insert;
    table.insert(pid, ppid, pgid, sid, ctty, proc.clone(), SIGCHLD);
    if let Some((peer, link)) = adopted {
        table.adopted(pid, peer, link);
    }
    if let Some(relay) = &relay {
        table.first_ctty(relay.n);
        let _ = table.relay.set(relay.clone());
        relay.start(&table);
    }
    let host = Host { vm: &vm, machine: &machine, sched, parking, table: table.clone() };
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
    if let Some(relay) = relay {
        relay.finish();
    }
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
    let (pgid, sid, ctty) = {
        let s = table.state();
        let me = s.procs.get(&proc.abi.pid);
        (me.map_or(pid, |e| e.pgid), me.map_or(pid, |e| e.sid), me.and_then(|e| e.ctty))
    };
    table.insert(pid, proc.abi.pid, pgid, sid, ctty, child.clone(), f.exit_signal);
    // Its pid is one linked hosts reach now.
    table.advertise();
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
    let table = &sh.table;
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
    // It exec'd a Mac program: it stands in for it from now on, its tasks
    // and memory gone, its entry kept.
    {
        let order = table.mac_order.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = table.state();
        if let Some(e) = s.procs.get_mut(&pid)
            && let Some(m) = &mut e.stand
        {
            let pending = std::mem::take(&mut m.pending);
            let was = std::mem::replace(&mut e.kind, Kind::StandIn { cpu: proc.cpu.times() });
            drop(s);
            // (Its files close as it goes: not with the table held.)
            drop(was);
            vfork_done(sh, proc);
            for change in pending {
                table.stand_in_event(pid, change);
            }
            drop(order);
            return;
        }
    }
    let exit = proc.exit().take().unwrap_or(Ok(Exit::Code(0)));
    let status = wait_status(&exit);
    // Its client, if it has one, exits as it did.
    table.tell_client(pid, || serve::Event::Exited(pid, exit.as_ref().map(|e| *e).map_err(|e| e.to_string())));
    if pid == table.first {
        *table.first_exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(exit);
    }
    vfork_done(sh, proc);
    table.ended(pid, status, proc.cpu.times(), proc.abi.comm());
}

impl Table {
    /// Process `pid` has ended, with wait status `status`, having used `cpu`:
    /// it becomes a zombie for its parent's `wait` (reaped at once if the
    /// parent is init, or doesn't care), its children become init's (or a
    /// subreaper's), and its parent gets its exit signal. The host ends with
    /// its last process.
    fn ended(&self, pid: i64, status: i32, cpu: Times, comm: Vec<u8>) {
    let table = self;
    // Its parent is in another host: that host keeps its zombie (it's
    // reaped here, below, having no parent here).
    self.tell_remote_parent(pid, || Change::Ended { status, user_ns: cpu.user, sys_ns: cpu.sys });
    let (parent, exit_signal, adopted, _pidfds, _was, hangups) = {
        let mut s = table.state();
        let Some(e) = s.procs.get_mut(&pid) else { return };
        let leader = e.sid == pid;
        // (What it was goes once the table is let go: its files may close
        // with it, and a pty master's hangup takes the table.)
        let was = std::mem::replace(&mut e.kind, Kind::Zombie { status, cpu, comm });
        let (ppid, exit_signal, my_pgid, my_sid) = (e.ppid, e.exit_signal, e.pgid, e.sid);
        let children = std::mem::take(&mut e.children);
        // Its pidfds read as ready from now on (once the lock is let go).
        let pidfds = std::mem::take(&mut e.pidfds);
        // A session leader ending: its terminal's foreground group gets
        // SIGHUP, and the session has no terminal any more.
        let mut hangups = if leader { table.disassociate(&mut s, pid, true) } else { Vec::new() };
        // Children in other groups of the session, which it may have been
        // the last link to (checked once they're reparented).
        let linked: Vec<i64> = children
            .iter()
            .filter_map(|c| s.procs.get(c))
            .filter(|ce| matches!(ce.kind, Kind::Local(_) | Kind::StandIn { .. }) && ce.pgid != my_pgid && ce.sid == my_sid)
            .map(|ce| ce.pgid)
            .collect();
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
        let mut moved = Vec::new();
        for c in children {
            let Some(ce) = s.procs.get_mut(&c) else { continue };
            ce.ppid = reaper;
            // One running in another host hears of its new parent there.
            if let Some(StandIn { of: Elsewhere::Host(peer), .. }) = &ce.stand {
                moved.push((peer.clone(), c));
            }
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
        // A group this leaves orphaned, with a stopped process in it, would
        // stay stopped for good: SIGHUP and SIGCONT to it (Linux's
        // `kill_orphaned_pgrp`), for its children's groups and its own.
        let mut groups: Vec<i64> = linked;
        if ppid != 1 && s.procs.get(&ppid).is_some_and(|pe| pe.pgid != my_pgid && pe.sid == my_sid) {
            groups.push(my_pgid);
        }
        groups.sort_unstable();
        groups.dedup();
        for g in groups {
            if orphaned(&s, g, pid) && has_stopped(&s, g) {
                hangups.extend([(g, SIGHUP), (g, SIGCONT)]);
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
        (parent, exit_signal, adopted, pidfds, was, (hangups, moved, reaper))
    };
    let (hangups, moved, reaper) = hangups;
    for (peer, c) in moved {
        peer.reparent(c, reaper);
    }
    for (pgid, sig) in hangups {
        table.signal_group(pgid, sig);
    }
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
    // A group may have gone with it.
    table.advertise();
    if table.live.fetch_sub(1, Ordering::AcqRel) == 1 {
        match table.service.get() {
            // A serving host waits a little for more clients first.
            Some(service) => service.post(serve::Event::NoProcesses),
            // No process left: the host is done.
            None => drop(table.parking.sched().stop()),
        }
    }
    }
}
