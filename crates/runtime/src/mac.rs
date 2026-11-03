//! Mac programs as stand-ins (docs/processes.md, section 7, step 6).
//!
//! With many Linux processes in one host, an `execve` of a Mac program
//! can't replace the process: the host process is every Linux process of
//! its session, and it never execs. So the Mac program runs as a process
//! of its own, and the Linux process stays in the table as its stand-in
//! (`host.rs`): its pid, parent, group and session, its parent's `wait`,
//! signals to it and its group, job control.
//!
//! Mac programs are spawned by **agents** (`macwl-run --tty-agent`): small
//! helpers that never make a VM, each a Mac session of its own. A Linux
//! session with a terminal (a pty of the host's) has one whose controlling
//! terminal is that pty, so that its Mac programs have it as theirs
//! (`less` reading keys from `/dev/tty`, a background `vim` stopped by the
//! driver as it touches the terminal). An agent puts the Mac programs of
//! each Linux process group in a Mac process group of their own, and makes
//! the Mac foreground group follow the Linux one: the Mac group of the
//! Linux foreground group if it has Mac programs, else the agent's own
//! (which ignores the keyboard's signals). Sessions without a terminal
//! share one agent with none. Agents are the Mac programs' parent: each
//! `waitpid`s its children (WUNTRACED, WCONTINUED) and tells the host how
//! they changed, and the host's reaper thread makes the stand-ins stop,
//! continue and end.
//!
//! Why agents (the experiment of step 6): `posix_spawn` with
//! `POSIX_SPAWN_SETSID` and the pty's slave opened in its file actions does
//! give one Mac program the terminal, but a second Mac program of the same
//! Linux session (the other half of `/bin/sleep | tr`, `git` and the `less`
//! it starts are fine) can't have it while the first's session does, and
//! no process outside a Mac session can set its foreground group (ENOTTY,
//! through the slave or the master): only a process in the session can
//! make the Mac foreground follow `fg` and `bg`.
//!
//! Everything here starts processes with [`spawn`], which never replaces
//! the calling process: it refuses `POSIX_SPAWN_SETEXEC`.

use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::ExitCode,
    sync::{Mutex, OnceLock, Weak},
};

use linux_abi::{
    errno,
    syscall::MacSpawn,
    wire::{Decoder, Encoder, Truncated},
};

use crate::proto;

/// `posix_spawn` flags macOS has that the libc crate doesn't name.
const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;
const POSIX_SPAWN_START_SUSPENDED: libc::c_short = 0x0080;

/// The spawn flags given, if they start a new process: never
/// `POSIX_SPAWN_SETEXEC`, which would replace the caller with the program.
/// Fails an assertion otherwise.
pub(crate) fn new_process_flags(flags: libc::c_short) -> libc::c_short {
    assert!(flags & libc::POSIX_SPAWN_SETEXEC as libc::c_short == 0, "a spawn here starts a new process: never POSIX_SPAWN_SETEXEC");
    flags
}

/// Start `path` as a new process, with `flags` (never
/// `POSIX_SPAWN_SETEXEC`: see [`new_process_flags`]) and whatever `setup`
/// adds to the file actions and attributes. Its pid, or the errno
/// `posix_spawn` gave.
pub(crate) fn spawn(
    path: &CStr,
    argv: &[CString],
    env: &[CString],
    flags: libc::c_short,
    setup: impl FnOnce(&mut libc::posix_spawn_file_actions_t, &mut libc::posix_spawnattr_t),
) -> Result<libc::pid_t, i32> {
    let flags = new_process_flags(flags);
    let ptrs = |v: &[CString]| v.iter().map(|s| s.as_ptr() as *mut libc::c_char).chain([std::ptr::null_mut()]).collect::<Vec<_>>();
    let (argv, env) = (ptrs(argv), ptrs(env));
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawnattr_init(&mut attr);
        setup(&mut actions, &mut attr);
        libc::posix_spawnattr_setflags(&mut attr, flags);
        // What it's about to be given, checked again: nothing in `setup`
        // may have asked for an exec in place.
        let mut set: libc::c_short = 0;
        libc::posix_spawnattr_getflags(&attr, &mut set);
        new_process_flags(set);
        let mut pid = 0;
        let r = libc::posix_spawn(&mut pid, path.as_ptr(), &actions, &attr, argv.as_ptr(), env.as_ptr());
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        if r != 0 { Err(r) } else { Ok(pid) }
    }
}

// Darwin's `posix_spawn_file_actions_addchdir_np` (macOS 10.15), which the
// libc crate doesn't declare.
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(actions: *mut libc::posix_spawn_file_actions_t, path: *const libc::c_char) -> libc::c_int;
}

/// Host to agent: start a Mac program (body: see [`encode_spawn`]; its fds
/// come with it and in [`FDS`] messages after it).
const SPAWN: u8 = 1;
/// Host to agent: more of a spawn's fds.
const FDS: u8 = 2;
/// Host to agent: this Linux process group is its terminal's foreground.
const FOREGROUND: u8 = 3;
/// Agent to host: the spawn's result (the Mac pid and group, or a Darwin
/// errno).
const SPAWNED: u8 = 16;
/// Agent to host: done (a foreground change).
const DONE: u8 = 17;
/// Agent to host, on the events socket: a Mac program changed (stopped,
/// continued, ended): its pid, Darwin wait status, and CPU time if it ended.
const CHANGED: u8 = 32;

/// Darwin's `WCONTINUED` status, and how `wait` statuses read.
fn stopped(status: i32) -> Option<i32> {
    (status & 0xff == 0x7f && status >> 8 != 0x13).then_some((status >> 8) & 0xff)
}
fn continued(status: i32) -> bool {
    status & 0xff == 0x7f && status >> 8 == 0x13
}

/// The Linux wait status for a Mac program's Darwin one (signal numbers
/// translated): exited, killed (with the core flag), or stopped.
pub(crate) fn linux_status(status: i32) -> i32 {
    let sig = |s: i32| linux_abi::syscall::signal_from_darwin(s) as i32;
    if let Some(s) = stopped(status) {
        (sig(s) << 8) | 0x7f
    } else if status & 0x7f == 0 {
        status & 0xff00
    } else {
        sig(status & 0x7f) | (status & 0x80)
    }
}

/// The Darwin wait-status kind of a change: stopped (with the Linux signal),
/// continued, or ended (the Linux wait status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Stopped(u64),
    Continued,
    Ended { status: i32, user_ns: u64, sys_ns: u64 },
}

impl Change {
    fn of(status: i32, user_ns: u64, sys_ns: u64) -> Change {
        if continued(status) {
            Change::Continued
        } else if let Some(s) = stopped(status) {
            Change::Stopped(linux_abi::syscall::signal_from_darwin(s))
        } else {
            Change::Ended { status: linux_status(status), user_ns, sys_ns }
        }
    }
}

/// What hears of Mac programs changing: the host's table.
pub(crate) trait MacEvents: Send + Sync {
    fn mac_changed(&self, mac_pid: i32, change: Change);
}

/// The Linux errno for a Darwin one a spawn gave: a binary macOS won't run
/// (wrong architecture, a bad Mach-O) is ENOEXEC, as Linux says of one it
/// can't.
fn spawn_errno(e: i32) -> i64 {
    const EBADEXEC: i32 = 85;
    const EBADARCH: i32 = 86;
    const EBADMACHO: i32 = 88;
    match e {
        EBADEXEC | EBADARCH | EBADMACHO => errno::ENOEXEC,
        e => errno::from_io(&std::io::Error::from_raw_os_error(e)),
    }
}

/// The `macwl-run` binary, to start agents with: this one, or (a test
/// binary running a host in-process) the one built beside it.
fn macwl_run_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.file_name().is_some_and(|n| n == "macwl-run") {
        return Ok(exe);
    }
    let dir = exe.parent().ok_or(std::io::ErrorKind::NotFound)?;
    [dir.join("macwl-run"), dir.join("../macwl-run")].into_iter().find(|p| p.is_file()).ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

/// An agent, from the host's side: the socket it takes requests on.
pub(crate) struct Agent {
    pub pid: libc::pid_t,
    /// The pty it has as its controlling terminal (None: the host's agent
    /// for sessions without one).
    pub tty: Option<u32>,
    requests: Mutex<(OwnedFd, proto::Reader)>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Agent({}, {:?})", self.pid, self.tty)
    }
}

/// A socket pair, both ends close-on-exec.
fn socket_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&a, &b] {
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((a, b))
}

impl Agent {
    /// Start an agent for pty `tty` (or for sessions without a terminal),
    /// whose Mac programs' changes go to `events`.
    pub fn start(tty: Option<u32>, events: Weak<dyn MacEvents>) -> std::io::Result<Agent> {
        let exe = macwl_run_exe()?;
        let cstr = |b: &[u8]| CString::new(b).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
        let mut args = vec![cstr(exe.as_os_str().as_encoded_bytes())?, cstr(b"--tty-agent")?];
        if let Some(n) = tty {
            args.push(cstr(format!("/dev/ttys{n:03}").as_bytes())?);
        }
        let (requests, their_requests) = socket_pair()?;
        let (events_fd, their_events) = socket_pair()?;
        let exe_c = args[0].clone();
        // A plain spawn of a new process, in a Mac session of its own (where
        // it takes the pty as its controlling terminal), its signals as a new
        // program's; fds 3 and 4 its sockets, 2 the host's stderr.
        let flags = POSIX_SPAWN_SETSID
            | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short
            | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
            | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
        let pid = spawn(&exe_c, &args, &[], flags, |actions, attr| unsafe {
            let mut all: libc::sigset_t = 0;
            libc::sigfillset(&mut all);
            libc::posix_spawnattr_setsigdefault(attr, &all);
            let mut none: libc::sigset_t = 0;
            libc::sigemptyset(&mut none);
            libc::posix_spawnattr_setsigmask(attr, &none);
            libc::posix_spawn_file_actions_addopen(actions, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
            libc::posix_spawn_file_actions_addopen(actions, 1, c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
            libc::posix_spawn_file_actions_adddup2(actions, 2, 2);
            libc::posix_spawn_file_actions_adddup2(actions, their_requests.as_raw_fd(), 3);
            libc::posix_spawn_file_actions_adddup2(actions, their_events.as_raw_fd(), 4);
        })
        .map_err(std::io::Error::from_raw_os_error)?;
        drop((their_requests, their_events));
        reaper().register(pid, events_fd, events);
        Ok(Agent { pid, tty, requests: Mutex::new((requests, proto::Reader::default())) })
    }

    /// A request and its answer.
    fn ask(&self, kind: u8, body: &[u8], fds: &[RawFd], want: u8) -> Result<proto::Msg, i64> {
        let mut r = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        let sock = r.0.as_raw_fd();
        let batches: Vec<&[RawFd]> = if fds.is_empty() { vec![&[]] } else { fds.chunks(proto::MAX_FDS).collect() };
        for (i, batch) in batches.iter().enumerate() {
            let (kind, body) = if i == 0 { (kind, body) } else { (FDS, &[][..]) };
            proto::send(sock, kind, body, batch).map_err(|_| errno::EAGAIN)?;
        }
        match r.1.recv(sock) {
            Some(msg) if msg.kind == want => Ok(msg),
            _ => Err(errno::EAGAIN),
        }
    }

    /// Start Mac program `s` in the Mac group for Linux group `pgid`: its
    /// Mac pid and process group, or the Linux errno it couldn't start with.
    pub fn spawn(&self, s: &MacSpawn, pgid: i64) -> Result<(libc::pid_t, libc::pid_t), i64> {
        let mut e = Encoder::default();
        e.bytes(s.path.as_bytes());
        e.u64(s.argv.len() as u64);
        for a in &s.argv {
            e.bytes(a);
        }
        e.u64(s.env.len() as u64);
        for v in &s.env {
            e.bytes(v);
        }
        e.bytes(s.cwd.as_bytes()).i64(pgid).u64(s.ignored).u64(s.blocked);
        e.u64(s.fds.len() as u64);
        for (n, _) in &s.fds {
            e.u64(*n as u64);
        }
        let fds: Vec<RawFd> = s.fds.iter().map(|(_, f)| f.host.as_raw_fd()).collect();
        let msg = self.ask(SPAWN, &e.0, &fds, SPAWNED)?;
        let mut d = Decoder(&msg.body);
        let (ok, a, b) = (d.bool(), d.i64(), d.i64());
        match (ok, a, b) {
            (Ok(true), Ok(pid), Ok(pgid)) => Ok((pid as libc::pid_t, pgid as libc::pid_t)),
            (Ok(false), Ok(e), _) => Err(spawn_errno(e as i32)),
            _ => Err(errno::EAGAIN),
        }
    }

    /// Linux group `pgid` is the terminal's foreground now: so is its Mac
    /// group (or the agent's own). Answered once the terminal says so.
    pub fn foreground(&self, pgid: i64) {
        let mut e = Encoder::default();
        e.i64(pgid);
        let _ = self.ask(FOREGROUND, &e.0, &[], DONE);
    }
}

/// The host's reaper: one thread that reads every agent's events and tells
/// the table, and reaps agents once they've gone.
struct Reaper {
    kq: OwnedFd,
    new: Mutex<Vec<(libc::pid_t, OwnedFd, Weak<dyn MacEvents>)>>,
}

const WAKE_ID: usize = 0;

fn reaper() -> &'static Reaper {
    static REAPER: OnceLock<&'static Reaper> = OnceLock::new();
    REAPER.get_or_init(|| {
        let kq = unsafe { libc::kqueue() };
        assert!(kq >= 0, "kqueue: {}", std::io::Error::last_os_error());
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = WAKE_ID;
        ev.filter = libc::EVFILT_USER;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        let reaper: &'static Reaper = Box::leak(Box::new(Reaper { kq, new: Mutex::new(Vec::new()) }));
        // One thread for every agent the host process has, for as long as
        // it lives: within the host's thread budget.
        linux_abi::host_threads::spawn("mac reaper", move || reaper.run()).expect("the reaper fits the host's thread budget");
        reaper
    })
}

impl Reaper {
    fn register(&self, pid: libc::pid_t, events: OwnedFd, sink: Weak<dyn MacEvents>) {
        self.new.lock().unwrap_or_else(|e| e.into_inner()).push((pid, events, sink));
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = WAKE_ID;
        ev.filter = libc::EVFILT_USER;
        ev.fflags = libc::NOTE_TRIGGER;
        unsafe { libc::kevent(self.kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    fn run(&self) {
        struct Watched {
            pid: libc::pid_t,
            sock: OwnedFd,
            reader: proto::Reader,
            sink: Weak<dyn MacEvents>,
        }
        let mut watched: HashMap<RawFd, Watched> = HashMap::new();
        loop {
            for (pid, sock, sink) in self.new.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                ev.ident = sock.as_raw_fd() as usize;
                ev.filter = libc::EVFILT_READ;
                ev.flags = libc::EV_ADD;
                unsafe { libc::kevent(self.kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
                watched.insert(sock.as_raw_fd(), Watched { pid, sock, reader: proto::Reader::default(), sink });
            }
            let mut out: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, out.as_mut_ptr(), out.len() as i32, std::ptr::null()) };
            for ev in out.iter().take(n.max(0) as usize) {
                if ev.filter != libc::EVFILT_READ {
                    continue;
                }
                let fd = ev.ident as RawFd;
                let Some(w) = watched.get_mut(&fd) else { continue };
                let eof = w.reader.fill(w.sock.as_raw_fd(), true) == proto::Read::Eof;
                while let Ok(Some(msg)) = w.reader.next() {
                    if msg.kind != CHANGED {
                        continue;
                    }
                    let mut d = Decoder(&msg.body);
                    if let (Ok(pid), Ok(status), Ok(user), Ok(sys)) = (d.i64(), d.i64(), d.u64(), d.u64())
                        && let Some(sink) = w.sink.upgrade()
                    {
                        sink.mac_changed(pid as libc::pid_t, Change::of(status as i32, user, sys));
                    }
                }
                if eof {
                    // The agent closes its events as it ends: reap it.
                    let w = watched.remove(&fd).unwrap();
                    let mut status = 0;
                    while unsafe { libc::waitpid(w.pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
                }
            }
        }
    }
}

// The agent's side.

/// Darwin signals the agent ignores: the keyboard's (its own group is the
/// terminal's foreground when no Mac program is), the background terminal
/// signals (it sets the foreground group from the background), the
/// terminal's hangup (the Linux session leader hears of that from the host)
/// and SIGPIPE.
const AGENT_IGNORES: [i32; 7] = [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU, libc::SIGHUP, libc::SIGPIPE];

/// A Linux process group's Mac programs: their Mac process group, and how
/// many of the agent's children are in it.
struct Group {
    mac: libc::pid_t,
    members: usize,
}

struct AgentState {
    tty: Option<OwnedFd>,
    own_pgid: libc::pid_t,
    groups: HashMap<i64, Group>,
    /// Each child's Linux group.
    children: HashMap<libc::pid_t, i64>,
    /// Children started suspended (to be put in the foreground first),
    /// whose first continue is the agent's own.
    starting: HashSet<libc::pid_t>,
    /// The terminal's Linux foreground group, as the host last said.
    foreground: Option<i64>,
    events: OwnedFd,
    /// What the agent waits on: requests, SIGCHLD, and each child's signals.
    kq: RawFd,
}

/// `macwl-run --tty-agent [TTY]` (started by a host, fds 3 and 4 its
/// sockets): run Mac programs for the host, with TTY (a pty's slave) as
/// their controlling terminal, until the host closes its end and the last
/// of them has ended.
pub fn agent_main(tty: Option<std::ffi::OsString>) -> ExitCode {
    // An agent never makes a VM: it may start processes, never replace one
    // with a VM in it.
    assert!(!crate::hv::vm_tried(), "an agent with a VM");
    for sig in AGENT_IGNORES {
        unsafe { libc::signal(sig, libc::SIG_IGN) };
    }
    let requests = unsafe { OwnedFd::from_raw_fd(3) };
    let events = unsafe { OwnedFd::from_raw_fd(4) };
    for fd in [&requests, &events] {
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // The terminal: ours, as the leader of the session we were started in.
    let tty = match tty {
        Some(path) => {
            let Ok(path) = CString::new(path.as_encoded_bytes()) else { return ExitCode::from(2) };
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd < 0 {
                eprintln!("macwl-run: tty agent: {}: {}", path.to_string_lossy(), std::io::Error::last_os_error());
                return ExitCode::from(1);
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSCTTY as libc::c_ulong, 0) } != 0 {
                eprintln!("macwl-run: tty agent: TIOCSCTTY: {}", std::io::Error::last_os_error());
            }
            Some(fd)
        }
        None => None,
    };
    let kq = unsafe { OwnedFd::from_raw_fd(libc::kqueue()) };
    let mut st = AgentState {
        kq: kq.as_raw_fd(),
        tty,
        own_pgid: unsafe { libc::getpgrp() },
        groups: HashMap::new(),
        children: HashMap::new(),
        starting: HashSet::new(),
        foreground: None,
        events,
    };
    let mut evs: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
    evs[0].ident = requests.as_raw_fd() as usize;
    evs[0].filter = libc::EVFILT_READ;
    evs[0].flags = libc::EV_ADD;
    // Children changing: SIGCHLD, left at its default (ignoring it would
    // reap them for us), seen by the kqueue.
    evs[1].ident = libc::SIGCHLD as usize;
    evs[1].filter = libc::EVFILT_SIGNAL;
    evs[1].flags = libc::EV_ADD;
    unsafe { libc::kevent(kq.as_raw_fd(), evs.as_ptr(), 2, std::ptr::null_mut(), 0, std::ptr::null()) };
    let mut reader = proto::Reader::default();
    let mut open = true;
    // A spawn whose fds haven't all come yet.
    let mut partial: Option<(proto::Msg, usize)> = None;
    loop {
        st.reap();
        if !open && st.children.is_empty() {
            return ExitCode::SUCCESS;
        }
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, out.as_mut_ptr(), 4, std::ptr::null()) };
        if !out.iter().take(n.max(0) as usize).any(|e| e.filter == libc::EVFILT_READ) || !open {
            continue;
        }
        if reader.fill(requests.as_raw_fd(), true) == proto::Read::Eof {
            open = false;
            // Nothing more to read: stop watching it.
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            ev.ident = requests.as_raw_fd() as usize;
            ev.filter = libc::EVFILT_READ;
            ev.flags = libc::EV_DELETE;
            unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        }
        loop {
            let msg = match reader.next() {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(_) => {
                    open = false;
                    break;
                }
            };
            let reply = match msg.kind {
                SPAWN => {
                    let want = Decoder(&msg.body).spawn_fd_count().unwrap_or(0);
                    if msg.fds.len() < want {
                        let have = msg.fds.len();
                        partial = Some((msg, have));
                        continue;
                    }
                    Some(st.spawn_reply(msg))
                }
                FDS => match partial.take() {
                    Some((mut first, _)) => {
                        first.fds.extend(msg.fds);
                        let want = Decoder(&first.body).spawn_fd_count().unwrap_or(0);
                        if first.fds.len() < want {
                            let have = first.fds.len();
                            partial = Some((first, have));
                            continue;
                        }
                        Some(st.spawn_reply(first))
                    }
                    None => None,
                },
                FOREGROUND => {
                    if let Ok(pgid) = Decoder(&msg.body).i64() {
                        st.foreground = Some(pgid);
                        st.follow_foreground();
                    }
                    Some((DONE, Vec::new()))
                }
                _ => None,
            };
            if let Some((kind, body)) = reply {
                let _ = proto::send(requests.as_raw_fd(), kind, &body, &[]);
            }
        }
    }
}

/// How many fds a spawn request says it brings (they come after it).
trait SpawnFdCount {
    fn spawn_fd_count(&mut self) -> Result<usize, Truncated>;
}

impl SpawnFdCount for Decoder<'_> {
    fn spawn_fd_count(&mut self) -> Result<usize, Truncated> {
        self.bytes()?;
        for _ in 0..2 {
            let n = self.count(8)?;
            for _ in 0..n {
                self.bytes()?;
            }
        }
        self.bytes()?;
        self.i64()?;
        self.u64()?;
        self.u64()?;
        self.count(8)
    }
}

impl AgentState {
    /// Tell the host of every child that changed.
    fn reap(&mut self) {
        loop {
            let mut status = 0;
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            let pid = unsafe { libc::wait4(-1, &mut status, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED, &mut ru) };
            if pid <= 0 {
                return;
            }
            self.changed(pid, status, &ru);
        }
    }

    fn changed(&mut self, pid: libc::pid_t, status: i32, ru: &libc::rusage) {
        if self.starting.contains(&pid) {
            // Its start: stopped before it ran, continued once in the
            // foreground. Neither is the program's own.
            if continued(status) {
                self.starting.remove(&pid);
                return;
            }
            if stopped(status).is_some() {
                return;
            }
            self.starting.remove(&pid);
        }
        let ns = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
        let mut e = Encoder::default();
        e.i64(pid as i64).i64(status as i64).u64(ns(ru.ru_utime)).u64(ns(ru.ru_stime));
        let _ = proto::send(self.events.as_raw_fd(), CHANGED, &e.0, &[]);
        if stopped(status).is_none() && !continued(status)
            && let Some(pgid) = self.children.remove(&pid)
            && let Some(g) = self.groups.get_mut(&pgid)
        {
            g.members -= 1;
            if g.members == 0 {
                self.groups.remove(&pgid);
                if self.foreground == Some(pgid) {
                    self.follow_foreground();
                }
            }
        }
    }

    /// The terminal's Mac foreground: the Mac group of its Linux
    /// foreground group, else our own.
    fn follow_foreground(&self) {
        let Some(tty) = &self.tty else { return };
        let mac = self.foreground.and_then(|g| self.groups.get(&g)).map_or(self.own_pgid, |g| g.mac);
        if unsafe { libc::tcsetpgrp(tty.as_raw_fd(), mac) } != 0 && mac != self.own_pgid {
            unsafe { libc::tcsetpgrp(tty.as_raw_fd(), self.own_pgid) };
        }
    }

    fn spawn_reply(&mut self, msg: proto::Msg) -> (u8, Vec<u8>) {
        let mut e = Encoder::default();
        match self.spawn(&msg) {
            Ok((pid, pgid)) => e.bool(true).i64(pid as i64).i64(pgid as i64),
            Err(err) => e.bool(false).i64(err as i64).i64(0),
        };
        (SPAWNED, e.0)
    }

    /// Start the Mac program a spawn request asks for: its pid and group,
    /// or a Darwin errno.
    fn spawn(&mut self, msg: &proto::Msg) -> Result<(libc::pid_t, libc::pid_t), i32> {
        assert!(!crate::hv::vm_tried(), "an agent with a VM");
        let mut d = Decoder(&msg.body);
        let bad = |_| libc::EINVAL;
        let cstr = |b: Vec<u8>| CString::new(b).map_err(|_| libc::EINVAL);
        let path = cstr(d.bytes().map_err(bad)?)?;
        let list = |d: &mut Decoder| -> Result<Vec<CString>, i32> {
            let n = d.count(8).map_err(bad)?;
            (0..n).map(|_| d.bytes().map_err(bad).map(|b| CString::new(b).unwrap_or_default())).collect()
        };
        let argv = list(&mut d)?;
        let env = list(&mut d)?;
        let cwd = cstr(d.bytes().map_err(bad)?)?;
        let pgid = d.i64().map_err(bad)?;
        let ignored = d.u64().map_err(bad)?;
        let blocked = d.u64().map_err(bad)?;
        let n = d.count(8).map_err(bad)?;
        let numbers: Vec<RawFd> = (0..n).map(|_| d.u64().map(|v| v as RawFd).map_err(bad)).collect::<Result<_, _>>()?;
        if msg.fds.len() != numbers.len() {
            return Err(libc::EINVAL);
        }
        // Guest fd n becomes fd n: by way of fds above every number in play
        // (so no move overwrites a source still to come), closed again.
        // Everything else closes (CLOEXEC_DEFAULT): our sockets, the tty.
        let ours = [3, 4, self.tty.as_ref().map_or(0, |t| t.as_raw_fd())];
        let above = numbers.iter().chain(msg.fds.iter().map(|f| f.as_raw_fd()).collect::<Vec<_>>().iter()).chain(ours.iter()).copied().max().unwrap_or(2) + 1;
        // Its signals: what the guest ignored stays ignored (set here, for
        // the spawn: a new process inherits ignored signals), the rest as a
        // new program finds them; the guest's mask carries over. SIGCHLD
        // isn't ignored here even for that: ignoring it would have our
        // children reaped for us.
        let mut ignore = Vec::new();
        let (mut default, mut mask): (libc::sigset_t, libc::sigset_t) = (0, 0);
        unsafe {
            libc::sigemptyset(&mut default);
            libc::sigemptyset(&mut mask);
        }
        for sig in 1..=31u64 {
            let Some(host) = linux_abi::syscall::signal_to_darwin(sig) else { continue };
            if ignored & (1 << (sig - 1)) != 0 && !matches!(host, libc::SIGCHLD | libc::SIGKILL | libc::SIGSTOP) {
                ignore.push(host);
            } else {
                unsafe { libc::sigaddset(&mut default, host) };
            }
            if blocked & (1 << (sig - 1)) != 0 {
                unsafe { libc::sigaddset(&mut mask, host) };
            }
        }
        let saved: Vec<(i32, libc::sighandler_t)> = ignore.iter().map(|&s| (s, unsafe { libc::signal(s, libc::SIG_IGN) })).collect();
        // A new Mac group for a Linux group with no Mac program yet, else
        // that group's. Put in the terminal's foreground before it runs, if
        // its Linux group is: it may touch the terminal at once.
        let existing = self.groups.get(&pgid).map(|g| g.mac);
        let suspended = self.tty.is_some() && self.foreground == Some(pgid) && existing.is_none();
        let attempt = |group: libc::pid_t, suspended: bool| {
            let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short
                | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
                | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short
                | libc::POSIX_SPAWN_SETPGROUP as libc::c_short;
            if suspended {
                flags |= POSIX_SPAWN_START_SUSPENDED;
            }
            spawn(&path, &argv, &env, flags, |actions, attr| unsafe {
                libc::posix_spawnattr_setsigdefault(attr, &default);
                libc::posix_spawnattr_setsigmask(attr, &mask);
                libc::posix_spawnattr_setpgroup(attr, group);
                for (i, f) in msg.fds.iter().enumerate() {
                    libc::posix_spawn_file_actions_adddup2(actions, f.as_raw_fd(), above + i as RawFd);
                }
                for (i, &n) in numbers.iter().enumerate() {
                    libc::posix_spawn_file_actions_adddup2(actions, above + i as RawFd, n);
                    libc::posix_spawn_file_actions_addclose(actions, above + i as RawFd);
                }
                posix_spawn_file_actions_addchdir_np(actions, cwd.as_ptr());
            })
        };
        let mut result = attempt(existing.unwrap_or(0), suspended);
        // The group's members have all gone (not yet reaped): a new one.
        let mut new_group = existing.is_none();
        if existing.is_some() && result == Err(libc::EPERM) {
            self.groups.remove(&pgid);
            new_group = true;
            result = attempt(0, self.tty.is_some() && self.foreground == Some(pgid));
        }
        for (s, old) in saved {
            unsafe { libc::signal(s, old) };
        }
        let pid = result?;
        let group = self.groups.entry(pgid).or_insert(Group { mac: pid, members: 0 });
        group.members += 1;
        let mac = group.mac;
        self.children.insert(pid, pgid);
        // macOS sends no SIGCHLD when a child continues: the signals it gets
        // say when to look (SIGCONT's continue shows by then).
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD;
        ev.fflags = libc::NOTE_SIGNAL | libc::NOTE_EXIT;
        unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if new_group && self.tty.is_some() && self.foreground == Some(pgid) {
            // Into the foreground, then on.
            self.starting.insert(pid);
            self.follow_foreground();
            unsafe { libc::kill(pid, libc::SIGCONT) };
        }
        Ok((pid, mac))
    }
}

/// What macOS says of Mac process `pid`: its user and system CPU time (ns)
/// and resident bytes, for a stand-in's `/proc` and `ps`.
pub(crate) fn usage(pid: libc::pid_t) -> Option<(u64, u64, u64)> {
    let mut ri: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut ri).cast()) } != 0 {
        return None;
    }
    // Its times are in Mach absolute time units.
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        #[allow(deprecated)]
        let mut tb = libc::mach_timebase_info { numer: 1, denom: 1 };
        #[allow(deprecated)]
        unsafe {
            libc::mach_timebase_info(&mut tb)
        };
        #[allow(deprecated)]
        let r = (tb.numer as u64, tb.denom.max(1) as u64);
        r
    });
    let ns = |t: u64| (t as u128 * numer as u128 / denom as u128) as u64;
    Some((ns(ri.ri_user_time), ns(ri.ri_system_time), ri.ri_resident_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_start_new_processes() {
        let plain = libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short | POSIX_SPAWN_SETSID;
        assert_eq!(new_process_flags(plain), plain);
    }

    #[test]
    #[should_panic(expected = "never POSIX_SPAWN_SETEXEC")]
    fn a_spawn_never_replaces_its_caller() {
        new_process_flags(libc::POSIX_SPAWN_SETEXEC as libc::c_short | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short);
    }

    #[test]
    #[should_panic(expected = "never POSIX_SPAWN_SETEXEC")]
    fn spawn_checks_its_flags_before_it_spawns() {
        let _ = spawn(c"/usr/bin/true", &[c"true".into()], &[], libc::POSIX_SPAWN_SETEXEC as libc::c_short, |_, _| {});
    }

    #[test]
    fn spawn_starts_a_child() {
        let pid = spawn(c"/usr/bin/false", &[c"false".into()], &[], 0, |_, _| {}).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(linux_status(status), 1 << 8);
    }

    #[test]
    fn darwin_statuses_in_linux_numbers() {
        // Killed by SIGUSR1 (Darwin 30, Linux 10), with and without core.
        assert_eq!(linux_status(30), 10);
        assert_eq!(linux_status(30 | 0x80), 10 | 0x80);
        // Stopped by SIGTSTP (Darwin 18, Linux 20).
        assert_eq!(linux_status((18 << 8) | 0x7f), (20 << 8) | 0x7f);
        assert_eq!(Change::of((18 << 8) | 0x7f, 0, 0), Change::Stopped(20));
        assert_eq!(Change::of((0x13 << 8) | 0x7f, 0, 0), Change::Continued);
        assert_eq!(Change::of(3 << 8, 1, 2), Change::Ended { status: 3 << 8, user_ns: 1, sys_ns: 2 });
        assert_eq!(spawn_errno(86), errno::ENOEXEC);
        assert_eq!(spawn_errno(libc::ENOENT), errno::ENOENT);
    }
}
