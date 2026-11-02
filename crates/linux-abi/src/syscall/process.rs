//! Processes: `clone` (fork and vfork), `execve`, `wait4`/`waitid`.
//!
//! A guest process is a host process. The dispatcher checks and decodes these
//! calls; the core does the parts that touch the CPU and memory — making the
//! child ([`Action::Fork`]) or replacing the program ([`Action::Exec`]) — and
//! calls back into [`Process`] around them.
//!
//! `execve` of a Mac program (a Mach-O file, or a script whose interpreter is
//! one) replaces the process with it for real, as WSL runs Windows programs:
//! [`exec_host`]. It keeps the pid, so the parent's `wait`, the terminal and
//! job control carry on as for any child. It gets the guest's fds (by their
//! guest numbers), working directory, signal mask and ignored signals, and
//! environment — except `PATH` and `SHELL`, which are the Mac's (those
//! macwl-run started with), so the programs it runs in turn are Mac ones too.
//! It sees the Mac's file system, which under `--root` isn't the guest's.
//! It is off unless `MACWL_MAC_PROGRAMS=1` (otherwise such an `execve` fails
//! with ENOEXEC). See ROADMAP.md, item 13.
//!
//! Each forked child keeps a control socket to its parent. Over it the child
//! says when it is ready (its memory snapshot is taken), when it has exec'd
//! (which ends a vfork), and — just before exiting — whether it was killed by
//! a Linux signal. The host process then exits with 128 + the signal rather
//! than raising it (which would make macOS write a crash report), and the
//! parent's `wait` reports the signal.

use std::{
    collections::BTreeMap,
    sync::atomic::Ordering,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
};

use super::{
    kernel::{ChildInfo, Waited},
    ok, path_arg, signal, Action, Process, Thread,
};
use crate::{
    errno,
    fs::{FdTable, Filesystem, Target},
    memory::{GuestMemory, GuestMemoryExt},
    wire::{Decoder, Encoder, Truncated},
};

/// Control-socket messages from a child to its parent.
pub mod ctl {
    /// The child has its memory snapshot; the parent may run on.
    pub const READY: u8 = b'R';
    /// The child exec'd a new program.
    pub const EXEC: u8 = b'E';
    /// Followed by one byte: the child is about to exit, killed by that
    /// (Linux) signal.
    pub const KILLED: u8 = b'S';
    /// Parent to child, followed by an i32: put yourself in this process
    /// group (`setpgid` on a child: see [`super::setpgid`]).
    pub const SETPGID: u8 = b'P';
    /// Child to parent, followed by an i32: how that went (0 or a Linux
    /// errno).
    pub const SETPGID_DONE: u8 = b'G';
}

/// A fork the core should carry out, from `clone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fork {
    /// The child's stack pointer, or 0 to keep the parent's.
    pub stack: u64,
    /// CLONE_SETTLS: the child's TPIDR_EL0.
    pub tls: Option<u64>,
    /// CLONE_VFORK: the parent waits until the child execs or exits.
    pub vfork: bool,
    /// CLONE_VM: the child runs in the parent's address space (with vfork,
    /// or on its own: [`Kernel::shares_memory`]).
    pub vm: bool,
    /// CLONE_PARENT_SETTID: where in the parent to store the child's pid.
    pub parent_tid: Option<u64>,
    /// CLONE_CHILD_SETTID: likewise, in the child.
    pub child_tid: Option<u64>,
    /// The signal the parent gets when the child ends (`flags & CSIGNAL`,
    /// SIGCHLD as a rule; 0 for none).
    pub exit_signal: u64,
}

impl Fork {
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.stack).bool(self.vfork).u64(self.exit_signal);
        for v in [self.tls, self.parent_tid, self.child_tid] {
            e.bool(v.is_some()).u64(v.unwrap_or(0));
        }
    }

    pub fn decode(d: &mut Decoder) -> Result<Self, Truncated> {
        let (stack, vfork, exit_signal) = (d.u64()?, d.bool()?, d.u64()?);
        let mut opt = || -> Result<Option<u64>, Truncated> {
            let (some, v) = (d.bool()?, d.u64()?);
            Ok(some.then_some(v))
        };
        Ok(Fork { stack, vfork, vm: vfork, exit_signal, tls: opt()?, parent_tid: opt()?, child_tid: opt()? })
    }
}

/// A thread the core should start, from `clone` with CLONE_THREAD. It
/// resumes like the caller with x0 = 0, on `stack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawn {
    pub stack: u64,
    /// CLONE_SETTLS: the thread's TPIDR_EL0.
    pub tls: Option<u64>,
    /// CLONE_PARENT_SETTID: where to store the new tid.
    pub parent_tid: Option<u64>,
    /// CLONE_CHILD_SETTID: likewise (in the new thread, but memory is shared).
    pub child_settid: Option<u64>,
    /// CLONE_CHILD_CLEARTID: zeroed and futex-woken when the thread exits.
    pub child_cleartid: Option<u64>,
}

/// A program the core should load in place of the current one, from `execve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub elf: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
    /// Guest path of the executable, for `/proc/self/exe`.
    pub exe: Vec<u8>,
    /// The name it runs under (`/proc/<pid>/comm`, what `pgrep` matches):
    /// the base name of the path `execve` was given, as Linux takes it
    /// (`sleep` for coreutils' multi-call binary linked as `sleep`).
    pub comm: Vec<u8>,
}

/// A Mac program to run in place of this one: see [`exec_host`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExec {
    /// Its host path.
    pub path: std::ffi::CString,
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
}

/// What `execve` found.
enum Program {
    Linux(Exec),
    /// A Mac program, at this host path, with this argv.
    Mac(std::ffi::CString, Vec<Vec<u8>>),
}

/// Per-process bookkeeping for forked children.
#[derive(Debug, Default)]
pub(super) struct Children {
    /// Control sockets to children not yet waited for, by pid.
    ctl: BTreeMap<i64, OwnedFd>,
    /// Children whose control socket said they were killed by a signal.
    killed: BTreeMap<i64, u8>,
    /// Bytes of a message not yet whole, per child.
    partial: BTreeMap<i64, Vec<u8>>,
}

impl Children {
    /// Read whatever `pid`'s control socket has to say, without blocking.
    fn poll(&mut self, pid: i64) {
        self.read_messages(pid, false);
    }

    /// Take in what `pid`'s control socket has said (waiting for a
    /// SETPGID_DONE if `wait_reply`), and return that reply.
    fn read_messages(&mut self, pid: i64, wait_reply: bool) -> Option<i32> {
        let sock = self.ctl.get(&pid)?.as_raw_fd();
        let mut reply = None;
        loop {
            let mut buf = [0u8; 64];
            let flags = if wait_reply && reply.is_none() { 0 } else { libc::MSG_DONTWAIT };
            let n = unsafe { libc::recv(sock, buf.as_mut_ptr().cast(), buf.len(), flags) };
            if n <= 0 {
                if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return reply;
            }
            let partial = self.partial.entry(pid).or_default();
            partial.extend_from_slice(&buf[..n as usize]);
            // Whole messages: 1 byte, or a kind byte and its payload.
            loop {
                let need = match partial.first() {
                    None => break,
                    Some(&ctl::KILLED) => 2,
                    Some(&ctl::SETPGID_DONE) => 5,
                    Some(_) => 1,
                };
                if partial.len() < need {
                    break;
                }
                let msg: Vec<u8> = partial.drain(..need).collect();
                match msg[0] {
                    ctl::KILLED => {
                        self.killed.insert(pid, msg[1]);
                    }
                    ctl::SETPGID_DONE => reply = Some(i32::from_le_bytes(msg[1..5].try_into().unwrap())),
                    _ => {}
                }
            }
            if reply.is_some() && !wait_reply {
                continue;
            }
            if wait_reply && reply.is_some() {
                return reply;
            }
        }
    }

    /// The Linux wait status for a host one, for child `pid`.
    fn status(&mut self, pid: i64, host: i32) -> i32 {
        if libc::WIFEXITED(host) {
            let code = libc::WEXITSTATUS(host);
            if code >= 128 {
                self.poll(pid);
                if let Some(&sig) = self.killed.get(&pid) {
                    return sig as i32;
                }
            }
            code << 8
        } else if libc::WIFSIGNALED(host) {
            let core = if libc::WCOREDUMP(host) { 0x80 } else { 0 };
            signal::from_darwin(libc::WTERMSIG(host)) as i32 | core
        } else if libc::WIFSTOPPED(host) {
            ((signal::from_darwin(libc::WSTOPSIG(host)) as i32) << 8) | 0x7f
        } else {
            0xffff // continued
        }
    }

    /// `pid` has been reaped: forget it.
    fn reaped(&mut self, pid: i64) {
        self.ctl.remove(&pid);
        self.killed.remove(&pid);
        self.partial.remove(&pid);
    }
}

impl Process {
    /// Encode everything a forked child inherits from thread `t`'s process.
    /// The host fds of the fd table aren't in the message: they're returned,
    /// in order, to be passed with it.
    pub fn encode_for_fork(&self, t: &Thread, e: &mut Encoder) -> Vec<RawFd> {
        let fds = self.fds.encode(e);
        self.fs.encode(e);
        e.bytes(&self.exe());
        self.signals().encode(&t.signals, e);
        for [soft, hard] in *self.rlimits.lock().unwrap_or_else(|e| e.into_inner()) {
            e.u64(soft).u64(hard);
        }
        e.bytes(&t.comm).bool(self.dumpable.load(Ordering::Relaxed)).bool(self.no_new_privs.load(Ordering::Relaxed));
        self.file_maps().encode(e);
        e.u64(self.umask() as u64);
        fds
    }

    /// The child's side of [`Process::encode_for_fork`]: the process and its
    /// one thread. `ctl` is its control socket to the parent.
    pub fn decode_forked(d: &mut Decoder, fds: Vec<OwnedFd>, ctl: OwnedFd) -> Result<(Process, Thread), Truncated> {
        let mut fds = fds.into_iter();
        let mut p = Process::new("/");
        let table = FdTable::decode(d, &mut fds, &p.inbox)?;
        let fs = Filesystem::decode(d)?;
        p.fds = table;
        p.fs = fs;
        p.set_exe(d.bytes()?);
        let (signals, thread_signals) = signal::Signals::decode(d)?;
        *p.signals() = signals;
        for limit in p.rlimits.get_mut().unwrap().iter_mut() {
            *limit = [d.u64()?, d.u64()?];
        }
        let mut t = Thread::main(&p);
        t.signals = thread_signals;
        t.comm = d.bytes()?;
        p.dumpable.store(d.bool()?, Ordering::Relaxed);
        p.no_new_privs.store(d.bool()?, Ordering::Relaxed);
        *p.file_maps() = super::mem::FileMaps::decode(d)?;
        // The host process is new: its umask may not be the parent's now.
        p.set_umask(d.u64()? as u32);
        p.parent_ctl = Some(ctl);
        Ok((p, t))
    }

    /// A child forked by thread `t` in the same host process (many processes
    /// in one VM), as pid `pid`, and its one thread: what
    /// [`Process::encode_for_fork`] would pass on, but open files shared
    /// rather than sent (one open file description each, as on Linux:
    /// see [`FdTable::fork`]), and the same kernel.
    pub fn fork_in_host(&self, t: &Thread, pid: i64) -> (Process, Thread) {
        let mut p = Process::new("/");
        p.pid = pid;
        p.set_kernel(self.kernel.clone());
        p.fds = self.fds.fork(&p.inbox);
        // The rest as a 1:1 fork carries it, through the same encoding.
        let mut e = Encoder::default();
        self.fs.encode(&mut e);
        self.signals().encode(&t.signals, &mut e);
        self.file_maps().encode(&mut e);
        let mut d = Decoder(&e.0);
        let decoded = (|| -> Result<_, Truncated> { Ok((Filesystem::decode(&mut d)?, signal::Signals::decode(&mut d)?, super::mem::FileMaps::decode(&mut d)?)) })();
        let (fs, (signals, thread_signals), file_maps) = decoded.expect("a fork's own encoding");
        p.fs = fs;
        *p.signals() = signals;
        *p.file_maps() = file_maps;
        p.set_exe(self.exe());
        p.set_comm(&self.comm());
        *p.rlimits.get_mut().unwrap() = *self.rlimits.lock().unwrap_or_else(|e| e.into_inner());
        p.dumpable.store(self.dumpable.load(Ordering::Relaxed), Ordering::Relaxed);
        p.no_new_privs.store(self.no_new_privs.load(Ordering::Relaxed), Ordering::Relaxed);
        p.umask.store(self.umask(), Ordering::Relaxed);
        *p.arg_area.get_mut().unwrap() = self.arg_area();
        let mut child = Thread::main(&p);
        child.signals = thread_signals;
        child.comm = t.comm.clone();
        (p, child)
    }

    /// In the parent, once the child `pid` exists: CLONE_PARENT_SETTID, and
    /// keep its control socket for `wait`.
    /// `seen` is anything already read from the socket (while waiting for a
    /// vfork child).
    pub fn forked_parent<M: GuestMemory + ?Sized>(&self, mem: &M, fork: &Fork, pid: i64, ctl: OwnedFd, seen: &[u8]) {
        if let Some(addr) = fork.parent_tid {
            let _ = mem.write_pod(addr, &(pid as i32));
        }
        let mut children = self.children();
        if let Some(i) = seen.iter().position(|&b| b == ctl::KILLED)
            && let Some(&sig) = seen.get(i + 1)
        {
            children.killed.insert(pid, sig);
        }
        children.ctl.insert(pid, ctl);
    }

    /// In the child, with its memory in place: CLONE_CHILD_SETTID.
    pub fn forked_child<M: GuestMemory + ?Sized>(&mut self, t: &mut Thread, mem: &M, fork: &Fork) {
        self.pid = std::process::id() as i64;
        t.tid = self.pid;
        if let Some(addr) = fork.child_tid {
            let _ = mem.write_pod(addr, &(self.pid as i32));
        }
    }

    /// Tell the parent something over the control socket (if forked).
    pub fn tell_parent(&self, msg: &[u8]) {
        if let Some(sock) = &self.parent_ctl {
            unsafe { libc::send(sock.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        }
    }

    /// The core replaced the program (other threads gone, `t` the one left):
    /// close-on-exec fds close, handlers reset, and a vforking parent may run
    /// on.
    pub fn exec_done(&self, t: &mut Thread, exe: Vec<u8>, comm: &[u8]) {
        self.set_comm(comm);
        self.fds.close_on_exec();
        self.file_maps().clear();
        self.signals().reset_on_exec(&mut t.signals);
        t.comm.clear();
        t.clear_child_tid = 0;
        self.set_exe(exe);
        self.tell_parent(&[ctl::EXEC]);
    }

    fn children(&self) -> std::sync::MutexGuard<'_, Children> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `setpgid(pid, pgid)`. A fork child is a fresh `macwl-run` the host sees
/// as having exec'd, and POSIX doesn't let a parent change an exec'd child's
/// group (EACCES), though Linux lets a parent do it to its forked children
/// (shells do, for each pipeline). So such a child is asked to do it itself,
/// over its control socket, and the parent waits for the answer.
pub(super) fn host_setpgid(p: &Process, pid: i64, pgid: i64) -> Result<i64, i64> {
    if unsafe { libc::setpgid(pid as i32, pgid as i32) } == 0 {
        return Ok(0);
    }
    let e = super::last_errno();
    if e != errno::EACCES || pid <= 0 || pid == p.pid {
        return Err(e);
    }
    let mut children = p.children();
    let Some(sock) = children.ctl.get(&pid).map(|s| s.as_raw_fd()) else { return Err(e) };
    let mut msg = vec![ctl::SETPGID];
    // pgid 0 means the child's own pid, from either side.
    msg.extend_from_slice(&(if pgid == 0 { pid } else { pgid } as i32).to_le_bytes());
    if unsafe { libc::send(sock, msg.as_ptr().cast(), msg.len(), 0) } != msg.len() as isize {
        return Err(errno::ESRCH);
    }
    match children.read_messages(pid, true) {
        Some(0) => Ok(0),
        Some(err) => Err(err as i64),
        // No answer: the child exec'd a Mac program (which doesn't keep the
        // socket), and Linux says EACCES once a child has exec'd.
        None => Err(errno::EACCES),
    }
}

/// The child's side of [`setpgid`]: answer one request (`msg` is its i32).
pub fn answer_setpgid(msg: [u8; 4]) -> [u8; 5] {
    let pgid = i32::from_le_bytes(msg);
    let r = unsafe { libc::setpgid(0, pgid) };
    let err = if r == 0 { 0 } else { super::last_errno() as i32 };
    let mut out = [ctl::SETPGID_DONE, 0, 0, 0, 0];
    out[1..].copy_from_slice(&err.to_le_bytes());
    out
}

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
/// CLONE_NEWNS and the other namespace flags.
const CLONE_NEW_ANY: u64 = 0x7e02_0080;

/// `clone(flags, stack, parent_tid, tls, child_tid)` (arm64 argument order).
pub(super) fn clone(p: &Process, flags: u64, stack: u64, ptid: u64, tls: u64, ctid: u64) -> Result<Action, i64> {
    if flags & CLONE_THREAD != 0 {
        // A thread shares memory and signal handlers (Linux insists), and in
        // practice files and the filesystem view too.
        const NEEDED: u64 = CLONE_VM | CLONE_SIGHAND;
        if flags & NEEDED != NEEDED || flags & (CLONE_VFORK | CLONE_NEW_ANY) != 0 {
            return Err(errno::EINVAL);
        }
        if flags & (CLONE_FILES | CLONE_FS) != CLONE_FILES | CLONE_FS {
            p.unimplemented(format!("threads with their own fd table or cwd (clone flags {flags:#x})"));
        }
        return Ok(Action::Spawn(Spawn {
            stack,
            tls: (flags & CLONE_SETTLS != 0).then_some(tls),
            parent_tid: (flags & CLONE_PARENT_SETTID != 0 && ptid != 0).then_some(ptid),
            child_settid: (flags & CLONE_CHILD_SETTID != 0 && ctid != 0).then_some(ctid),
            child_cleartid: (flags & CLONE_CHILD_CLEARTID != 0 && ctid != 0).then_some(ctid),
        }));
    }
    if flags & CLONE_NEW_ANY != 0 {
        p.unimplemented(format!("namespaces (clone flags {flags:#x})"));
        return Err(errno::EPERM);
    }
    if flags & CLONE_PIDFD != 0 {
        p.unimplemented("pidfds".into());
        return Err(errno::EINVAL);
    }
    let vfork = flags & CLONE_VFORK != 0;
    // A child sharing the parent's memory without vfork: where processes
    // share a VM, it runs in the same address space (CLONE_VM alone);
    // a Mac process can't share another's.
    let vm = flags & CLONE_VM != 0;
    let shares_memory = vm && !vfork && p.kernel.shares_memory();
    // Sharing files or handlers with a child in another process only works
    // out when the child is about to exec (posix_spawn's vfork), so
    // approximate that case with a copy.
    if flags & (CLONE_VM | CLONE_FILES | CLONE_FS | CLONE_SIGHAND) != 0 && !vfork && !(shares_memory && flags & (CLONE_FILES | CLONE_FS | CLONE_SIGHAND) == 0) {
        p.unimplemented(format!("clone sharing state without CLONE_VFORK (flags {flags:#x})"));
        return Err(errno::EINVAL);
    }
    Ok(Action::Fork(Fork {
        stack,
        tls: (flags & CLONE_SETTLS != 0).then_some(tls),
        vfork,
        vm: vm || vfork,
        parent_tid: (flags & CLONE_PARENT_SETTID != 0 && ptid != 0).then_some(ptid),
        child_tid: (flags & CLONE_CHILD_SETTID != 0 && ctid != 0).then_some(ctid),
        exit_signal: flags & 0xff,
    }))
}

/// Linux's limits on `execve` arguments.
const MAX_ARG_STRLEN: usize = 32 * 4096;
const ARG_MAX: usize = 2 << 20;
/// How many `#!` interpreters may name further scripts.
const MAX_INTERP_DEPTH: u32 = 4;

/// A NUL-terminated string of up to MAX_ARG_STRLEN bytes.
fn read_arg<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let at = addr + out.len() as u64;
        // Don't read past a page boundary in one go: the string may end
        // just before an unmapped page.
        let n = chunk.len().min((4096 - (at % 4096)) as usize);
        mem.read(at, &mut chunk[..n]).map_err(i64::from)?;
        if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..end]);
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() >= MAX_ARG_STRLEN {
            return Err(errno::E2BIG);
        }
    }
}

/// A NULL-terminated array of string pointers (NULL itself: empty).
fn read_args<M: GuestMemory + ?Sized>(mem: &M, mut addr: u64, total: &mut usize) -> Result<Vec<Vec<u8>>, i64> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    loop {
        let ptr: u64 = mem.read_pod(addr)?;
        if ptr == 0 {
            return Ok(out);
        }
        let s = read_arg(mem, ptr)?;
        *total += s.len() + 1 + 8;
        if *total > ARG_MAX {
            return Err(errno::E2BIG);
        }
        out.push(s);
        addr += 8;
    }
}

pub(super) fn execve<M: GuestMemory + ?Sized>(p: &Process, mem: &M, path: u64, argv: u64, envp: u64) -> Result<Action, i64> {
    let path = path_arg(mem, path)?;
    let mut total = 0;
    let argv = read_args(mem, argv, &mut total)?;
    let envp = read_args(mem, envp, &mut total)?;
    let search = envp.iter().find_map(|e| e.strip_prefix(b"PATH=")).unwrap_or(b"/bin:/usr/bin").to_vec();
    let comm = path.rsplit(|&b| b == b'/').next().unwrap_or_default().to_vec();
    Ok(match load_program(p, path, argv, &search, 0)? {
        Program::Linux(exec) => Action::Exec(Exec { envp, comm, ..exec }),
        Program::Mac(path, argv) => Action::ExecHost(HostExec { path, argv, envp }),
    })
}

/// Whether `head` starts a Mach-O file: thin 64-bit, or universal. (A Java
/// class file starts like a universal one, but where the architecture count
/// goes it has its version, which is larger.)
fn is_mach_o(head: &[u8]) -> bool {
    match head {
        [0xcf, 0xfa, 0xed, 0xfe, ..] => true,
        [0xca, 0xfe, 0xba, 0xbe | 0xbf, a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]) < 20,
        _ => false,
    }
}

pub(super) fn mac_programs_allowed() -> bool {
    std::env::var_os("MACWL_MAC_PROGRAMS").is_some_and(|v| v == "1")
}

const AT_FDCWD: u64 = -100i64 as u64;

/// The host path of the executable regular file at guest `path`.
fn executable(p: &Process, path: &[u8]) -> Result<std::ffi::CString, i64> {
    let Target::Host(host) = p.fs.resolve(&p.fds, AT_FDCWD, path, true)?.target else { return Err(errno::EACCES) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::stat(host.as_ptr(), &mut st) } as i64)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(errno::EACCES);
    }
    ok(unsafe { libc::access(host.as_ptr(), libc::X_OK) } as i64)?;
    Ok(host)
}

/// For `#!/usr/bin/env NAME` (or `/bin/env`) with a plain NAME: the first
/// executable NAME on `search` (the guest's PATH), as Linux `env` would find
/// it. On the Mac's own `/`, `env` is the Mac's, which would search the
/// Mac's PATH and miss the Linux NAME the script was written for.
fn env_lookup(p: &Process, interp: &[u8], arg: &[u8], search: &[u8]) -> Option<Vec<u8>> {
    let plain = !arg.is_empty() && !arg.starts_with(b"-") && !arg.iter().any(|&b| matches!(b, b'/' | b'=' | b' ' | b'\t'));
    if !matches!(interp, b"/usr/bin/env" | b"/bin/env") || !plain {
        return None;
    }
    search.split(|&b| b == b':').find_map(|dir| {
        let dir = if dir.is_empty() { &b"."[..] } else { dir };
        let candidate = [dir, b"/", arg].concat();
        executable(p, &candidate).is_ok().then_some(candidate)
    })
}

/// Read the program at guest `path`, following `#!` lines: its ELF image, its
/// final argv, and its guest path (`envp` left empty) — or the Mac program
/// it turns out to be. `search` is the guest's PATH.
fn load_program(p: &Process, path: Vec<u8>, argv: Vec<Vec<u8>>, search: &[u8], depth: u32) -> Result<Program, i64> {
    if let Target::Proc(rest) = p.fs.resolve(&p.fds, AT_FDCWD, &path, true)?.target {
        // /proc/self/exe and the like: the program itself.
        return if rest == b"self/exe" { load_program(p, p.exe(), argv, search, depth) } else { Err(errno::EACCES) };
    }
    let host = executable(p, &path)?;
    let host_path = std::path::Path::new(std::ffi::OsStr::from_bytes(host.as_bytes()));
    // The start says what it is; only an ELF file is read whole.
    let mut file = std::fs::File::open(host_path).map_err(|e| errno::from_io(&e))?;
    let mut head = Vec::with_capacity(256);
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, 256), &mut head).map_err(|e| errno::from_io(&e))?;
    if is_mach_o(&head) {
        return if p.kernel.mac_programs() { Ok(Program::Mac(host, argv)) } else { Err(errno::ENOEXEC) };
    }
    let data = head;

    if let Some(line) = data.strip_prefix(b"#!") {
        if depth >= MAX_INTERP_DEPTH {
            return Err(errno::ELOOP);
        }
        // Linux reads up to 256 bytes of the line: the interpreter, then
        // everything after it as one optional argument.
        let line = &line[..line.len().min(254)];
        let line = line.split(|&b| b == b'\n').next().unwrap_or_default();
        let line = line.trim_ascii();
        let split = line.iter().position(|&b| b == b' ' || b == b'\t').unwrap_or(line.len());
        let (interp, arg) = (line[..split].to_vec(), line[split..].trim_ascii().to_vec());
        if interp.is_empty() {
            return Err(errno::ENOEXEC);
        }
        if let Some(found) = env_lookup(p, &interp, &arg, search) {
            // What env would exec: NAME as argv[0], then the script.
            let new_argv = [vec![arg, path], argv.into_iter().skip(1).collect()].concat();
            return load_program(p, found, new_argv, search, depth + 1);
        }
        let mut new_argv = vec![interp.clone()];
        if !arg.is_empty() {
            new_argv.push(arg);
        }
        new_argv.push(path);
        new_argv.extend(argv.into_iter().skip(1));
        return load_program(p, interp, new_argv, search, depth + 1);
    }
    if !data.starts_with(b"\x7fELF") {
        return Err(errno::ENOEXEC);
    }
    let mut data = data;
    std::io::Read::read_to_end(&mut file, &mut data).map_err(|e| errno::from_io(&e))?;
    let canonical = std::fs::canonicalize(host_path).map_err(|e| errno::from_io(&e))?;
    let exe = p.fs.guest_path_of_host(canonical.as_os_str().as_bytes());
    Ok(Program::Linux(Exec { elf: data, argv, envp: Vec::new(), exe, comm: Vec::new() }))
}

/// Whether macOS's ncurses (5.7) would find a terminfo entry for `term` it
/// can read, looking where it does: `TERMINFO`, `~/.terminfo`,
/// `TERMINFO_DIRS` (all from `envp`), then the system's.
pub(super) fn mac_has_terminfo(term: &[u8], envp: &[Vec<u8>]) -> bool {
    let var = |name: &[u8]| envp.iter().find_map(|e| e.strip_prefix(name)?.strip_prefix(b"="));
    let Some(&first) = term.first() else { return false };
    if term.contains(&b'/') {
        return false;
    }
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    dirs.extend(var(b"TERMINFO").map(<[u8]>::to_vec));
    dirs.extend(var(b"HOME").map(|h| [h, b"/.terminfo"].concat()));
    dirs.extend(var(b"TERMINFO_DIRS").into_iter().flat_map(|d| d.split(|&b| b == b':')).map(<[u8]>::to_vec));
    dirs.push(b"/usr/share/terminfo".to_vec());
    // Entries sit under the first letter, or (as macOS has them) its hex code.
    let subdirs = [vec![first], format!("{first:02x}").into_bytes()];
    dirs.iter().filter(|d| !d.is_empty()).any(|dir| {
        subdirs.iter().any(|sub| {
            let path = [&dir[..], b"/", sub, b"/", term].concat();
            let mut magic = [0u8; 2];
            std::fs::File::open(std::ffi::OsStr::from_bytes(&path))
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
                .is_ok_and(|()| magic == [0x1a, 0x01])
        })
    })
}

// Darwin's `posix_spawn_file_actions_addchdir_np` (macOS 10.15), which the
// libc crate doesn't declare.
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(actions: *mut libc::posix_spawn_file_actions_t, path: *const libc::c_char) -> libc::c_int;
}

/// Run the Mac program `exec` in place of this process, as `execve` does
/// (see the module docs for what it inherits), with `blocked` (a guest
/// thread's mask) as its signal mask. Returns only if it couldn't, with the
/// Linux errno.
///
/// The core calls this only once the guest's threads have ended and the VM
/// is destroyed and its memory unmapped, so the Mac program starts clean
/// (ROADMAP.md, item 13).
pub fn exec_host(p: &Process, blocked: u64, exec: &HostExec) -> i64 {
    use std::ffi::CString;
    let cstrings = |v: &[Vec<u8>]| v.iter().filter_map(|s| CString::new(s.clone()).ok()).collect::<Vec<_>>();
    let argv = cstrings(&exec.argv);
    // The guest's environment, but the Mac's PATH and SHELL.
    let from_mac = ["PATH", "SHELL"];
    let mut env: Vec<Vec<u8>> = exec
        .envp
        .iter()
        .filter(|e| !from_mac.iter().any(|v| e.starts_with(v.as_bytes()) && e.get(v.len()) == Some(&b'=')))
        .cloned()
        .collect();
    for var in from_mac {
        if let Some(value) = std::env::var_os(var) {
            env.push([var.as_bytes(), b"=", value.as_bytes()].concat());
        }
    }
    // macOS's ncurses has no entry for foot (and can't read Linux's, which
    // are in ncurses 6's format): give it one it knows that foot is
    // compatible with.
    if let Some(term) = env.iter_mut().find(|e| e.starts_with(b"TERM="))
        && !mac_has_terminfo(&term[5..], &exec.envp)
    {
        *term = b"TERM=xterm-256color".to_vec();
    }
    let env = cstrings(&env);
    let ptrs = |v: &[CString]| v.iter().map(|s| s.as_ptr()).chain([std::ptr::null()]).collect::<Vec<_>>();
    let (argv_p, env_p) = (ptrs(&argv), ptrs(&env));
    let Ok(cwd) = p.fs.host_path(&p.fs.cwd()) else { return errno::ENOENT };

    // Guest fd n becomes fd n: by way of fds above all of them (so no move
    // overwrites a source still to come), which are closed again. Everything
    // else closes (CLOEXEC_DEFAULT).
    let inherited = p.fds.inherited();
    let above = inherited.iter().map(|&(n, host)| (n as RawFd).max(host)).max().unwrap_or(0) + 1;
    let ignored = p.signals().ignored();
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawnattr_init(&mut attr);
        // With POSIX_SPAWN_SETEXEC, close-on-exec fds are gone before the
        // file actions run (dup2 from one fails with EBADF), and the host
        // fds all are: clear the flag on those that are passed on.
        // CLOEXEC_DEFAULT still closes them once they've been moved.
        for &(_, host) in &inherited {
            libc::fcntl(host, libc::F_SETFD, 0);
        }
        for (i, &(_, host)) in inherited.iter().enumerate() {
            libc::posix_spawn_file_actions_adddup2(&mut actions, host, above + i as RawFd);
        }
        for (i, &(n, _)) in inherited.iter().enumerate() {
            libc::posix_spawn_file_actions_adddup2(&mut actions, above + i as RawFd, n as RawFd);
            libc::posix_spawn_file_actions_addclose(&mut actions, above + i as RawFd);
        }
        posix_spawn_file_actions_addchdir_np(&mut actions, cwd.as_ptr());
        // Signals: the guest's ignored ones stay ignored, the rest are as a
        // new program finds them; the guest's mask carries over.
        let (mut default, mut mask): (libc::sigset_t, libc::sigset_t) = (0, 0);
        libc::sigemptyset(&mut default);
        libc::sigemptyset(&mut mask);
        let mut ignore = Vec::new();
        for sig in 1..=31u64 {
            let Some(host) = signal::to_darwin(sig) else { continue };
            if ignored & (1 << (sig - 1)) != 0 {
                ignore.push(host);
            } else {
                libc::sigaddset(&mut default, host);
            }
            if blocked & (1 << (sig - 1)) != 0 {
                libc::sigaddset(&mut mask, host);
            }
        }
        let mut saved = Vec::new();
        for &sig in &ignore {
            let mut old: libc::sigaction = std::mem::zeroed();
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = libc::SIG_IGN;
            libc::sigaction(sig, &sa, &mut old);
            saved.push((sig, old));
        }
        libc::posix_spawnattr_setsigdefault(&mut attr, &default);
        libc::posix_spawnattr_setsigmask(&mut attr, &mask);
        let flags = libc::POSIX_SPAWN_SETEXEC | libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK;
        libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short);
        let r = libc::posix_spawn(std::ptr::null_mut(), exec.path.as_ptr(), &actions, &attr, argv_p.as_ptr().cast(), env_p.as_ptr().cast());
        // Still here: it didn't start.
        for &(_, host) in &inherited {
            libc::fcntl(host, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        for (sig, old) in saved {
            libc::sigaction(sig, &old, std::ptr::null_mut());
        }
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        errno::from_io(&std::io::Error::from_raw_os_error(r))
    }
}

/// Linux `wait4` options, and the `__W*` flags that don't matter to us.
const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WCONTINUED: u64 = 8;
const WALL_ETC: u64 = 0xe000_0000;

pub(super) fn wait4<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, pid: u64, status: u64, options: u64, rusage: u64) -> Result<i64, i64> {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WALL_ETC) != 0 {
        return Err(errno::EINVAL);
    }
    let Some(got) = p.kernel.wait4(p, t, pid as i64, options)? else { return Ok(0) };
    if status != 0 {
        mem.write_pod(status, &got.status)?;
    }
    if rusage != 0 {
        mem.write(rusage, &got.rusage)?;
    }
    Ok(got.pid)
}

/// [`wait4`] when each Linux process is a host process: the host's `wait4`,
/// its status in Linux's terms. It doesn't block in the host's: it looks
/// (WNOHANG) and waits for SIGCHLD to the host (watched in the thread's
/// waker kqueue, registered before the first look), so that the wait parks
/// like any other, and a wake can't be missed.
pub(super) fn host_wait4(p: &Process, t: &Thread, pid: i64, options: u64) -> Result<Option<Waited>, i64> {
    if options & WNOHANG != 0 {
        return host_wait4_now(p, pid, options);
    }
    t.waker.watch_signal(libc::SIGCHLD);
    loop {
        if let Some(got) = host_wait4_now(p, pid, options | WNOHANG)? {
            return Ok(Some(got));
        }
        if p.exiting() || p.signal_ready(t) {
            return Err(errno::EINTR);
        }
        super::block::wait_woken(t, None)?;
    }
}

fn host_wait4_now(p: &Process, pid: i64, options: u64) -> Result<Option<Waited>, i64> {
    let mut host_opts = 0;
    if options & WNOHANG != 0 {
        host_opts |= libc::WNOHANG;
    }
    if options & WUNTRACED != 0 {
        host_opts |= libc::WUNTRACED;
    }
    if options & WCONTINUED != 0 {
        host_opts |= libc::WCONTINUED;
    }
    // The host process may have children the guest didn't fork (the runtime's
    // spare, see the core's fork): they don't count, and are skipped if they
    // turn up.
    let any = pid <= 0;
    if any && p.children().ctl.is_empty() {
        return Err(errno::ECHILD);
    }
    let mut st = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let got = loop {
        let got = ok(unsafe { libc::wait4(pid as i32, &mut st, host_opts, &mut ru) } as i64)?;
        if got == 0 {
            return Ok(None);
        }
        if !any || p.children().ctl.contains_key(&got) {
            break got;
        }
    };
    let linux = p.children().status(got, st);
    if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) {
        p.children().reaped(got);
    }
    Ok(Some(Waited { pid: got, status: linux, rusage: super::kernel::rusage_bytes(&ru) }))
}

/// `waitid(idtype, id, infop, options, rusage)`.
pub(super) fn waitid<M: GuestMemory + ?Sized>(p: &Process, t: &Thread, mem: &M, idtype: u64, id: u64, infop: u64, options: u64, rusage: u64) -> Result<i64, i64> {
    const WSTOPPED: u64 = 2;
    const WEXITED: u64 = 4;
    const WNOWAIT: u64 = 0x0100_0000;
    const SIGCHLD: i32 = 17;
    if options & !(WNOHANG | WSTOPPED | WEXITED | WCONTINUED | WNOWAIT | WALL_ETC) != 0
        || options & (WSTOPPED | WEXITED | WCONTINUED) == 0
    {
        return Err(errno::EINVAL);
    }
    const P_PIDFD: u64 = 3;
    // A pidfd names the child: as P_PID for it.
    let (idtype, id) = if idtype == P_PIDFD { (1, super::pidfd::pid_of(p, id)? as u64) } else { (idtype, id) };
    if idtype > 2 {
        return Err(errno::EINVAL);
    }
    let mut out = [0u8; 128];
    if let Some(child) = p.kernel.waitid(p, t, idtype, id, options)? {
        let fields: [(usize, i32); 5] = [(0, SIGCHLD), (8, child.code), (16, child.pid as i32), (20, child.uid as i32), (24, child.status)];
        for (off, v) in fields {
            out[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    if infop != 0 {
        mem.write(infop, &out)?;
    }
    if rusage != 0 {
        mem.write(rusage, &[0u8; 144])?;
    }
    Ok(0)
}

/// [`waitid`] when each Linux process is a host process: the host's
/// `waitid`, in Linux's terms; waiting as [`host_wait4`] does.
pub(super) fn host_waitid(p: &Process, t: &Thread, idtype: u64, id: u64, options: u64) -> Result<Option<ChildInfo>, i64> {
    if options & WNOHANG != 0 {
        return host_waitid_now(p, idtype, id, options);
    }
    t.waker.watch_signal(libc::SIGCHLD);
    loop {
        if let Some(got) = host_waitid_now(p, idtype, id, options | WNOHANG)? {
            return Ok(Some(got));
        }
        if p.exiting() || p.signal_ready(t) {
            return Err(errno::EINTR);
        }
        super::block::wait_woken(t, None)?;
    }
}

fn host_waitid_now(p: &Process, idtype: u64, id: u64, options: u64) -> Result<Option<ChildInfo>, i64> {
    const WSTOPPED: u64 = 2;
    const WEXITED: u64 = 4;
    const WNOWAIT: u64 = 0x0100_0000;
    const CLD_EXITED: i32 = 1;
    const CLD_KILLED: i32 = 2;
    const CLD_DUMPED: i32 = 3;
    let idtype = match idtype {
        0 => libc::P_ALL,
        1 => libc::P_PID,
        _ => libc::P_PGID,
    };
    let mut host_opts = 0;
    for (l, d) in [(WNOHANG, libc::WNOHANG), (WSTOPPED, libc::WSTOPPED), (WEXITED, libc::WEXITED), (WCONTINUED, libc::WCONTINUED), (WNOWAIT, libc::WNOWAIT)] {
        if options & l != 0 {
            host_opts |= d;
        }
    }
    // As in wait4: only children the guest forked count.
    let any = idtype != libc::P_PID;
    if any && p.children().ctl.is_empty() {
        return Err(errno::ECHILD);
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        ok(unsafe { libc::waitid(idtype, id as libc::id_t, &mut info, host_opts) } as i64)?;
        if !any || info.si_pid == 0 || p.children().ctl.contains_key(&(info.si_pid as i64)) {
            break;
        }
        if options & WNOWAIT != 0 {
            // Reap the stranger, or it would come back every time.
            unsafe { libc::waitpid(info.si_pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
    }
    if info.si_pid == 0 {
        return Ok(None);
    }
    let pid = info.si_pid as i64;
    let (mut code, mut status) = (info.si_code, info.si_status);
    match code {
        CLD_EXITED if status >= 128 => {
            let mut children = p.children();
            children.poll(pid);
            if let Some(&sig) = children.killed.get(&pid) {
                (code, status) = (CLD_KILLED, sig as i32);
            }
        }
        CLD_KILLED | CLD_DUMPED => status = signal::from_darwin(status) as i32,
        CLD_EXITED => {}
        _ => status = signal::from_darwin(status) as i32, // stopped, continued, trapped
    }
    if options & WNOWAIT == 0 && matches!(code, CLD_EXITED | CLD_KILLED | CLD_DUMPED) {
        p.children().reaped(pid);
    }
    Ok(Some(ChildInfo { pid, uid: info.si_uid, code, status }))
}
