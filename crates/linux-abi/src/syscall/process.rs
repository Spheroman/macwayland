//! Processes: `clone` (fork and vfork), `execve`, `wait4`/`waitid`.
//!
//! The dispatcher checks and decodes these calls; the core does the parts
//! that touch the CPU and memory — making the child ([`Action::Fork`]) or
//! replacing the program ([`Action::Exec`]) — and calls back into
//! [`Process`] around them. The process table, and so `wait`, is the
//! kernel's ([`Kernel`](super::Kernel)).
//!
//! `execve` of a Mac program (a Mach-O file, or a script whose interpreter is
//! one) runs it, as WSL runs Windows programs: the core spawns it as a Mac
//! process of its own and this process stands in for it
//! ([`Action::MacStandIn`]), keeping the pid, so the parent's `wait`, the
//! terminal and job control carry on as for any child. It gets the guest's
//! fds (by their guest numbers), working directory, signal mask and ignored
//! signals, and environment — except `PATH` and `SHELL`, which are the Mac's
//! (those macwl-run started with), so the programs it runs in turn are Mac
//! ones too ([`mac_spawn`]). It sees the Mac's file system, which under
//! `--root` isn't the guest's. It is off unless `MACWL_MAC_PROGRAMS=1`
//! (otherwise such an `execve` fails with ENOEXEC). See ROADMAP.md, item 13.

use std::{os::{fd::OwnedFd, unix::ffi::OsStrExt}, sync::atomic::Ordering};

use super::{
    kernel::MacPrograms,
    ok, path_arg, signal, Action, Process, Thread,
};
use crate::{
    errno,
    fs::{Filesystem, Target},
    memory::{GuestMemory, GuestMemoryExt},
    wire::{Decoder, Encoder, Truncated},
};

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

/// A Mac program to run for this process: see [`mac_spawn`].
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

impl Process {
    /// A child forked by thread `t` (in the same host process), as pid
    /// `pid`, and its one thread: everything it inherits, open files shared
    /// (one open file description each, as on Linux: see [`crate::fs::FdTable::fork`]),
    /// and the same kernel.
    pub fn fork_in_host(&self, t: &Thread, pid: i64) -> (Process, Thread) {
        let mut p = Process::new("/");
        p.pid = pid;
        p.set_kernel(self.kernel.clone());
        p.fds = self.fds.fork(&p.inbox);
        // The rest through its encoding: a copy.
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

    /// Everything process `p`'s thread `t` keeps as it execs `exec` in a
    /// host of its own (docs/processes.md, step 9, "Moving at execve"): its
    /// filesystem view and working directory, dispositions (handlers reset,
    /// as by an exec; ignored signals kept) and `t`'s mask, rlimits, umask,
    /// `prctl` flags, the program's path, name, argv and environment, and
    /// the guest numbers of the fds it inherits, and its pending signals
    /// (the process's and `t`'s, which an exec keeps). The fds the host
    /// can hand over as they are (files, pipes, sockets, terminals) are
    /// returned, to be passed alongside. An epoll set, eventfd, timerfd,
    /// signalfd or inotify instance is a kqueue of this host's: its state
    /// goes instead (an epoll set's interests, an eventfd's count, a
    /// timer's interval and time left, a signalfd's mask, an inotify
    /// instance's watches, but not events it hasn't read), and the new
    /// host makes one like it. A pidfd is passed as it is, with the pid it
    /// names: its far end is held where that process is, so it still reads
    /// as ready when the process ends.
    pub fn encode_for_move(&self, t: &Thread, exec: &Exec, e: &mut Encoder) -> Vec<(usize, std::sync::Arc<crate::fs::OpenFile>)> {
        self.fs.encode(e);
        let mut signals = self.signals().clone();
        let mut ts = t.signals.spawned();
        signals.reset_on_exec(&mut ts);
        signals.encode(&ts, e);
        for [soft, hard] in *self.rlimits.lock().unwrap_or_else(|e| e.into_inner()) {
            e.u64(soft).u64(hard);
        }
        e.u64(self.umask() as u64).bool(self.dumpable.load(Ordering::Relaxed)).bool(self.no_new_privs.load(Ordering::Relaxed));
        e.bytes(&exec.exe).bytes(&exec.comm);
        for list in [&exec.argv, &exec.envp] {
            e.u64(list.len() as u64);
            for item in list {
                e.bytes(item);
            }
        }
        let fds: Vec<_> = self.fds.inherited_files().into_iter().filter(|(_, f)| matches!(f.kind, crate::fs::FileKind::Plain | crate::fs::FileKind::PidFd(_))).collect();
        e.u64(fds.len() as u64);
        for (n, f) in &fds {
            // The pid a pidfd names (0: a plain fd).
            e.u64(*n as u64).i64(if let crate::fs::FileKind::PidFd(pid) = f.kind { pid } else { 0 });
        }
        // The kqueue-backed fds it inherits, by their state.
        let made: Vec<_> = self.fds.inherited_files().into_iter().filter(|(_, f)| !matches!(f.kind, crate::fs::FileKind::Plain | crate::fs::FileKind::PidFd(_))).collect();
        e.u64(made.len() as u64);
        for (n, f) in &made {
            e.u64(*n as u64);
            let nonblock = super::kfd::nonblocking(f);
            match &f.kind {
                crate::fs::FileKind::Epoll(set) => {
                    let set = set.lock().unwrap_or_else(|e| e.into_inner());
                    e.u64(1).u64(set.len() as u64);
                    for (&fd, i) in set.iter() {
                        e.i64(fd as i64).u64(i.events as u64).u64(i.data);
                    }
                }
                crate::fs::FileKind::EventFd(state) => {
                    let (count, semaphore) = state.lock().unwrap_or_else(|e| e.into_inner()).state();
                    e.u64(2).u64(count).bool(semaphore);
                }
                crate::fs::FileKind::TimerFd(timer) => {
                    let (clock, interval, left) = timer.lock().unwrap_or_else(|e| e.into_inner()).state();
                    e.u64(3).u64(clock).u64(interval.as_nanos() as u64).bool(left.is_some()).u64(left.map_or(0, |l| l.as_nanos() as u64));
                }
                crate::fs::FileKind::Inotify(inotify) => {
                    let watches = inotify.state();
                    e.u64(4).u64(watches.len() as u64);
                    for (wd, path, mask) in watches {
                        e.i64(wd as i64).bytes(&path).u64(mask as u64);
                    }
                }
                crate::fs::FileKind::SignalFd(signalfd) => drop(e.u64(5).u64(signalfd.mask())),
                crate::fs::FileKind::Plain | crate::fs::FileKind::PidFd(_) => unreachable!(),
            }
            e.bool(nonblock);
        }
        let process = self.signals().pending.queued();
        let thread = t.signals.pending.lock().unwrap_or_else(|e| e.into_inner()).queued();
        for list in [process, thread] {
            e.u64(list.len() as u64);
            for i in list {
                e.u64(i.signo).i64(i.code as i64).i64(i.pid as i64).u64(i.uid as u64).i64(i.status as i64).u64(i.addr);
            }
        }
        fds
    }

    /// The new host's side of [`Process::encode_for_move`]: the process
    /// (with `fds`, the passed files, in order, at their guest numbers), its
    /// thread, the program's guest path, argv and environment, and the
    /// signals pending for it (true: for its thread), for the caller to
    /// post once it runs.
    #[allow(clippy::type_complexity)]
    pub fn decode_moved(d: &mut Decoder, fds: Vec<OwnedFd>) -> Result<(Process, Thread, Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<(bool, signal::SigInfo)>), Truncated> {
        let mut p = Process::new("/");
        p.fs = Filesystem::decode(d)?;
        let (signals, thread_signals) = signal::Signals::decode(d)?;
        *p.signals() = signals;
        for limit in p.rlimits.get_mut().unwrap().iter_mut() {
            *limit = [d.u64()?, d.u64()?];
        }
        p.umask.store(d.u64()? as u32 & 0o777, Ordering::Relaxed);
        p.dumpable.store(d.bool()?, Ordering::Relaxed);
        p.no_new_privs.store(d.bool()?, Ordering::Relaxed);
        let (exe, comm) = (d.bytes()?, d.bytes()?);
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            for _ in 0..d.count(8)? {
                list.push(d.bytes()?);
            }
        }
        let [argv, envp] = lists;
        let n = d.count(16)?;
        if n != fds.len() {
            return Err(Truncated);
        }
        for host in fds {
            let (n, pid) = (d.u64()? as usize, d.i64()?);
            let mut entry = crate::fs::FdEntry::new(host);
            if pid != 0 {
                entry.kind = crate::fs::FileKind::PidFd(pid);
            }
            p.fds.insert_at(n, entry);
        }
        // Its kqueue-backed fds, made again here; then what its epoll sets
        // watched, once every fd is in place. (One that can't be made is
        // left out, as if it had been closed.)
        let mut epolls = Vec::new();
        for _ in 0..d.count(16)? {
            let n = d.u64()? as usize;
            let ns = std::time::Duration::from_nanos;
            let made = match d.u64()? {
                1 => {
                    let watched = (0..d.count(24)?)
                        .map(|_| Ok((d.i64()? as i32, crate::fs::Interest { events: d.u64()? as u32, data: d.u64()? })))
                        .collect::<Result<Vec<_>, Truncated>>()?;
                    d.bool()?;
                    epolls.push((n, watched));
                    super::epoll::new_instance()
                }
                2 => {
                    let (count, semaphore, nonblock) = (d.u64()?, d.bool()?, d.bool()?);
                    super::kfd::eventfd_copy(count, semaphore, nonblock)
                }
                3 => {
                    let (clock, interval, has_left, left, nonblock) = (d.u64()?, ns(d.u64()?), d.bool()?, ns(d.u64()?), d.bool()?);
                    super::kfd::timerfd_copy(clock, interval, has_left.then_some(left), nonblock)
                }
                4 => {
                    let watches = (0..d.count(24)?).map(|_| Ok((d.i64()? as i32, d.bytes()?, d.u64()? as u32))).collect::<Result<Vec<_>, Truncated>>()?;
                    super::inotify::inotify_copy(watches, d.bool()?)
                }
                5 => {
                    let (mask, nonblock) = (d.u64()?, d.bool()?);
                    super::signalfd_copy(&p.inbox, mask, nonblock)
                }
                _ => return Err(Truncated),
            };
            if let Ok(entry) = made {
                p.fds.insert_at(n, entry);
            }
        }
        for (epfd, watched) in epolls {
            for (fd, interest) in watched {
                super::epoll::rewatch(&p.fds, epfd as u64, fd, interest);
            }
        }
        p.set_exe(exe.clone());
        p.set_comm(&comm);
        let mut t = Thread::main(&p);
        t.signals = thread_signals;
        // Its pending signals, the process's and the thread's.
        let mut pending = Vec::new();
        for to_thread in [false, true] {
            for _ in 0..d.count(48)? {
                let info = signal::SigInfo {
                    signo: d.u64()?,
                    code: d.i64()? as i32,
                    pid: d.i64()? as i32,
                    uid: d.u64()? as u32,
                    status: d.i64()? as i32,
                    addr: d.u64()?,
                    timer: None,
                };
                if !(1..=64).contains(&info.signo) {
                    return Err(Truncated);
                }
                pending.push((to_thread, info));
            }
        }
        Ok((p, t, exe, argv, envp, pending))
    }

    /// The core replaced the program (other threads gone, `t` the one left):
    /// close-on-exec fds close, and handlers reset.
    pub fn exec_done(&self, t: &mut Thread, exe: Vec<u8>, comm: &[u8]) {
        self.set_comm(comm);
        self.fds.close_on_exec();
        self.file_maps().clear();
        self.signals().reset_on_exec(&mut t.signals);
        t.comm.clear();
        t.clear_child_tid = 0;
        self.set_exe(exe);
    }
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
        Program::Mac(path, argv) => match p.kernel.mac_programs() {
            MacPrograms::StandIn => Action::MacStandIn(HostExec { path, argv, envp }),
            MacPrograms::Refused => return Err(errno::ENOEXEC),
        },
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

pub fn mac_programs_allowed() -> bool {
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
        return if p.kernel.mac_programs() != MacPrograms::Refused { Ok(Program::Mac(host, argv)) } else { Err(errno::ENOEXEC) };
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

/// The environment a Mac program gets from guest environment `envp`: the
/// guest's, but with the Mac's `PATH` and `SHELL` (those macwl-run started
/// with, so the programs it runs in turn are Mac ones too), and a `TERM`
/// macOS's ncurses can read.
pub fn mac_environment(envp: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let from_mac = ["PATH", "SHELL"];
    let mut env: Vec<Vec<u8>> =
        envp.iter().filter(|e| !from_mac.iter().any(|v| e.starts_with(v.as_bytes()) && e.get(v.len()) == Some(&b'='))).cloned().collect();
    for var in from_mac {
        if let Some(value) = std::env::var_os(var) {
            env.push([var.as_bytes(), b"=", value.as_bytes()].concat());
        }
    }
    // macOS's ncurses has no entry for foot (and can't read Linux's, which
    // are in ncurses 6's format): give it one it knows that foot is
    // compatible with.
    if let Some(term) = env.iter_mut().find(|e| e.starts_with(b"TERM="))
        && !mac_has_terminfo(&term[5..], envp)
    {
        *term = b"TERM=xterm-256color".to_vec();
    }
    env
}

/// What a Mac program started for a process ([`Action::MacStandIn`]) gets
/// from it, as `execve` would pass it on:
/// see the module docs. The core spawns it as a process of its own.
#[derive(Debug)]
pub struct MacSpawn {
    /// Its host path, argv and environment (the Mac's `PATH` and `SHELL`).
    pub path: std::ffi::CString,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
    /// The guest's working directory, as a host path.
    pub cwd: std::ffi::CString,
    /// The fds it inherits: guest number and open file.
    pub fds: Vec<(usize, std::sync::Arc<crate::fs::OpenFile>)>,
    /// Linux signals (bit `n - 1` for signal `n`) the process ignores, which
    /// stay ignored, and the exec'ing thread's mask, which carries over.
    pub ignored: u64,
    pub blocked: u64,
}

/// What process `p` passes on to the Mac program `exec`, from a thread
/// with signal mask `blocked`.
pub fn mac_spawn(p: &Process, blocked: u64, exec: &HostExec) -> Result<MacSpawn, i64> {
    let cwd = p.fs.host_path(&p.fs.cwd()).map_err(|_| errno::ENOENT)?;
    Ok(MacSpawn {
        path: exec.path.clone(),
        argv: exec.argv.clone(),
        env: mac_environment(&exec.envp),
        cwd,
        fds: p.fds.inherited_files(),
        ignored: p.signals().ignored(),
        blocked,
    })
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
