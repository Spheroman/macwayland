//! What the dispatcher asks of the world outside its process: other
//! processes (`kill`, `wait`), process groups and sessions, terminals'
//! foreground groups, CPU accounting.
//!
//! Today every Linux process is a Mac process, so these are the host's own
//! calls ([`HostKernel`]). With many processes in one VM (docs/processes.md)
//! the runtime keeps the process table instead, and answers them from it
//! with a `Kernel` of its own; the dispatcher doesn't change.

use std::{os::fd::RawFd, time::Duration};

use super::{ok, process, signal, Process, Thread};
use crate::errno;

/// A child that changed state, as `wait4` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waited {
    pub pid: i64,
    /// The Linux wait status (exit code << 8, a signal number, 0x7f for a
    /// stop, 0xffff for a continue).
    pub status: i32,
    /// Its `struct rusage` (Linux's layout, which Darwin shares).
    pub rusage: [u8; RUSAGE_SIZE],
}

/// A child that changed state, as `waitid` reports it (`siginfo_t` fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildInfo {
    pub pid: i64,
    pub uid: u32,
    /// `CLD_EXITED`, `CLD_KILLED`, `CLD_DUMPED`, `CLD_STOPPED`, ...
    pub code: i32,
    /// The exit code, or the Linux signal.
    pub status: i32,
}

/// `sizeof(struct rusage)` on arm64 Linux: two timevals and fourteen longs.
pub const RUSAGE_SIZE: usize = 144;

/// What `/proc/<pid>` says of a process that the process table knows (the
/// rest comes from the process itself: see [`Kernel::with_process`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcStatus {
    /// Its name (`comm`), which a zombie keeps.
    pub comm: Vec<u8>,
    pub ppid: i64,
    pub pgid: i64,
    pub sid: i64,
    /// `R` running, `S` sleeping, `T` stopped, `Z` zombie.
    pub state: u8,
    pub threads: u64,
    /// CPU time, in ns: its own, and its reaped children's.
    pub user_ns: u64,
    pub sys_ns: u64,
    pub children_user_ns: u64,
    pub children_sys_ns: u64,
    /// When it started, in ns since the machine booted.
    pub start_ns: u64,
}

/// A clock reading a process's CPU time, from any thread.
pub type CpuClock = Box<dyn Fn() -> Duration + Send + Sync>;

/// The process model the dispatcher runs against. Arguments and results are
/// Linux's (pids, signal numbers, wait options and statuses); errors are
/// Linux errnos.
pub trait Kernel: Send + Sync + std::fmt::Debug {
    /// `getppid`.
    fn getppid(&self, p: &Process) -> i64;
    /// `getpgid(pid)` (0: the caller).
    fn getpgid(&self, p: &Process, pid: i64) -> Result<i64, i64>;
    /// `setpgid(pid, pgid)`.
    fn setpgid(&self, p: &Process, pid: i64, pgid: i64) -> Result<i64, i64>;
    /// `getsid(pid)` (0: the caller).
    fn getsid(&self, p: &Process, pid: i64) -> Result<i64, i64>;
    /// `setsid`.
    fn setsid(&self, p: &Process) -> Result<i64, i64>;
    /// `kill(target, sig)` for a target other than the caller itself: another
    /// process, the caller's group (0), a process group (negative), or every
    /// process it may signal (-1). `sig` 0 only checks.
    fn kill(&self, p: &Process, target: i64, sig: u64) -> Result<i64, i64>;
    /// `tgkill(tgid, tid, sig)` (`tkill` without a `tgid`) for a thread the
    /// caller's process doesn't have: one of another process.
    fn tgkill(&self, p: &Process, tgid: Option<i64>, tid: i64, sig: u64) -> Result<i64, i64>;
    /// `wait4(pid, options)` by thread `t`: a child that changed state, or
    /// None (WNOHANG and nothing to report).
    fn wait4(&self, p: &Process, t: &Thread, pid: i64, options: u64) -> Result<Option<Waited>, i64>;
    /// `waitid(idtype, id, options)`: likewise.
    fn waitid(&self, p: &Process, t: &Thread, idtype: u64, id: u64, options: u64) -> Result<Option<ChildInfo>, i64>;
    /// The foreground process group of the terminal open as `host_fd`.
    fn tcgetpgrp(&self, p: &Process, host_fd: RawFd) -> Result<i64, i64>;
    /// Make `pgrp` the foreground group of the terminal open as `host_fd`.
    fn tcsetpgrp(&self, p: &Process, host_fd: RawFd, pgrp: i64) -> Result<i64, i64>;
    /// A stop signal's default action: the process stops until continued.
    fn stop(&self, p: &Process, sig: u64);
    /// `getrusage(who)`: RUSAGE_SELF (0), RUSAGE_CHILDREN (-1), RUSAGE_THREAD
    /// (1), as Linux's `struct rusage`.
    fn rusage(&self, p: &Process, who: i64) -> Result<[u8; RUSAGE_SIZE], i64>;
    /// A clock of the CPU time the process has used, user time or user and
    /// system time (for ITIMER_VIRTUAL and ITIMER_PROF), which a timer's
    /// thread reads.
    fn cpu_clock(&self, p: &Process, user_only: bool) -> CpuClock;
    /// The process's umask changed to `mask` (the runtime applies it to the
    /// modes of what the guest creates; a kernel may mirror it).
    fn umask_changed(&self, p: &Process, mask: u32);
    /// Whether the host process is this Linux process alone, so that its
    /// signal dispositions, and its threads' masks for the job-control
    /// signals, are mirrored onto the host's (and the host's own default
    /// actions act for it). Not when a host runs many processes: signals
    /// are all in memory then.
    fn host_signals(&self) -> bool {
        true
    }
    /// Whether `execve` may run Mac programs (`Action::ExecHost`): only when
    /// the host process is the Linux process alone, and can become one.
    fn mac_programs(&self) -> bool;
    /// A host fd that reads as ready once process `pid` has ended, for a
    /// pidfd (`pidfd_open`). Here: a kqueue watching the Mac process.
    fn pidfd(&self, _p: &Process, pid: i64) -> Result<std::os::fd::OwnedFd, i64> {
        super::pidfd::host_exit_watch(pid)
    }
    /// Another process's open file `fd` (`pidfd_getfd`): out of reach of a
    /// Mac process.
    fn process_file(&self, _p: &Process, _pid: i64, _fd: u64) -> Result<std::sync::Arc<crate::fs::OpenFile>, i64> {
        Err(errno::ENOSYS)
    }
    /// `PR_SET_CHILD_SUBREAPER`: orphaned descendants become the caller's,
    /// not init's. A Mac process can't be one: only turning it off works.
    fn set_child_subreaper(&self, _p: &Process, on: bool) -> Result<(), i64> {
        if on { Err(errno::EINVAL) } else { Ok(()) }
    }
    /// Process `pid`'s CPU time (user and system), for its CPU clock; None:
    /// the host's own clock for this process says (1:1, where the host
    /// process is the Linux one), and other processes' are out of reach.
    fn process_cpu_time(&self, p: &Process, pid: i64) -> Result<Option<Duration>, i64> {
        if pid == p.pid { Ok(None) } else { Err(errno::EINVAL) }
    }
    /// The processes `/proc` lists: every one the kernel has (in 1:1 mode,
    /// the caller alone: others are Mac processes).
    fn pids(&self, p: &Process) -> Vec<i64> {
        vec![p.pid]
    }
    /// What the process table says of `pid` (for `/proc/<pid>`).
    fn proc_status(&self, p: &Process, pid: i64) -> Result<ProcStatus, i64> {
        if pid != p.pid {
            return Err(errno::ESRCH);
        }
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let mut cru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe {
            libc::getrusage(libc::RUSAGE_SELF, &mut ru);
            libc::getrusage(libc::RUSAGE_CHILDREN, &mut cru);
        }
        let ns = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
        Ok(ProcStatus {
            comm: p.comm(),
            ppid: unsafe { libc::getppid() } as i64,
            pgid: unsafe { libc::getpgrp() } as i64,
            sid: unsafe { libc::getsid(0) } as i64,
            state: b'R',
            threads: p.thread_count().max(1) as u64,
            user_ns: ns(ru.ru_utime),
            sys_ns: ns(ru.ru_stime),
            children_user_ns: ns(cru.ru_utime),
            children_sys_ns: ns(cru.ru_stime),
            start_ns: p.started_ns,
        })
    }
    /// Run `f` with another process's state and memory, for its `/proc`
    /// files: ESRCH if it isn't one the kernel runs (a zombie, or in 1:1
    /// mode any other).
    fn with_process(&self, _p: &Process, _pid: i64, _f: &mut dyn FnMut(&Process, &dyn crate::memory::GuestMemory)) -> Result<(), i64> {
        Err(errno::ESRCH)
    }
    /// Whether a child can share its parent's address space without vfork
    /// (`clone` with CLONE_VM alone): when the processes share a VM.
    fn shares_memory(&self) -> bool {
        false
    }
    /// `PR_GET_CHILD_SUBREAPER`.
    fn child_subreaper(&self, _p: &Process) -> Result<bool, i64> {
        Ok(false)
    }
}

/// Each Linux process is a host process: pids, groups, sessions, `wait` and
/// signals are the host's own, adjusted where Linux differs (a fork child's
/// control socket: see `process.rs`).
#[derive(Debug, Default)]
pub struct HostKernel;

impl Kernel for HostKernel {
    fn getppid(&self, _p: &Process) -> i64 {
        unsafe { libc::getppid() as i64 }
    }

    fn getpgid(&self, _p: &Process, pid: i64) -> Result<i64, i64> {
        ok(unsafe { libc::getpgid(pid as i32) } as i64)
    }

    fn setpgid(&self, p: &Process, pid: i64, pgid: i64) -> Result<i64, i64> {
        process::host_setpgid(p, pid, pgid)
    }

    fn getsid(&self, _p: &Process, pid: i64) -> Result<i64, i64> {
        ok(unsafe { libc::getsid(pid as i32) } as i64)
    }

    fn setsid(&self, _p: &Process) -> Result<i64, i64> {
        ok(unsafe { libc::setsid() } as i64)
    }

    fn kill(&self, p: &Process, target: i64, sig: u64) -> Result<i64, i64> {
        match target {
            // The caller's group: the Mac process group holds processes that
            // aren't Linux ones (the runtime itself, a spare), so the caller
            // only.
            0 => signal::kill_self(p, sig),
            -1 => {
                // Every process the user owns: on a Mac that's the whole login
                // session.
                p.unimplemented("kill(-1, ...) (signalling every process)".into());
                Err(errno::EPERM)
            }
            _ => {
                let darwin = if sig == 0 { 0 } else { signal::to_darwin(sig).ok_or(errno::EINVAL)? };
                ok(unsafe { libc::kill(target as i32, darwin) } as i64)
            }
        }
    }

    fn tgkill(&self, p: &Process, tgid: Option<i64>, tid: i64, sig: u64) -> Result<i64, i64> {
        // Another process's thread: its process is the best we can do (its
        // tids aren't the host's).
        if tgid.is_none() && tid == p.pid {
            return Err(errno::ESRCH);
        }
        self.kill(p, tid, sig)
    }

    fn wait4(&self, p: &Process, _t: &Thread, pid: i64, options: u64) -> Result<Option<Waited>, i64> {
        process::host_wait4(p, pid, options)
    }

    fn waitid(&self, p: &Process, _t: &Thread, idtype: u64, id: u64, options: u64) -> Result<Option<ChildInfo>, i64> {
        process::host_waitid(p, idtype, id, options)
    }

    fn tcgetpgrp(&self, _p: &Process, host_fd: RawFd) -> Result<i64, i64> {
        ok(unsafe { libc::tcgetpgrp(host_fd) } as i64)
    }

    fn tcsetpgrp(&self, _p: &Process, host_fd: RawFd, pgrp: i64) -> Result<i64, i64> {
        ok(unsafe { libc::tcsetpgrp(host_fd, pgrp as i32) } as i64).map(|_| 0)
    }

    fn stop(&self, _p: &Process, _sig: u64) {
        unsafe { libc::raise(libc::SIGSTOP) };
    }

    fn rusage(&self, _p: &Process, who: i64) -> Result<[u8; RUSAGE_SIZE], i64> {
        let who = match who {
            0 => libc::RUSAGE_SELF,
            -1 => libc::RUSAGE_CHILDREN,
            1 => libc::RUSAGE_SELF, // RUSAGE_THREAD: the host has no per-thread figures
            _ => return Err(errno::EINVAL),
        };
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        ok(unsafe { libc::getrusage(who, &mut ru) } as i64)?;
        Ok(rusage_bytes(&ru))
    }

    fn cpu_clock(&self, _p: &Process, user_only: bool) -> CpuClock {
        Box::new(move || {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
            if user_only { tv(ru.ru_utime) } else { tv(ru.ru_utime) + tv(ru.ru_stime) }
        })
    }

    fn umask_changed(&self, _p: &Process, mask: u32) {
        // The host process is the Linux process: what it creates (and a Mac
        // program it becomes) gets the mask from the host too.
        unsafe { libc::umask(mask as libc::mode_t) };
    }

    fn mac_programs(&self) -> bool {
        process::mac_programs_allowed()
    }
}

/// A host `struct rusage` as Linux's bytes (the layouts are the same).
pub(super) fn rusage_bytes(ru: &libc::rusage) -> [u8; RUSAGE_SIZE] {
    let mut out = [0u8; RUSAGE_SIZE];
    out.copy_from_slice(unsafe { std::slice::from_raw_parts((ru as *const libc::rusage).cast::<u8>(), RUSAGE_SIZE) });
    out
}
