//! Federated hosts (docs/processes.md, section 1, "Federated hosts"; step
//! 9): a program can run in a host of its own and stay an ordinary
//! process of its session.
//!
//! It moves at `execve`, when the program is one set to have a host of its
//! own (`MACWL_NEW_HOST=1` in the environment the `execve` passes, or its
//! path or name in `~/.config/macwl/hosts`). The process's host starts a
//! new one (`macwl-run --adopted`, a new Mac process, never a fork) with a
//! socket to it, and hands it the process (`ADOPT`): its pid, parent, group,
//! session and terminal, what it keeps across an exec ([`Process::encode_for_move`])
//! and its fds. The new host loads the program and says whether it could
//! (`ADOPTED`, or `REFUSED` with the errno `execve` then returns), and the
//! process becomes a stand-in in the old host's table, as for a Mac
//! program (section 7): its parent `wait`s for it there, and signals to it
//! go to its host.
//!
//! The socket stays as the link between the two hosts (the doc's design
//! had every message go through the coordinator; a direct link is one hop,
//! and its end is the other host's end): the new host reports the
//! process's stops, continues and end on it ([`CHANGED`]), as an agent
//! reports a Mac program's; the old host sends it signals ([`SIGNAL`]) and
//! its new parent if it's orphaned ([`REPARENT`]). If the new host dies,
//! the stand-in ends as killed by SIGKILL; if the old one does, the moved
//! process's parent has gone, and it's the new host's init's.

use std::{
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use linux_abi::{
    errno,
    syscall::{Exec, SigInfo, Thread},
    wire::{Decoder, Encoder, Truncated},
    Process,
};

use super::{Elsewhere, First, RangePids, StandIn, Table, Target};
use crate::{mac::Change, proto, Error, Exit, Proc};

/// Old host to new: the process (body: [`encode_adopt`]; its fds with it,
/// and in [`FDS`] messages after it).
const ADOPT: u8 = 1;
/// Old host to new: more of the fds.
const FDS: u8 = 2;
/// Old host to new: a signal for the process, or one of its threads.
const SIGNAL: u8 = 3;
/// Old host to new: the process's parent is this one now (its old one
/// ended).
const REPARENT: u8 = 4;
/// New host to old: the program is loaded and runs.
const ADOPTED: u8 = 16;
/// New host to old: it couldn't be (a Linux errno), and the new host goes.
const REFUSED: u8 = 17;
/// New host to old: the process stopped, continued or ended.
const CHANGED: u8 = 18;

/// How long the old host waits for the new one's answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// The other end of a link between two hosts.
#[derive(Debug)]
pub(crate) struct Peer {
    /// The other host's Mac pid.
    pub mac_pid: libc::pid_t,
    sock: Mutex<UnixStream>,
}

impl Peer {
    fn send(&self, kind: u8, body: &[u8]) {
        let sock = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = proto::send(sock.as_raw_fd(), kind, body, &[]);
    }

    /// Signal `pid` there (or its thread `tid`).
    pub fn signal(&self, pid: i64, tid: Option<i64>, info: SigInfo) {
        let mut e = Encoder::default();
        e.i64(pid).bool(tid.is_some()).i64(tid.unwrap_or(0));
        e.u64(info.signo).i64(info.code as i64).i64(info.pid as i64).u64(info.uid as u64).i64(info.status as i64);
        self.send(SIGNAL, &e.0);
    }

    /// `pid`'s parent is `ppid` from now on.
    pub fn reparent(&self, pid: i64, ppid: i64) {
        let mut e = Encoder::default();
        e.i64(pid).i64(ppid);
        self.send(REPARENT, &e.0);
    }

    /// `pid` (here, whose parent is the other host's) changed.
    pub fn changed(&self, pid: i64, change: &Change) {
        let mut e = Encoder::default();
        e.i64(pid);
        match *change {
            Change::Stopped(sig) => e.u64(0).u64(sig),
            Change::Continued => e.u64(1),
            Change::Ended { status, user_ns, sys_ns } => e.u64(2).i64(status as i64).u64(user_ns).u64(sys_ns),
        };
        self.send(CHANGED, &e.0);
    }
}

fn decode_change(d: &mut Decoder) -> Result<Change, Truncated> {
    Ok(match d.u64()? {
        0 => Change::Stopped(d.u64()?),
        1 => Change::Continued,
        2 => Change::Ended { status: d.i64()? as i32, user_ns: d.u64()?, sys_ns: d.u64()? },
        _ => return Err(Truncated),
    })
}

/// Whether `exec` names a program set to have a host of its own: by
/// `MACWL_NEW_HOST=1` in the environment it's given, or by its path or
/// name in `~/.config/macwl/hosts` (a line each; `#` starts a comment).
fn wants_own_host(exec: &Exec) -> bool {
    if exec.envp.iter().any(|e| e == b"MACWL_NEW_HOST=1") {
        return true;
    }
    let Some(home) = std::env::var_os("HOME") else { return false };
    let Ok(text) = std::fs::read(std::path::Path::new(&home).join(".config/macwl/hosts")) else { return false };
    text.split(|&b| b == b'\n').map(|l| l.split(|&b| b == b'#').next().unwrap_or_default().trim_ascii()).any(|l| {
        !l.is_empty() && if l.contains(&b'/') { l == exec.exe.as_slice() } else { l == exec.comm.as_slice() }
    })
}

impl Table {
    /// Whether `proc`'s `execve` of `exec` moves it to a host of its own:
    /// a program set to have one, unless it's alone in this host already.
    pub(crate) fn moves(&self, exec: &Exec) -> bool {
        self.live.load(std::sync::atomic::Ordering::Acquire) > 1 && wants_own_host(exec)
    }

    /// Move `proc` (whose thread `thread` is exec'ing `exec`) to a new host
    /// of its own: once the new host has its program loaded, `proc` stands
    /// in for it here (its tasks end, as for a Mac program's `execve`).
    /// The errno for `execve` if it can't.
    pub(crate) fn move_to_host(self: &Arc<Self>, proc: &Proc, thread: &Thread, exec: &Exec) -> Result<(), i64> {
        let pid = proc.abi.pid;
        let (ppid, pgid, sid, ctty) = self.state().procs.get(&pid).map(|e| (e.ppid, e.pgid, e.sid, e.ctty)).ok_or(errno::ESRCH)?;
        // It doesn't move again from its new host.
        let mut exec = exec.clone();
        exec.envp.retain(|e| !e.starts_with(b"MACWL_NEW_HOST="));
        let (sock, mac_pid) = spawn_host().map_err(|e| errno::from_io(&e))?;
        let mut moved = Encoder::default();
        let files = proc.abi.encode_for_move(thread, &exec, &mut moved);
        let mut e = Encoder::default();
        e.i64(pid).i64(ppid).i64(pgid).i64(sid).bool(ctty.is_some()).u64(ctty.unwrap_or(0) as u64).u64(files.len() as u64);
        e.0.extend_from_slice(&moved.0);
        let fds: Vec<RawFd> = files.iter().map(|(_, f)| f.host.as_raw_fd()).collect();
        let mut batches = fds.chunks(proto::MAX_FDS);
        let sent = proto::send(sock.as_raw_fd(), ADOPT, &e.0, batches.next().unwrap_or(&[]))
            .and_then(|()| batches.try_for_each(|b| proto::send(sock.as_raw_fd(), FDS, &[], b)));
        let gone = |sock: UnixStream| {
            drop(sock);
            reap(mac_pid);
        };
        if sent.is_err() {
            gone(sock);
            return Err(errno::EAGAIN);
        }
        let _ = sock.set_read_timeout(Some(ANSWER_WITHIN));
        let mut reader = proto::Reader::default();
        match reader.recv(sock.as_raw_fd()) {
            Some(msg) if msg.kind == ADOPTED => {}
            Some(msg) if msg.kind == REFUSED => {
                gone(sock);
                return Err(Decoder(&msg.body).i64().unwrap_or(errno::ENOEXEC));
            }
            _ => {
                gone(sock);
                return Err(errno::EAGAIN);
            }
        }
        let _ = sock.set_read_timeout(None);
        let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
        let Ok(send) = sock.try_clone() else {
            gone(sock);
            return Err(errno::EAGAIN);
        };
        let peer = Arc::new(Peer { mac_pid, sock: Mutex::new(send) });
        // Terminals' masters it passed stay held here as well: their last
        // reference going here would hang them up.
        let held: Vec<_> = files.into_iter().map(|(_, f)| f).filter(|f| f.tty().is_some_and(|t| t.master)).collect();
        let cmdline: Vec<u8> = exec.argv.iter().flat_map(|a| a.iter().copied().chain([0])).collect();
        {
            let mut s = self.state();
            if let Some(e) = s.procs.get_mut(&pid) {
                // It has exec'd: setpgid on it is EACCES from now on.
                e.execd = true;
                e.stand = Some(StandIn { of: Elsewhere::Host(peer.clone()), stopped: false, cmdline, comm: exec.comm.clone(), pending: Vec::new(), _held: held });
            }
        }
        let weak = Arc::downgrade(self);
        let spawned = linux_abi::host_threads::spawn("host link", move || from_child_host(weak, pid, peer, sock, reader));
        if spawned.is_err() {
            eprintln!("macwl-run: pid {pid}: no thread for its host's link");
        }
        Ok(())
    }

    /// The host's first process, `pid`, moved here from the host at the
    /// other end of `peer` (`link` its socket, to read), where its parent
    /// is: that host hears of the process, and signals it.
    pub(crate) fn adopted(self: &Arc<Self>, pid: i64, peer: Arc<Peer>, link: UnixStream) {
        if let Some(e) = self.state().procs.get_mut(&pid) {
            e.remote_parent = true;
            e.execd = true;
        }
        let _ = self.parent_peer.set(peer);
        let weak = Arc::downgrade(self);
        if linux_abi::host_threads::spawn("parent host link", move || from_parent_host(weak, link)).is_err() {
            eprintln!("macwl-run: no thread for the link to the parent host");
        }
    }

    /// Tell the host of `pid`'s parent, if that's another host, of its
    /// change. True if it was told (the parent isn't here).
    pub(super) fn tell_remote_parent(&self, pid: i64, change: impl FnOnce() -> Change) -> bool {
        let remote = self.state().procs.get(&pid).is_some_and(|e| e.remote_parent);
        if !remote {
            return false;
        }
        if let Some(peer) = self.parent_peer.get() {
            peer.changed(pid, &change());
        }
        true
    }
}

/// Reap host `mac_pid`, which has gone or is going (a thread waits for it,
/// so no caller does).
fn reap(mac_pid: libc::pid_t) {
    let _ = linux_abi::host_threads::spawn("host reaper", move || {
        let mut status = 0;
        while unsafe { libc::waitpid(mac_pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
    });
}

/// The old host's side of a link: the moved process `pid`'s changes, as
/// they come, until the new host goes (the process ends with it, as killed
/// by SIGKILL, if it hadn't).
fn from_child_host(table: Weak<Table>, pid: i64, peer: Arc<Peer>, sock: UnixStream, mut reader: proto::Reader) {
    loop {
        let Some(msg) = reader.recv(sock.as_raw_fd()) else { break };
        let Some(table) = table.upgrade() else { break };
        if msg.kind == CHANGED {
            let mut d = Decoder(&msg.body);
            if let (Ok(p), Ok(change)) = (d.i64(), decode_change(&mut Decoder(&msg.body[8..]))) {
                table.stand_in_changed(p, change);
            }
        }
    }
    if let Some(table) = table.upgrade() {
        let ours = {
            let s = table.state();
            s.procs.get(&pid).and_then(|e| e.stand.as_ref()).is_some_and(|m| matches!(&m.of, Elsewhere::Host(p) if Arc::ptr_eq(p, &peer)))
        };
        if ours {
            table.stand_in_changed(pid, Change::Ended { status: 9, user_ns: 0, sys_ns: 0 });
        }
    }
    let mut status = 0;
    while unsafe { libc::waitpid(peer.mac_pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
}

/// The new host's side of a link: signals and reparentings for its
/// processes from the old host, until it goes; then the processes whose
/// parent was there are init's.
fn from_parent_host(table: Weak<Table>, sock: UnixStream) {
    let mut reader = proto::Reader::default();
    while let Some(msg) = reader.recv(sock.as_raw_fd()) {
        let Some(table) = table.upgrade() else { return };
        let mut d = Decoder(&msg.body);
        match msg.kind {
            SIGNAL => {
                let Ok((pid, has_tid, tid)) = (|| Ok::<_, Truncated>((d.i64()?, d.bool()?, d.i64()?)))() else { continue };
                let Ok(info) = (|| {
                    let (signo, code, from, uid, status) = (d.u64()?, d.i64()? as i32, d.i64()?, d.u64()? as u32, d.i64()? as i32);
                    Ok::<_, Truncated>(SigInfo { uid, status, ..SigInfo::user(signo, code, from) })
                })() else {
                    continue;
                };
                let to = if has_tid { Target::Thread { tgid: Some(pid), tid } } else { Target::Pid(pid) };
                let _ = table.deliver(to, info);
            }
            REPARENT => {
                if let (Ok(pid), Ok(ppid)) = (d.i64(), d.i64())
                    && let Some(e) = table.state().procs.get_mut(&pid)
                {
                    e.ppid = ppid;
                }
            }
            _ => {}
        }
    }
    // The old host has gone: its processes can't wait for these any more.
    if let Some(table) = table.upgrade() {
        let mut s = table.state();
        for e in s.procs.values_mut().filter(|e| e.remote_parent) {
            e.remote_parent = false;
            e.ppid = 1;
        }
    }
}

/// Start `macwl-run --adopted`: a new host, a new Mac process in a Mac
/// session of its own, its signals as a new program's, with a socket to
/// this host as its fd 3 and this host's stderr as its own. The socket's
/// end here, and its Mac pid.
fn spawn_host() -> std::io::Result<(UnixStream, libc::pid_t)> {
    let (mine, theirs) = UnixStream::pair()?;
    for fd in [mine.as_raw_fd(), theirs.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let exe = crate::mac::macwl_run_exe()?;
    let cstr = |b: &[u8]| std::ffi::CString::new(b).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let args = [cstr(exe.as_os_str().as_encoded_bytes())?, cstr(b"--adopted")?];
    // Its environment is this host's (MACWL_* settings, HOME): the guest's
    // goes in the hand-off.
    let env: Vec<std::ffi::CString> = std::env::vars_os().filter_map(|(k, v)| cstr(&[k.as_encoded_bytes(), b"=", v.as_encoded_bytes()].concat()).ok()).collect();
    let flags = crate::mac::POSIX_SPAWN_SETSID
        | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short
        | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
        | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
    let pid = crate::mac::spawn(&args[0], &args, &env, flags, |actions, attr| unsafe {
        let mut all: libc::sigset_t = 0;
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(attr, &all);
        let mut none: libc::sigset_t = 0;
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(attr, &none);
        libc::posix_spawn_file_actions_addopen(actions, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
        libc::posix_spawn_file_actions_addopen(actions, 1, c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
        libc::posix_spawn_file_actions_adddup2(actions, 2, 2);
        libc::posix_spawn_file_actions_adddup2(actions, theirs.as_raw_fd(), 3);
    })
    .map_err(std::io::Error::from_raw_os_error)?;
    Ok((mine, pid))
}

/// `macwl-run --adopted`: a host for a process moving here from the host
/// at the other end of fd 3, as it execs (see the module's docs).
pub fn run_adopted() -> Result<Exit, Error> {
    let sock = unsafe { UnixStream::from_raw_fd(3) };
    unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut reader = proto::Reader::default();
    let refuse = |sock: &UnixStream, err: i64| {
        let mut e = Encoder::default();
        e.i64(err);
        let _ = proto::send(sock.as_raw_fd(), REFUSED, &e.0, &[]);
    };
    let Some(msg) = reader.recv(sock.as_raw_fd()).filter(|m| m.kind == ADOPT) else {
        return Err(Error::Unexpected("no process to adopt".into()));
    };
    let mut d = Decoder(&msg.body);
    let header = (|| Ok::<_, Truncated>((d.i64()?, d.i64()?, d.i64()?, d.i64()?, d.bool()?, d.u64()?, d.count(4)?)))();
    let Ok((pid, ppid, pgid, sid, has_ctty, ctty, wanted)) = header else {
        refuse(&sock, errno::EINVAL);
        return Err(Error::Unexpected("a bad hand-off".into()));
    };
    // Its fds: those that came with the message, then the rest.
    let mut fds: Vec<OwnedFd> = msg.fds;
    while fds.len() < wanted {
        match reader.recv(sock.as_raw_fd()) {
            Some(m) if m.kind == FDS => fds.extend(m.fds),
            _ => {
                refuse(&sock, errno::EINVAL);
                return Err(Error::Unexpected("a hand-off's fds didn't come".into()));
            }
        }
    }
    let Ok((process, thread, exe, argv, envp)) = Process::decode_moved(&mut d, fds) else {
        refuse(&sock, errno::EINVAL);
        return Err(Error::Unexpected("a bad hand-off".into()));
    };
    let elf = match crate::launch::read_program(&process, std::ffi::OsStr::from_bytes(&exe)) {
        Ok((elf, _)) => elf,
        Err(why) => {
            refuse(&sock, errno::ENOENT);
            return Err(Error::Unexpected(why));
        }
    };
    let pids = RangePids::new()?;
    let (Ok(send), Ok(answer)) = (sock.try_clone(), sock.try_clone()) else {
        refuse(&sock, errno::EAGAIN);
        return Err(Error::Unexpected("no socket".into()));
    };
    let peer = Arc::new(Peer { mac_pid: unsafe { libc::getppid() }, sock: Mutex::new(send) });
    let loaded: Box<dyn FnOnce(Result<(), i64>)> = Box::new(move |r| match r {
        Ok(()) => drop(proto::send(answer.as_raw_fd(), ADOPTED, &[], &[])),
        Err(e) => {
            let mut b = Encoder::default();
            b.i64(e);
            let _ = proto::send(answer.as_raw_fd(), REFUSED, &b.0, &[]);
        }
    });
    let first = First::Adopted { ppid, pgid, sid, ctty: has_ctty.then_some(ctty as u32), peer, link: sock, loaded };
    let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let envp: Vec<&[u8]> = envp.iter().map(Vec::as_slice).collect();
    match super::run_first(Box::new(pids), pid, &elf, &argv, &envp, process, thread, first) {
        // Refused, and its execve said why: nothing more to say.
        Err(Error::Load(_)) => Ok(Exit::Code(127)),
        r => r,
    }
}
