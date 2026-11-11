//! What the dispatcher asks of the world outside its process: other
//! processes (`kill`, `wait`), process groups and sessions, terminals'
//! foreground groups, CPU accounting.
//!
//! The runtime keeps the process table of its host (docs/processes.md) and
//! answers these from it with a `Kernel` of its own. [`HostKernel`] is for
//! a process on its own (this crate's tests): no children, and the host's
//! own calls for the rest.

use std::{os::fd::AsRawFd, time::Duration};

use crate::fs::OpenFile;

use super::{ok, signal, Process, Thread};
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
    /// Its controlling terminal, a pty's number, and that terminal's
    /// foreground group (-1 without one).
    pub tty: Option<u32>,
    pub tpgid: i64,
    /// For a process whose memory the kernel doesn't hold (a Mac program
    /// standing in for it): its command line (`cmdline`'s NUL-separated
    /// strings) and resident 4 KiB pages.
    pub cmdline: Option<Vec<u8>>,
    pub rss_pages: u64,
}

/// How `execve` of a Mac program goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacPrograms {
    /// It fails with ENOEXEC (Mac programs are off).
    Refused,
    /// The Mac program is spawned as a process of its own, and this one
    /// stands in for it in the process table (`Action::MacStandIn`): the
    /// host never execs.
    StandIn,
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
    /// The foreground process group of terminal `file` (`TIOCGPGRP`).
    fn tcgetpgrp(&self, p: &Process, file: &OpenFile) -> Result<i64, i64>;
    /// Make `pgrp` the foreground group of terminal `file` (`TIOCSPGRP`), by
    /// thread `t`.
    fn tcsetpgrp(&self, p: &Process, t: &Thread, file: &OpenFile, pgrp: i64) -> Result<i64, i64>;
    /// The session terminal `file` controls (`TIOCGSID`).
    fn tcgetsid(&self, _p: &Process, file: &OpenFile) -> Result<i64, i64> {
        ok(unsafe { libc::tcgetsid(file.host.as_raw_fd()) } as i64)
    }
    /// Make terminal `file` the caller's controlling terminal (`TIOCSCTTY`;
    /// `steal`: from another session).
    fn set_ctty(&self, _p: &Process, file: &OpenFile, steal: bool) -> Result<i64, i64> {
        ok(unsafe { libc::ioctl(file.host.as_raw_fd(), libc::TIOCSCTTY as u64, steal as libc::c_int) } as i64).map(|_| 0)
    }
    /// Give up the controlling terminal `file` (`TIOCNOTTY`).
    fn drop_ctty(&self, _p: &Process, file: &OpenFile) -> Result<i64, i64> {
        ok(unsafe { libc::ioctl(file.host.as_raw_fd(), libc::TIOCNOTTY) } as i64).map(|_| 0)
    }
    /// Terminal `file` was just opened (`noctty`: with O_NOCTTY, or as a
    /// master): a session leader with no controlling terminal gets it as
    /// one. Here the host's open did that, if anything.
    fn tty_opened(&self, _p: &Process, _file: &OpenFile, _noctty: bool) {}
    /// Whether host opens of terminals must not make them the host
    /// process's controlling terminal (the kernel keeps controlling
    /// terminals itself).
    fn keeps_ttys(&self) -> bool {
        false
    }
    /// The terminal `/dev/tty` opens: None for the host's own `/dev/tty`
    /// (the host process is the Linux one), else its host path, or ENXIO
    /// with none.
    fn ctty_path(&self, _p: &Process) -> Option<Result<Vec<u8>, i64>> {
        None
    }
    /// Thread `t` is about to do `op` to terminal `file`: Linux's
    /// job-control check for a process outside the foreground group of its
    /// controlling terminal (SIGTTIN, SIGTTOU or EIO; EINTR here once the
    /// signal is sent, and the call is made again). Ok(false): make the
    /// host call at once, without waiting for the terminal to be ready, for
    /// the host's driver to check.
    ///
    /// Here the host's driver does it, as the call is made; but a read
    /// waits for input before it's made, which a background process's
    /// shouldn't: it reads at once, and the driver stops it.
    fn tty_access(&self, _p: &Process, _t: &Thread, file: &OpenFile, op: super::TtyOp) -> Result<bool, i64> {
        if op != super::TtyOp::Read {
            return Ok(true);
        }
        let fg = unsafe { libc::tcgetpgrp(file.host.as_raw_fd()) };
        Ok(fg < 0 || fg == unsafe { libc::getpgrp() })
    }
    /// Write `data` to pty master `file` as typed input, raising the
    /// keyboard's signals for the terminal's foreground group (None: write it
    /// as any file, the host's driver raising them).
    fn write_master(&self, _p: &Process, _file: &OpenFile, _data: &[u8]) -> Option<Result<i64, i64>> {
        None
    }
    /// Terminal `file`'s size changed: SIGWINCH to its foreground group.
    /// Here the host's driver sends it.
    fn winsize_changed(&self, _p: &Process, _file: &OpenFile) {}
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
    /// Whether and how `execve` runs Mac programs: see [`MacPrograms`].
    fn mac_programs(&self) -> MacPrograms;
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
    /// the host's own clock for this process says (a process on its own,
    /// where the host process is the Linux one), and other processes' are
    /// out of reach.
    fn process_cpu_time(&self, p: &Process, pid: i64) -> Result<Option<Duration>, i64> {
        if pid == p.pid { Ok(None) } else { Err(errno::EINVAL) }
    }
    /// The processes `/proc` lists: every one the kernel has (for a process
    /// on its own, the caller alone: others are Mac processes).
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
            tty: None,
            tpgid: -1,
            cmdline: None,
            rss_pages: 0,
        })
    }
    /// Run `f` with another process's state and memory, for its `/proc`
    /// files: ESRCH if it isn't one the kernel runs (a zombie, or for a
    /// process on its own any other).
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

/// A Linux process on its own, as this crate's tests run one: it has no
/// children (it can't fork: that's the runtime's), and pids, groups,
/// sessions and signals are the host's own.
#[derive(Debug, Default)]
pub struct HostKernel;

impl Kernel for HostKernel {
    fn getppid(&self, _p: &Process) -> i64 {
        unsafe { libc::getppid() as i64 }
    }

    fn getpgid(&self, _p: &Process, pid: i64) -> Result<i64, i64> {
        ok(unsafe { libc::getpgid(pid as i32) } as i64)
    }

    fn setpgid(&self, _p: &Process, pid: i64, pgid: i64) -> Result<i64, i64> {
        ok(unsafe { libc::setpgid(pid as i32, pgid as i32) } as i64)
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
            // aren't Linux ones (the test runner), so the caller only.
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
        // Another process's thread. Its tids aren't the host's, and macOS
        // can't signal another process's thread, so the signal goes to its
        // process: the best we can do. What mustn't happen is ESRCH for a
        // thread that's alive: wineserver takes that to mean the thread has
        // died and ends it on its side while it still runs, after which it
        // releases an object once too often and its heap is corrupt.
        let pid = match tgid {
            Some(pid) if Process::may_be_thread_of(pid, tid) => pid,
            Some(_) => return Err(errno::ESRCH),
            // `tkill` alone: a main thread's tid is its pid; another
            // thread's says too little of its process.
            None if !Process::is_other_thread(tid) && tid != p.pid => tid,
            None => return Err(errno::ESRCH),
        };
        self.kill(p, pid, sig)
    }

    fn wait4(&self, _p: &Process, _t: &Thread, _pid: i64, _options: u64) -> Result<Option<Waited>, i64> {
        Err(errno::ECHILD)
    }

    fn waitid(&self, _p: &Process, _t: &Thread, _idtype: u64, _id: u64, _options: u64) -> Result<Option<ChildInfo>, i64> {
        Err(errno::ECHILD)
    }

    fn tcgetpgrp(&self, _p: &Process, file: &OpenFile) -> Result<i64, i64> {
        ok(unsafe { libc::tcgetpgrp(file.host.as_raw_fd()) } as i64)
    }

    fn tcsetpgrp(&self, _p: &Process, _t: &Thread, file: &OpenFile, pgrp: i64) -> Result<i64, i64> {
        ok(unsafe { libc::tcsetpgrp(file.host.as_raw_fd(), pgrp as i32) } as i64).map(|_| 0)
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
        // The host process is the Linux process: what it creates gets the
        // mask from the host too.
        unsafe { libc::umask(mask as libc::mode_t) };
    }

    fn mac_programs(&self) -> MacPrograms {
        MacPrograms::Refused
    }
}

/// A host `struct rusage` as Linux's bytes (the layouts are the same).
pub(super) fn rusage_bytes(ru: &libc::rusage) -> [u8; RUSAGE_SIZE] {
    let mut out = [0u8; RUSAGE_SIZE];
    out.copy_from_slice(unsafe { std::slice::from_raw_parts((ru as *const libc::rusage).cast::<u8>(), RUSAGE_SIZE) });
    out
}
