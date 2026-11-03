//! The system call dispatcher: one Linux arm64 system call → its host
//! implementation.
//!
//! Status: memory (`brk`, `mmap`, ...), files and directories, an emulated
//! `/proc`, pipes, `ppoll`, and sockets including fd passing. Everything else
//! returns -ENOSYS and is logged once per call number, so running a program
//! shows what to implement next.

mod block;
pub(crate) use block::can_block;
pub use block::wait_woken;
mod epoll;
mod futex;
mod inotify;
mod io;
mod kernel;
mod kfd;
mod mem;
mod misc;
mod net;
mod path;
pub(crate) mod pidfd;
mod process;
mod signal;
mod signalfd;
mod timer;
pub mod tty;
pub use tty::{identify as tty_of, TtyId, TtyOp};
#[cfg(test)]
mod tests;

pub(crate) use epoll::{new_instance as new_epoll, rewatch};
pub(crate) use inotify::{inotify_copy, inotify_pending, inotify_read};
pub(crate) use kfd::{eventfd_copy, timerfd_copy};
pub use kfd::now as kevent_now;
pub(crate) use signalfd::{signalfd_copy, signalfd_read};
pub use signalfd::{HostSignalWait, SignalFd};
pub use futex::{begin_wait as futex_begin_wait, finish_wait as futex_finish_wait, FutexWait, Waker};
pub use kernel::{ChildInfo, CpuClock, HostKernel, Kernel, ProcStatus, Waited, RUSAGE_SIZE};
pub use inotify::Inotify;
pub use kfd::{EventFd, TimerFd};
pub use process::{answer_setpgid, ctl, exec_host, Exec, Fork, HostExec, Spawn};
pub use signal::{catch_host_signals, set_kick, set_yield_check, Delivery, HandlerCall, Inbox, SigInfo, StackT};
/// Signal numbers between Linux's and Darwin's (a client forwards the Mac
/// side's to its Linux process, and ends as that process did).
pub use signal::{from_darwin as signal_from_darwin, to_darwin as signal_to_darwin};

use std::{
    collections::BTreeSet,
    os::fd::{AsFd, OwnedFd},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

use crate::{
    abi,
    errno,
    fs::{FdEntry, FdTable, Filesystem},
    memory::{GuestMemory, GuestMemoryExt},
    nr,
};

/// Linux's IOV_MAX.
const IOV_MAX: u64 = 1024;

/// Largest single read or write we stage through a host buffer. Longer ones
/// come back short, which Linux allows, so a bogus guest length can't make the
/// host allocate without bound.
const MAX_IO: u64 = 16 << 20;

/// Per-process state the dispatcher works on (everything except memory, which
/// the core owns and passes in). Shared by the process's threads.
#[derive(Debug)]
pub struct Process {
    pub fds: FdTable,
    pub fs: Filesystem,
    pub pid: i64,
    /// Guest path of the running executable, for `/proc/self/exe`.
    exe: Mutex<Vec<u8>>,
    signals: Mutex<signal::Signals>,
    rlimits: Mutex<[[u64; 2]; 16]>,
    /// `prctl` flags.
    dumpable: AtomicBool,
    no_new_privs: AtomicBool,
    /// Forked children not yet waited for.
    children: Mutex<process::Children>,
    /// If this process was forked: the control socket to its parent.
    parent_ctl: Option<OwnedFd>,
    /// Threads waiting in `futex`.
    futexes: futex::Futexes,
    /// `exit_group` (or a fatal signal, or `execve`) is ending every thread.
    exiting: AtomicBool,
    /// POSIX timers.
    timers: Mutex<timer::Timers>,
    /// Thread ids handed out so far.
    tids: std::sync::atomic::AtomicI64,
    /// Unimplemented calls (and ioctls, sockopts) already reported, so each
    /// is logged once.
    unimplemented: Mutex<BTreeSet<String>>,
    /// Which files are mapped where.
    file_maps: Mutex<mem::FileMaps>,
    /// What the process asks of the world outside it: other processes,
    /// groups, sessions, accounting (the host's own, in 1:1 mode).
    pub kernel: Arc<dyn Kernel>,
    /// Signals and timer expiries from outside its system calls, and who
    /// to wake for them.
    pub(crate) inbox: Arc<Inbox>,
    /// The file mode creation mask.
    umask: AtomicU32,
    /// Interval timers (`setitimer`).
    itimers: Mutex<timer::ITimers>,
    /// Where its arguments and environment strings are in its memory
    /// (start and end of each), as the loader laid them out: what
    /// `/proc/<pid>/cmdline` and `environ` read, as Linux's do.
    arg_area: Mutex<[u64; 4]>,
    /// When it started, in ns since the machine booted (`/proc/<pid>/stat`).
    pub started_ns: u64,
    /// Its name (`/proc/<pid>/comm`): its program's, or what its main
    /// thread called itself (PR_SET_NAME).
    comm: Mutex<Vec<u8>>,
}

/// What other threads need of a thread: where to put signals for it, and how
/// to get its attention.
pub struct ThreadHandle {
    pub(crate) pending: Arc<Mutex<signal::Pending>>,
    pub(crate) waker: Arc<Waker>,
    pub(crate) wake: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for ThreadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThreadHandle")
    }
}

/// One thread's own state. The core owns one per guest thread and passes it
/// to [`dispatch`] with the shared [`Process`].
#[derive(Debug)]
pub struct Thread {
    pub tid: i64,
    pub(crate) signals: signal::ThreadSignals,
    /// The thread's name (`prctl(PR_SET_NAME)`); empty until set: derived
    /// from the executable.
    comm: Vec<u8>,
    /// How other threads wake this one from a blocking wait.
    pub waker: Arc<Waker>,
    /// `set_tid_address` / CLONE_CHILD_CLEARTID: zeroed, and futex-woken, when
    /// the thread exits (how `pthread_join` knows).
    pub clear_child_tid: u64,
    /// Its process's inbox, whose alerts reach its waker.
    inbox: Arc<Inbox>,
    /// What a call that parked waits for (see [`Thread::take_park`]).
    park: Mutex<Option<Park>>,
    /// The call is made again after a parked wait: the deadline that wait
    /// had (Some(None): none), which the call keeps rather than starting
    /// its timeout over.
    retry: Option<Option<std::time::Instant>>,
}

/// What a call waits for, having returned [`errno::PARK`] to the core: its
/// files (for the core to watch), its deadline, and (`futex`) the wait
/// still queued; wakes reach it through the waker's unpark hook. The core holds this while the task waits, and gives back
/// the deadline ([`Thread::set_retry`]) and the futex wait
/// ([`futex_finish_wait`]) when it makes the call again.
#[derive(Debug)]
pub struct Park {
    /// Host fds and the poll events it waits for (none: a wait in memory,
    /// which the waker's unpark hook ends).
    pub watch: Vec<(i32, i16)>,
    pub deadline: Option<std::time::Instant>,
    pub futex: Option<FutexWait>,
    /// The files the call uses, kept open while it waits.
    pub files: Vec<Arc<crate::fs::OpenFile>>,
}

impl Thread {
    /// The process's first thread (tid = pid).
    pub fn main(p: &Process) -> Thread {
        let waker = p.inbox.new_waker();
        Thread { tid: p.pid, signals: signal::ThreadSignals::default(), comm: Vec::new(), waker, clear_child_tid: 0, inbox: p.inbox.clone(), park: Mutex::new(None), retry: None }
    }

    /// A thread `self` creates (`clone` with CLONE_THREAD), as tid `tid`.
    pub fn spawn(&self, tid: i64) -> Thread {
        let waker = self.inbox.new_waker();
        Thread { tid, signals: self.signals.spawned(), comm: self.comm.clone(), waker, clear_child_tid: 0, inbox: self.inbox.clone(), park: Mutex::new(None), retry: None }
    }

    /// The signals it blocks (Linux numbering, bit n-1 for signal n).
    pub fn blocked_signals(&self) -> u64 {
        self.signals.blocked
    }

    /// What the call that returned [`errno::PARK`] waits for.
    pub fn take_park(&self) -> Option<Park> {
        self.park.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub(crate) fn set_park(&self, park: Park) {
        *self.park.lock().unwrap_or_else(|e| e.into_inner()) = Some(park);
    }

    /// The next call is one made again after a parked wait, which had
    /// `deadline` (None: it's a call like any other).
    pub fn set_retry(&mut self, deadline: Option<Option<std::time::Instant>>) {
        self.retry = deadline;
    }

    /// Whether the call is one made again after a parked wait.
    pub(crate) fn retrying(&self) -> bool {
        self.retry.is_some()
    }

    /// The deadline of a wait the call makes, `computed` from its timeout
    /// now: the first time, that; made again after a parked wait, the one
    /// it had then (a timeout runs from the call, not from each try).
    pub(crate) fn deadline(&self, computed: Option<std::time::Instant>) -> Option<std::time::Instant> {
        self.retry.unwrap_or(computed)
    }
}

impl Process {
    /// A process with no open fds, seeing the host directory `root` as `/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Process {
            fds: FdTable::default(),
            fs: Filesystem::new(root),
            pid: std::process::id() as i64,
            exe: Mutex::default(),
            signals: Mutex::default(),
            rlimits: Mutex::new(misc::default_rlimits()),
            dumpable: AtomicBool::new(true),
            no_new_privs: AtomicBool::new(false),
            children: Mutex::default(),
            parent_ctl: None,
            futexes: futex::Futexes::default(),
            exiting: AtomicBool::new(false),
            tids: std::sync::atomic::AtomicI64::new(0),
            timers: Mutex::default(),
            unimplemented: Mutex::default(),
            file_maps: Mutex::default(),
            kernel: Arc::new(HostKernel),
            inbox: Arc::default(),
            umask: AtomicU32::new(0o022),
            itimers: Mutex::default(),
            arg_area: Mutex::default(),
            comm: Mutex::default(),
            started_ns: (crate::hostinfo::uptime() * 1e9) as u64,
        }
    }

    /// Run against `kernel` from now on (a host's process table, when it
    /// runs many processes).
    pub fn set_kernel(&mut self, kernel: Arc<dyn Kernel>) {
        self.inbox.mirror_host.store(kernel.host_signals(), Ordering::Relaxed);
        self.kernel = kernel;
    }

    /// The file mode creation mask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::Relaxed)
    }

    /// Set the file mode creation mask (and tell the kernel), returning the
    /// old one.
    pub fn set_umask(&self, mask: u32) -> u32 {
        let old = self.umask.swap(mask & 0o777, Ordering::Relaxed);
        self.kernel.umask_changed(self, mask & 0o777);
        old
    }

    pub(crate) fn file_maps(&self) -> std::sync::MutexGuard<'_, mem::FileMaps> {
        self.file_maps.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The file mapped at `addr` (its guest path), and the offset in it.
    pub fn mapped_file(&self, addr: u64) -> Option<(Vec<u8>, u64)> {
        self.file_maps().at(addr).map(|(path, offset)| (path.to_vec(), offset))
    }

    /// Like [`Process::new`], with guest fds 0–2 duplicated from the host's
    /// stdin, stdout and stderr.
    pub fn with_host_stdio(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let p = Self::new(root);
        let stdio: [OwnedFd; 3] = [
            std::io::stdin().as_fd().try_clone_to_owned()?,
            std::io::stdout().as_fd().try_clone_to_owned()?,
            std::io::stderr().as_fd().try_clone_to_owned()?,
        ];
        for (fd, host) in stdio.into_iter().enumerate() {
            p.fds.insert_at(fd, FdEntry::new(host));
        }
        Ok(p)
    }

    /// A new thread id. The first thread's is the pid; others are numbers no
    /// host process is likely to have (Linux tids share the pid space), and
    /// unique within the process.
    pub fn new_tid(&self) -> i64 {
        // 65536 a process (more threads than that can be blocked at once),
        // skipping those still in use as they come round again.
        loop {
            let n = self.tids.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let tid = 0x1000_0000 + (self.pid % 0x800) * 0x1_0000 + (n % 0x1_0000);
            if !self.inbox.threads().contains_key(&tid) {
                return tid;
            }
        }
    }

    /// Its program's arguments and environment are at `[args.0, args.1)`
    /// and `[env.0, env.1)` (set as a program starts).
    pub fn set_arg_area(&self, args: (u64, u64), env: (u64, u64)) {
        *self.arg_area.lock().unwrap_or_else(|e| e.into_inner()) = [args.0, args.1, env.0, env.1];
    }

    /// Where its arguments and environment strings are: start and end of
    /// each.
    pub fn arg_area(&self) -> [u64; 4] {
        *self.arg_area.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Its name, at most 15 bytes (Linux's TASK_COMM_LEN less the NUL).
    pub fn set_comm(&self, name: &[u8]) {
        *self.comm.lock().unwrap_or_else(|e| e.into_inner()) = name[..name.len().min(15)].to_vec();
    }

    /// Its name: as set, else its executable's base name.
    pub fn comm(&self) -> Vec<u8> {
        let comm = self.comm.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !comm.is_empty() {
            return comm;
        }
        let exe = self.exe();
        let name = exe.rsplit(|&b| b == b'/').next().unwrap_or_default();
        name[..name.len().min(15)].to_vec()
    }

    /// Its threads' ids (those registered).
    pub fn thread_ids(&self) -> Vec<i64> {
        self.inbox.threads().keys().copied().collect()
    }

    /// The signals its threads block (none known here: masks are each
    /// thread's own), ignore, and catch, as `/proc/<pid>/status` shows them.
    pub fn signal_masks(&self) -> (u64, u64, u64) {
        let s = self.signals();
        (0, s.ignored_now(), s.caught())
    }

    /// Guest path of the running executable.
    pub fn exe(&self) -> Vec<u8> {
        self.exe.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_exe(&self, exe: Vec<u8>) {
        *self.exe.lock().unwrap_or_else(|e| e.into_inner()) = exe;
    }

    /// Make thread `t` reachable by the others: signals for it land in its
    /// pending set, and `wake` (which must be async-signal-safe-ish: it's
    /// called from other threads at any time) gets it to look.
    pub fn register_thread(&self, t: &Thread, wake: Box<dyn Fn() + Send + Sync>) {
        let handle = ThreadHandle { pending: t.signals.pending.clone(), waker: t.waker.clone(), wake };
        self.inbox.threads().insert(t.tid, handle);
    }

    pub fn unregister_thread(&self, tid: i64) {
        self.inbox.threads().remove(&tid);
    }

    /// How many threads are registered.
    pub fn thread_count(&self) -> usize {
        self.inbox.threads().len()
    }

    /// Wake every registered thread (to look at signals, or to exit).
    pub fn wake_all_threads(&self) {
        self.inbox.wake_threads();
    }

    /// Start ending every thread (`exit_group`, a fatal signal, `execve`).
    pub fn set_exiting(&self, on: bool) {
        self.exiting.store(on, std::sync::atomic::Ordering::Release);
        if on {
            self.wake_all_threads();
        }
    }

    pub fn exiting(&self) -> bool {
        self.exiting.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Wake up to `n` threads waiting in `futex` on `addr` (a thread's exit
    /// with CLONE_CHILD_CLEARTID: what `pthread_join` waits for).
    pub fn futex_wake(&self, addr: u64, n: u64) -> i64 {
        self.futexes.wake(addr, n, u32::MAX)
    }

    fn thread_pending(&self, tid: i64) -> Option<Arc<Mutex<signal::Pending>>> {
        self.inbox.threads().get(&tid).map(|h| h.pending.clone())
    }

    fn wake_thread(&self, tid: i64) {
        if let Some(h) = self.inbox.threads().get(&tid) {
            h.waker.wake();
            (h.wake)();
        }
    }

    /// Log `what` as unimplemented, once.
    fn unimplemented(&self, what: String) {
        let mut seen = self.unimplemented.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&what) {
            eprintln!("linux-abi: unimplemented {what}");
            seen.insert(what);
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Host signal handlers mustn't look for its inbox any more.
        self.stop_receiving_host_signals();
    }
}

/// What the core should do after a system call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Put this value in x0 and resume the guest.
    Return(i64),
    /// The process exited with this status (0–255): `exit_group`, which ends
    /// every thread.
    Exit(i32),
    /// The calling thread exited (`exit`), with this status if it's the
    /// last one.
    ExitThread(i32),
    /// `rt_sigreturn`: the core restores the state the signal frame on the
    /// guest stack saved.
    SigReturn,
    /// Make a child process (`clone` as fork or vfork). The child resumes
    /// with x0 = 0, the parent with its pid.
    Fork(Fork),
    /// Replace the program (`execve`). If the core can't load it, the call
    /// fails with the errno it reports.
    Exec(Exec),
    /// Replace the program with a Mac one (`execve` of a Mach-O file): the
    /// core tears down the VM and hands it to [`exec_host`], which returns
    /// only on failure.
    ExecHost(HostExec),
    /// Start a thread (`clone` with CLONE_THREAD). The core makes it (with
    /// [`Process::new_tid`]) and returns its tid, or fails with EAGAIN.
    Spawn(Spawn),
}

/// Run system call `nr` with arguments x0–x5, for thread `t`.
pub fn dispatch<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, nr: u64, args: [u64; 6]) -> Action {
    let action = dispatch_call(p, t, mem, nr, args);
    if action == Action::Return(-errno::PARK) {
        // The call is made again when the wait is over, and sets a mask
        // of its own again (ppoll's, sigsuspend's): the thread's own until
        // then. And the files it uses stay open while it waits.
        t.signals.end_wait_mask();
        let files = crate::fs::take_in_flight();
        if let Some(park) = t.park.get_mut().unwrap_or_else(|e| e.into_inner()) {
            park.files = files;
        }
    }
    crate::fs::end_call();
    t.sync_host_mask();
    action
}

fn dispatch_call<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, nr: u64, args: [u64; 6]) -> Action {
    let [a0, a1, a2, a3, a4, a5] = args;
    let ret = match nr {
        nr::EXIT => return Action::ExitThread((a0 & 0xff) as i32),
        nr::EXIT_GROUP => return Action::Exit((a0 & 0xff) as i32),
        nr::SET_TID_ADDRESS => {
            t.clear_child_tid = a0;
            Ok(t.tid)
        }
        // Only consulted when a thread dies holding a robust futex.
        nr::SET_ROBUST_LIST => Ok(0),
        nr::GETPID => Ok(p.pid),
        nr::GETTID => Ok(t.tid),
        nr::GETPPID => Ok(p.kernel.getppid(p)),
        nr::GETUID => Ok(unsafe { libc::getuid() } as i64),
        nr::GETEUID => Ok(unsafe { libc::geteuid() } as i64),
        nr::GETGID => Ok(unsafe { libc::getgid() } as i64),
        nr::GETEGID => Ok(unsafe { libc::getegid() } as i64),
        nr::GETGROUPS => misc::getgroups(mem, a0, a1),
        nr::GETPGID => p.kernel.getpgid(p, a0 as i32 as i64),
        nr::SETPGID => p.kernel.setpgid(p, a0 as i32 as i64, a1 as i32 as i64),
        nr::GETSID => p.kernel.getsid(p, a0 as i32 as i64),
        nr::SETSID => p.kernel.setsid(p),
        nr::SETUID => misc::setids(&[a0], false),
        nr::SETGID => misc::setids(&[a0], true),
        nr::SETREUID => misc::setids(&[a0, a1], false),
        nr::SETREGID => misc::setids(&[a0, a1], true),
        nr::SETRESUID => misc::setids(&[a0, a1, a2], false),
        nr::SETRESGID => misc::setids(&[a0, a1, a2], true),
        nr::GETRESUID => misc::getresids(mem, [a0, a1, a2], false),
        nr::GETRESGID => misc::getresids(mem, [a0, a1, a2], true),
        nr::SCHED_YIELD => Ok(0),
        #[cfg(debug_assertions)]
        nr::MACWL_TEST_PANIC => panic!("MACWL_TEST_PANIC from pid {} tid {}", p.pid, t.tid),
        // Every thread is SCHED_OTHER at nice 0, for now (docs/mn-threads.md,
        // step 5, is where priorities come in). getpriority returns 20 - nice.
        nr::GETPRIORITY => Ok(20),
        // Lowering one's priority (nice >= 0) is allowed and, for now, has no
        // effect; raising it needs CAP_SYS_NICE.
        nr::SETPRIORITY => if (a2 as i32) < 0 { Err(errno::EACCES) } else { Ok(0) },
        nr::SCHED_GETSCHEDULER => Ok(0),
        nr::SCHED_GETPARAM => mem.write_pod(a1, &0i32).map(|()| 0),
        // SCHED_FIFO and SCHED_RR are 1 to 99; the others only 0.
        nr::SCHED_GET_PRIORITY_MAX => sched_priority(a0, 99),
        nr::SCHED_GET_PRIORITY_MIN => sched_priority(a0, 1),
        nr::FLOCK => p.fds.host(a0).and_then(|host| ok(unsafe { libc::flock(host, a1 as i32) } as i64)),
        // Restartable sequences need the kernel to notice preemption; without
        // them glibc falls back on atomics, as on kernels before 4.18.
        nr::RSEQ => Err(errno::ENOSYS),
        // A kernel built without it; Wine probes and does without.
        nr::USERFAULTFD => Err(errno::ENOSYS),
        // glibc tries clone3 first and falls back on clone.
        nr::CLONE3 => Err(errno::ENOSYS),
        nr::CLONE => match process::clone(p, a0, a1, a2, a3, a4) {
            Ok(action) => return action,
            Err(e) => Err(e),
        },
        nr::EXECVE => match process::execve(p, mem, a0, a1, a2) {
            Ok(action) => return action,
            Err(e) => Err(e),
        },
        nr::WAIT4 => process::wait4(p, t, mem, a0 as i32 as i64 as u64, a1, a2, a3),
        nr::WAITID => process::waitid(p, t, mem, a0, a1, a2, a3, a4),
        // Advice only.
        nr::FADVISE64 | nr::READAHEAD => p.fds.host(a0).map(|_| 0),
        // Linux xattr names ("user.", "security."...) don't map onto Darwin's;
        // until they do, files look like they're on a filesystem without them.
        nr::SETXATTR..=nr::FREMOVEXATTR => Err(errno::EOPNOTSUPP),
        nr::FUTEX => {
            let r = futex::futex(p, t, mem, a0, a1, a2, a3, a4, a5);
            // What glibc takes as fatal ("The futex facility returned an
            // unexpected error code"): said once, with what was asked.
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Err(e @ (errno::EINVAL | errno::EFAULT | errno::ENOSYS)) = r
                && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                eprintln!("linux-abi: futex gave {e} (op {a1:#x}, uaddr {a0:#x}, timeout {a3:#x}), which glibc takes as fatal");
            }
            r
        }
        nr::CLOCK_GETTIME => misc::clock_gettime(p, mem, a0, a1, false),
        nr::CLOCK_GETRES => misc::clock_gettime(p, mem, a0, a1, true),
        nr::TIMES => misc::times(p, mem, a0),
        nr::GETTIMEOFDAY => misc::gettimeofday(mem, a0, a1),
        nr::GETRUSAGE => misc::getrusage(p, mem, a0, a1),
        nr::SYSINFO => misc::sysinfo(mem, a0),
        nr::SCHED_GETAFFINITY => misc::sched_getaffinity(mem, a1, a2),
        // macOS has no CPU affinity: accept any mask, as if the scheduler
        // honoured it.
        nr::SCHED_SETAFFINITY => Ok(0),
        nr::NANOSLEEP => misc::nanosleep(p, t, mem, a0, a1),
        nr::CLOCK_NANOSLEEP => misc::clock_nanosleep(p, t, mem, a0, a1, a2, a3),
        nr::UNAME => misc::uname(mem, a0),
        nr::GETRANDOM => misc::getrandom(mem, a0, a1),
        nr::PRLIMIT64 => misc::prlimit64(p, mem, a0, a1, a2, a3),
        nr::PRCTL => misc::prctl(p, t, mem, a0, a1, a2),
        nr::CAPGET => misc::capget(mem, a0, a1),
        nr::CAPSET => misc::capset(mem, a0, a1),
        nr::GET_MEMPOLICY => misc::get_mempolicy(mem, a0, a1, a2, a3, a4),
        nr::PIDFD_OPEN => pidfd::pidfd_open(p, a0, a1),
        nr::PIDFD_SEND_SIGNAL => pidfd::pidfd_send_signal(p, a0, a1, a2, a3),
        nr::PIDFD_GETFD => pidfd::pidfd_getfd(p, a0, a1, a2),

        nr::RT_SIGACTION => signal::rt_sigaction(p, mem, a0, a1, a2, a3),
        nr::RT_SIGPROCMASK => signal::rt_sigprocmask(t, mem, a0, a1, a2, a3),
        nr::SIGALTSTACK => signal::sigaltstack(t, mem, a0, a1),
        nr::RT_SIGSUSPEND => signal::rt_sigsuspend(p, t, mem, a0, a1),
        nr::RT_SIGPENDING => signal::rt_sigpending(p, t, mem, a0, a1),
        nr::RT_SIGTIMEDWAIT => signal::rt_sigtimedwait(p, t, mem, a0, a1, a2, a3),
        nr::RT_SIGRETURN => return Action::SigReturn,
        // pids are ints: the upper half of the register is whatever the
        // caller left (glibc's `kill(-pgid)` leaves it zero).
        nr::KILL => signal::kill(p, a0 as i32 as i64, a1),
        nr::TKILL => signal::tkill(p, t, None, a0 as i32 as i64, a1),
        nr::TGKILL => signal::tkill(p, t, Some(a0 as i32 as i64), a1 as i32 as i64, a2),
        nr::SETITIMER => timer::setitimer(p, mem, a0, a1, a2),
        nr::TIMER_CREATE => timer::timer_create(p, t, mem, a0, a1, a2),
        nr::TIMER_SETTIME => timer::timer_settime(p, mem, a0, a1, a2, a3),
        nr::TIMER_GETTIME => timer::timer_gettime(p, mem, a0, a1),
        nr::TIMER_GETOVERRUN => timer::timer_getoverrun(p, a0),
        nr::TIMER_DELETE => timer::timer_delete(p, a0),
        nr::GETITIMER => timer::setitimer(p, mem, a0, 0, a1),

        nr::BRK => Ok(mem.brk(a0) as i64),
        nr::MMAP => mem::mmap(p, mem, args),
        nr::MUNMAP => mem::munmap(p, mem, a0, a1),
        nr::MPROTECT => neg(mem.mprotect(a0, a1, a2)),
        nr::MADVISE => mem::madvise(p, mem, a0, a1, a2),

        nr::READ => io::read(p, t, mem, a0, a1, a2),
        nr::WRITE => io::write(p, t, mem, a0, a1, a2),
        nr::READV => io::readv(p, t, mem, a0, a1, a2),
        nr::WRITEV => io::writev(p, t, mem, a0, a1, a2),
        nr::PREAD64 => io::pread(p, t, mem, a0, a1, a2, a3),
        nr::PWRITE64 => io::pwrite(p, t, mem, a0, a1, a2, a3),
        nr::LSEEK => io::lseek(p, a0, a1 as i64, a2),
        nr::CLOSE => p.fds.remove(a0).map(|_| 0),
        nr::DUP => io::dup(p, a0),
        nr::DUP3 => io::dup3(p, a0, a1, a2),
        nr::FCNTL => io::fcntl(p, mem, a0, a1, a2),
        nr::IOCTL => io::ioctl(p, t, mem, a0, a1, a2),
        nr::PIPE2 => io::pipe2(p, mem, a0, a1),
        nr::PPOLL => io::ppoll(p, t, mem, a0, a1, a2, a3),
        nr::PSELECT6 => io::pselect6(p, t, mem, a0, [a1, a2, a3], a4, a5),
        nr::EPOLL_CREATE1 => epoll::epoll_create1(p, a0),
        nr::EVENTFD2 => kfd::eventfd2(p, a0, a1),
        nr::SIGNALFD4 => signalfd::signalfd4(p, mem, a0, a1, a2, a3),
        nr::CLOSE_RANGE => io::close_range(p, a0, a1, a2),
        nr::INOTIFY_INIT1 => inotify::inotify_init1(p, a0),
        nr::INOTIFY_ADD_WATCH => inotify::inotify_add_watch(p, mem, a0, a1, a2),
        nr::INOTIFY_RM_WATCH => inotify::inotify_rm_watch(p, a0, a1),
        nr::TIMERFD_CREATE => kfd::timerfd_create(p, a0, a1),
        nr::TIMERFD_SETTIME => kfd::timerfd_settime(p, mem, a0, a1, a2, a3),
        nr::TIMERFD_GETTIME => kfd::timerfd_gettime(p, mem, a0, a1),
        nr::EPOLL_CTL => epoll::epoll_ctl(p, mem, a0, a1, a2, a3),
        nr::EPOLL_PWAIT => {
            let timeout = (a3 as i32 >= 0).then(|| abi::Timespec { tv_sec: (a3 as i32 / 1000) as i64, tv_nsec: (a3 as i32 % 1000) as i64 * 1_000_000 });
            epoll::epoll_wait(p, t, mem, a0, a1, a2, timeout, a4)
        }
        nr::EPOLL_PWAIT2 => match if a3 == 0 { Ok(None) } else { mem.read_pod::<abi::Timespec>(a3).map(Some) } {
            Ok(timeout) => epoll::epoll_wait(p, t, mem, a0, a1, a2, timeout, a4),
            Err(e) => Err(e),
        },
        nr::SENDFILE => io::sendfile(p, mem, a0, a1, a2, a3),
        // "Not between these files": callers (coreutils' cp and cat) copy
        // with read and write instead, as they must on older kernels.
        nr::COPY_FILE_RANGE => Err(errno::EXDEV),
        nr::FTRUNCATE => io::ftruncate(p, a0, a1),
        nr::FALLOCATE => io::fallocate(p, a0, a1, a2, a3),
        nr::FSYNC | nr::FDATASYNC => io::fsync(p, a0),
        nr::MEMFD_CREATE => io::memfd_create(p, mem, a0, a1),

        nr::OPENAT => path::openat(p, mem, a0, a1, a2, a3),
        nr::GETDENTS64 => path::getdents64(p, mem, a0, a1, a2),
        nr::NEWFSTATAT => path::fstatat(p, mem, a0, a1, a2, a3),
        nr::STATX => path::statx(p, mem, a0, a1, a2, a3, a4),
        nr::STATFS => path::statfs(p, mem, a0, a1),
        nr::FSTATFS => path::fstatfs(p, mem, a0, a1),
        nr::FSTAT => path::fstat(p, mem, a0, a1),
        nr::READLINKAT => path::readlinkat(p, mem, a0, a1, a2, a3),
        nr::FACCESSAT => path::faccessat(p, mem, a0, a1, a2, 0),
        nr::FACCESSAT2 => path::faccessat(p, mem, a0, a1, a2, a3),
        nr::MKDIRAT => path::mkdirat(p, mem, a0, a1, a2),
        nr::UNLINKAT => path::unlinkat(p, mem, a0, a1, a2),
        nr::SYMLINKAT => path::symlinkat(p, mem, a0, a1, a2),
        nr::LINKAT => path::linkat(p, mem, a0, a1, a2, a3, a4),
        nr::RENAMEAT => path::renameat2(p, mem, a0, a1, a2, a3, 0),
        nr::RENAMEAT2 => path::renameat2(p, mem, a0, a1, a2, a3, a4),
        nr::FCHMODAT => path::fchmodat(p, mem, a0, a1, a2),
        nr::FCHMOD => path::fchmod(p, a0, a1),
        nr::FCHOWNAT => path::fchownat(p, mem, a0, a1, a2, a3, a4),
        nr::FCHOWN => path::fchown(p, a0, a1, a2),
        nr::UTIMENSAT => path::utimensat(p, mem, a0, a1, a2, a3),
        nr::TRUNCATE => path::truncate(p, mem, a0, a1),
        nr::GETCWD => path::getcwd(p, mem, a0, a1),
        nr::CHDIR => path::chdir(p, mem, a0),
        nr::FCHDIR => path::fchdir(p, a0),
        nr::UMASK => Ok(p.set_umask(a0 as u32) as i64),

        nr::SOCKET => net::socket(p, a0, a1, a2),
        nr::SOCKETPAIR => net::socketpair(p, mem, a0, a1, a2, a3),
        nr::BIND => net::bind(p, mem, a0, a1, a2),
        nr::LISTEN => p.fds.host(a0).and_then(|h| ok(unsafe { libc::listen(h, a1 as i32) } as i64)),
        nr::ACCEPT => net::accept4(p, t, mem, a0, a1, a2, 0),
        nr::ACCEPT4 => net::accept4(p, t, mem, a0, a1, a2, a3),
        nr::CONNECT => net::connect(p, t, mem, a0, a1, a2),
        nr::GETSOCKNAME => net::getname(p, mem, a0, a1, a2, false),
        nr::GETPEERNAME => net::getname(p, mem, a0, a1, a2, true),
        nr::SENDTO => net::sendto(p, t, mem, a0, a1, a2, a3, a4, a5),
        nr::RECVFROM => net::recvfrom(p, t, mem, a0, a1, a2, a3, a4, a5),
        nr::SETSOCKOPT => net::setsockopt(p, mem, a0, a1, a2, a3, a4),
        nr::GETSOCKOPT => net::getsockopt(p, mem, a0, a1, a2, a3, a4),
        nr::SHUTDOWN => p.fds.host(a0).and_then(|h| ok(unsafe { libc::shutdown(h, a1 as i32) } as i64)),
        nr::SENDMSG => net::sendmsg(p, t, mem, a0, a1, a2),
        nr::RECVMSG => net::recvmsg(p, t, mem, a0, a1, a2),
        nr::SENDMMSG => net::sendmmsg(p, t, mem, a0, a1, a2, a3),
        nr::RECVMMSG => net::recvmmsg(p, t, mem, a0, a1, a2, a3),

        _ => {
            p.unimplemented(format!("system call {nr} ({})", nr::name(nr)));
            Err(errno::ENOSYS)
        }
    };
    // A Unix datagram socket whose peer has closed (how SOCK_SEQPACKET
    // pairs are made, see net::socketpair) says so the Darwin way: as Linux,
    // reads see the end of the file, and writes EPIPE (seqpacket) or
    // ECONNREFUSED (datagram).
    let ret = match (nr, ret) {
        (nr::READ | nr::READV | nr::RECVFROM | nr::RECVMSG, Err(errno::ECONNRESET | errno::ENOTCONN)) if net::unix_dgram(p, a0) => Ok(0),
        (nr::WRITE | nr::WRITEV | nr::SENDTO | nr::SENDMSG, Err(errno::EDESTADDRREQ | errno::ENOTCONN)) if net::unix_dgram(p, a0) => {
            Err(if net::is_seqpacket(p, a0) { errno::EPIPE } else { errno::ECONNREFUSED })
        }
        (_, ret) => ret,
    };
    // Writing to a pipe or socket with no reader raises SIGPIPE (whose
    // default kills: how `yes | head` ends), unless MSG_NOSIGNAL says not to.
    const MSG_NOSIGNAL: u64 = 0x4000;
    let writes = match nr {
        nr::WRITE | nr::WRITEV | nr::PWRITE64 => true,
        nr::SENDTO => a3 & MSG_NOSIGNAL == 0,
        nr::SENDMSG => a2 & MSG_NOSIGNAL == 0,
        nr::SENDMMSG => a3 & MSG_NOSIGNAL == 0,
        _ => false,
    };
    if writes && ret == Err(errno::EPIPE) {
        signal::raise(t, 13);
    }
    Action::Return(ret.unwrap_or_else(|e| -e))
}

/// A raw "value or negative errno" as this module's `Result`.
fn neg(v: i64) -> Result<i64, i64> {
    if v < 0 { Err(-v) } else { Ok(v) }
}

/// The Linux errno for the host's current `errno`.
pub(crate) fn last_errno() -> i64 {
    errno::from_io(&std::io::Error::last_os_error())
}

/// A host call's result: negative (-1) means failure with `errno` set.
/// `sched_get_priority_max`/`min` for `policy`: `realtime` for SCHED_FIFO
/// and SCHED_RR, 0 for SCHED_OTHER, SCHED_BATCH and SCHED_IDLE.
fn sched_priority(policy: u64, realtime: i64) -> Result<i64, i64> {
    match policy {
        1 | 2 => Ok(realtime),
        0 | 3 | 5 => Ok(0),
        _ => Err(errno::EINVAL),
    }
}

fn ok(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(last_errno()) } else { Ok(r) }
}

/// A NUL-terminated path argument.
fn path_arg<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> Result<Vec<u8>, i64> {
    if addr == 0 {
        return Err(errno::EFAULT);
    }
    mem.read_cstr(addr)
}

/// Read a guest iovec array, returning (base, len) pairs.
fn iovecs<M: GuestMemory + ?Sized>(mem: &M, iov: u64, count: u64) -> Result<Vec<(u64, u64)>, i64> {
    if count > IOV_MAX {
        return Err(errno::EINVAL);
    }
    (0..count)
        .map(|i| {
            let v: crate::abi::Iovec = mem.read_pod(iov + i * 16)?;
            Ok((v.base, v.len))
        })
        .collect()
}

/// Gather the iovecs' contents (capped at [`MAX_IO`] in total).
fn gather<M: GuestMemory + ?Sized>(mem: &M, iov: &[(u64, u64)]) -> Result<Vec<u8>, i64> {
    let mut data = Vec::new();
    for &(base, len) in iov {
        let start = data.len();
        let take = len.min(MAX_IO - start as u64);
        data.resize(start + take as usize, 0);
        mem.read(base, &mut data[start..])?;
        if data.len() as u64 == MAX_IO {
            break;
        }
    }
    Ok(data)
}

/// Scatter `data` across the iovecs.
fn scatter<M: GuestMemory + ?Sized>(mem: &M, iov: &[(u64, u64)], mut data: &[u8]) -> Result<(), i64> {
    for &(base, len) in iov {
        if data.is_empty() {
            break;
        }
        let n = (len as usize).min(data.len());
        mem.write(base, &data[..n])?;
        data = &data[n..];
    }
    Ok(())
}

/// Total length of the iovecs, capped at [`MAX_IO`].
fn iov_len(iov: &[(u64, u64)]) -> u64 {
    iov.iter().fold(0u64, |acc, &(_, l)| acc.saturating_add(l)).min(MAX_IO)
}
