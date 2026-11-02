//! Track B core runtime (B0): run a static arm64 Linux binary directly under
//! Hypervisor.framework, with no guest kernel.
//!
//! The program runs at EL0 in a VM whose memory and page tables we manage
//! ourselves (see [`memory`]). Its `svc` instructions and page faults trap to
//! a tiny EL1 vector table that immediately exits to the host with `hvc`; the
//! host runs the system call through `linux_abi::dispatch` (or maps the
//! faulting page) and resumes the guest.
//!
//! Status: single-threaded programs, static or dynamically linked. See ROADMAP.md, Track B.

pub mod hv;
mod cputime;
mod fork;
mod host;
mod park;
pub mod loader;
pub mod mach;
pub mod memory;
pub mod paging;
pub mod profile;
mod sched;
pub mod shared;
pub mod signal;
pub mod vma;

use std::{
    fmt,
    os::unix::ffi::OsStrExt,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use linux_abi::{
    dispatch,
    syscall::{ctl, futex_finish_wait, Delivery, HostExec, Park, SigInfo, Spawn, Thread},
    Action, Process,
};

use hv::{reg, sys_reg, Vcpu, Vm};
use memory::{Access, AddressSpace, Machine, Space};
use park::{Parking, Ready};
use sched::{Cpu, Job, Offloaded, Placed, Sched, Start, Task};

/// Exception classes (ESR bits 31:26) we care about.
const EC_IABT_LOWER: u64 = 0x20;
const EC_DABT_LOWER: u64 = 0x24;
const EC_SVC64: u64 = 0x15;
const EC_HVC64: u64 = 0x16;

/// Give up on a fault the handler claims to have fixed after this many
/// identical repeats (a runtime bug, but better a SIGSEGV than a hang).
const MAX_FAULT_RETRIES: u32 = 3;

/// How a guest program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `exit`/`exit_group` with this status.
    Code(i32),
    /// Killed by this (Linux) signal, e.g. SIGABRT from `abort()`.
    Signal(i32),
    /// An exception the guest can't recover from (bad memory access,
    /// undefined instruction, ...). A real kernel would send a signal.
    Fault { sig: i32, esr: u64, far: u64, pc: u64 },
    /// An access the hypervisor couldn't back: a shared file mapping past the
    /// end of its file (Linux sends SIGBUS).
    BusError { far: u64, pc: u64 },
    /// Guest physical memory ran out (Linux would OOM-kill).
    OutOfMemory,
}

impl Exit {
    /// The Linux signal that ended the process, if one did.
    pub fn signal(self) -> Option<i32> {
        match self {
            Exit::Code(_) => None,
            Exit::Signal(sig) => Some(sig),
            Exit::Fault { sig, .. } => Some(sig),
            Exit::BusError { .. } => Some(7),  // SIGBUS
            Exit::OutOfMemory => Some(9),      // SIGKILL
        }
    }

    /// The status a Linux shell would report: the exit code, or 128 + the
    /// (Linux) signal number.
    pub fn status(self) -> i32 {
        match self {
            Exit::Code(c) => c,
            _ => 128 + self.signal().unwrap(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Hv(hv::HvError),
    Load(loader::LoadError),
    Io(std::io::Error),
    /// The VM exited in a way the runtime doesn't handle (a runtime bug).
    Unexpected(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hv(e) => e.fmt(f),
            Error::Load(e) => e.fmt(f),
            Error::Io(e) => e.fmt(f),
            Error::Unexpected(what) => write!(f, "unexpected VM exit: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<hv::HvError> for Error {
    fn from(e: hv::HvError) -> Self {
        Error::Hv(e)
    }
}
impl From<loader::LoadError> for Error {
    fn from(e: loader::LoadError) -> Self {
        Error::Load(e)
    }
}
pub use fork::run_child as run_fork_child;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Read a file the guest names (the dynamic linker), through its filesystem.
fn read_guest_file(process: &Process, path: &[u8]) -> Result<Vec<u8>, loader::LoadError> {
    const AT_FDCWD: u64 = -100i64 as u64;
    let name = String::from_utf8_lossy(path).into_owned();
    let host = match process.fs.resolve(&process.fds, AT_FDCWD, path, true) {
        Ok(linux_abi::fs::Resolved { target: linux_abi::fs::Target::Host(host), .. }) => host,
        Ok(_) => return Err(loader::LoadError::Interpreter(name, std::io::ErrorKind::Unsupported.into())),
        Err(e) => return Err(loader::LoadError::Interpreter(name, std::io::Error::from_raw_os_error(e as i32))),
    };
    std::fs::read(std::ffi::OsStr::from_bytes(host.as_bytes())).map_err(|e| loader::LoadError::Interpreter(name, e))
}

/// Run the ELF image `elf` (and its dynamic linker, if it names one) to
/// completion on the calling thread, through any `execve`s it makes.
///
/// Creates (and destroys) the process's one VM, so only one call can be in
/// flight per host process.
///
/// With `MACWL_ONE_VM=1`, the VM is a host for every Linux process this
/// one starts, forks inside it included (docs/processes.md, step 3); the
/// result is the first process's, once all of them have ended. Otherwise
/// each Linux process is a Mac process with a VM of its own (1:1).
pub fn run(elf: &[u8], argv: &[&[u8]], envp: &[&[u8]], process: Process) -> Result<Exit, Error> {
    if one_vm() {
        return host::run(elf, argv, envp, process);
    }
    profile::mark("load start");
    let machine = Machine::new()?;
    let program = load(&machine, elf, argv, envp, &process)?;
    process.set_arg_area(program.args, program.env);
    profile::mark("loaded");
    let thread = Thread::main(&process);
    execute(program.space, Start::Fresh { entry: program.entry, sp: program.sp }, process, thread)
}

/// Whether Linux processes share one VM (`MACWL_ONE_VM=1`): a host per
/// `macwl-run` and everything it starts. Off by default while it's built
/// (docs/processes.md, steps 3 to 6); 1:1 is the fallback.
pub fn one_vm() -> bool {
    std::env::var_os("MACWL_ONE_VM").is_some_and(|v| v == "1")
}

/// A program loaded into a new address space, ready to start.
struct Loaded {
    space: AddressSpace,
    entry: u64,
    sp: u64,
    /// Where its argument and environment strings are (for /proc).
    args: (u64, u64),
    env: (u64, u64),
}

/// Load a program into a new address space of `machine`.
fn load(machine: &Arc<Machine>, elf: &[u8], argv: &[&[u8]], envp: &[&[u8]], process: &Process) -> Result<Loaded, Error> {
    let mut space = AddressSpace::new(machine)?;
    let image = loader::load(&mut space, elf)?;
    space.set_brk_start(image.end);
    let interp = match &image.interp {
        Some(path) => Some(loader::load_interpreter(&mut space, &read_guest_file(process, path)?)?),
        None => None,
    };
    let sp = loader::build_stack(&mut space, &image, interp.as_ref(), argv, envp)?;
    // A dynamically linked program starts in its dynamic linker.
    let entry = interp.as_ref().unwrap_or(&image).entry;
    // The strings are one block ending at the top of the stack, argv's then
    // envp's (see build_stack).
    let len = |v: &[&[u8]]| v.iter().map(|s| s.len() as u64 + 1).sum::<u64>();
    let arg_start = memory::STACK_TOP - len(argv) - len(envp);
    let env_start = arg_start + len(argv);
    Ok(Loaded { space, entry, sp, args: (arg_start, env_start), env: (env_start, memory::STACK_TOP) })
}

/// Why [`run_loop`] stopped.
enum Next {
    /// The process ends (`exit_group`, a fatal signal).
    Exit(Exit),
    /// This thread ends (`exit`), with this status if it's the last.
    ExitThread(i32),
    /// `execve` loaded a new program; run it in place of the old one.
    Exec(Loaded),
    /// `execve` of a Mac program: the process becomes it, once the VM is gone.
    ExecHost(HostExec),
    /// Another thread is ending the process (or exec'ing): stop.
    Stop,
    /// Its time slice is used and others wait: give up the CPU.
    Preempted,
    /// It's making a call that would block on something a kqueue can't
    /// watch, with others waiting for the CPU (or the CPU was taken back
    /// from it in the call): a worker makes it, and the CPU goes to another
    /// task.
    Blocked(u64, [u64; 6]),
    /// Its call waits, parked (see `park`), and the CPU goes to another
    /// task: the call is made again when the wait is over.
    Wait(u64, [u64; 6], Park),
    /// Its process is stopped, or it vforked: set it aside (one VM).
    Park(host::Why),
}

/// A Linux process the host runs (in 1:1 mode, the one): what the
/// dispatcher keeps of it, its address space, and how it ended. Its tasks,
/// and (with many processes in a VM) the process table, hold it.
pub(crate) struct Proc {
    pub abi: Process,
    /// Its address space: shared with a vfork parent until it execs, and
    /// gone (None) once the host process is to become a Mac program.
    space: Mutex<Option<Arc<Space>>>,
    /// How it ended, once it has.
    exit: Mutex<Option<Result<Exit, Error>>>,
    /// It has ended and its last thread has gone: the host is done with it.
    finished: AtomicBool,
    /// Stopped (SIGSTOP and the like, one VM): its tasks park rather than
    /// run, until it's continued.
    stopped: AtomicBool,
    /// Tasks set aside while it's stopped, or while a vfork child runs.
    parked: Mutex<Parked>,
    /// CPU time its threads have used, user and system (one VM: getrusage,
    /// wait4, CPU clocks and timers, /proc).
    cpu: Arc<cputime::CpuTime>,
    /// A vfork child's parent, whose task waits until it execs or exits.
    vfork_parent: Mutex<Option<Arc<Proc>>>,
}

/// A process's tasks that aren't to run for now.
#[derive(Default)]
struct Parked {
    /// While the process is stopped.
    stopped: Vec<(Task, Start)>,
    /// vfork parents, by the child each waits for.
    vfork: Vec<(i64, Task, Start)>,
    /// vfork children that exec'd or exited before their parent's task
    /// was parked.
    released: Vec<i64>,
}

impl Proc {
    fn new(abi: Process, space: Arc<Space>) -> Arc<Proc> {
        Arc::new(Proc {
            abi,
            space: Mutex::new(Some(space)),
            exit: Mutex::new(None),
            finished: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            parked: Mutex::default(),
            cpu: Arc::default(),
            vfork_parent: Mutex::new(None),
        })
    }

    /// Its address space now.
    fn space(&self) -> Arc<Space> {
        self.space.lock().unwrap_or_else(|e| e.into_inner()).clone().expect("a process with no address space")
    }

    /// A new address space (`execve`); the old one is returned.
    fn set_space(&self, space: Arc<Space>) -> Option<Arc<Space>> {
        self.space.lock().unwrap_or_else(|e| e.into_inner()).replace(space)
    }

    fn parked(&self) -> MutexGuard<'_, Parked> {
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How it ended, if it has.
    fn exit(&self) -> MutexGuard<'_, Option<Result<Exit, Error>>> {
        self.exit.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a VM's CPUs share: the run queue and the memory of every process
/// in it, and (one VM) the process table.
struct Host<'a> {
    vm: &'a Vm,
    /// The VM's memory: the frame pool every address space takes from.
    machine: &'a Arc<Machine>,
    /// The run queue and CPUs.
    sched: Arc<Sched>,
    /// Tasks waiting off their CPUs, and the waiter threads' kqueue.
    parking: Arc<Parking>,
    /// Many processes in the VM: their table. None in 1:1 mode.
    table: Option<Arc<host::Table>>,
    /// 1:1: the Mac program the process becomes instead of ending, with the
    /// signal mask of the thread that exec'd it.
    exec_host: Mutex<Option<(HostExec, u64)>>,
}

impl Host<'_> {
    /// `task`'s thread is one of the host's: reachable by its process's
    /// others, and unparked by its wakers.
    fn register(&self, task: &Task) {
        task.proc.abi.register_thread(&task.thread, task.location.waker());
        self.parking.adopt(task);
    }

    /// Process `proc` ends this way (unless it already ended another): every
    /// thread is told to stop, and those waiting for a CPU end at once.
    fn end(&self, proc: &Arc<Proc>, how: Result<Exit, Error>) {
        {
            let mut exit = proc.exit();
            if exit.is_none() {
                *exit = Some(how);
            }
        }
        proc.abi.set_exiting(true);
        let ended = match &self.table {
            // The host is the process: everything stops.
            None => self.sched.stop(),
            Some(_) => {
                let mut tasks = self.sched.drain(proc.abi.pid);
                let mut parked = proc.parked();
                tasks.extend(parked.stopped.drain(..).map(|(t, _)| t));
                tasks.extend(parked.vfork.drain(..).map(|(_, t, _)| t));
                tasks
            }
        };
        for task in ended {
            proc.abi.unregister_thread(task.thread.tid);
        }
        // Its threads were woken (set_exiting): each in a host call sleeps
        // where that wakeup can't be missed (linux_abi's `block`).
        self.check_finished(proc);
    }

    /// Thread `tid` of `proc` has ended (its task won't run again).
    fn thread_gone(&self, proc: &Arc<Proc>, tid: i64) {
        proc.abi.unregister_thread(tid);
        if let Some(table) = &self.table {
            table.thread_gone(proc.abi.pid, tid);
        }
        self.check_finished(proc);
    }

    /// Once a process has ended and its last thread gone, the host is done
    /// with it: with many in the VM, it becomes a zombie for its parent, and
    /// the host stops when no process is left.
    fn check_finished(&self, proc: &Arc<Proc>) {
        if proc.abi.thread_count() > 0 || proc.exit().is_none() || proc.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(table) = &self.table {
            host::finished(self, table, proc);
        }
    }
}

type Scope<'scope, 'env> = std::thread::Scope<'scope, 'env>;

/// Run the guest in `space` until it exits: its first thread on a CPU on this
/// host thread, the others on CPUs it starts as they're needed (each a host
/// thread with a vCPU in the one VM).
fn execute(space: AddressSpace, start: Start, process: Process, thread: Thread) -> Result<Exit, Error> {
    // A guest writing to a closed pipe or socket gets EPIPE; the host
    // process must not be killed by SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    signal::install_interrupt();
    // This host process runs this one Linux process: signals from the Mac
    // side are its.
    process.receive_host_signals();
    // Teardown order matters: vCPUs, then the VM, then the memory mapped into
    // it — so the machine (its frame pool) is declared first (dropped last),
    // and the VM before the process (which holds the address space).
    let machine = space.machine().clone();
    let vm = Vm::new()?;
    let space = Arc::new(Space::new(space));
    {
        let mut s = space.lock();
        s.take_tlb_stale(); // fresh vCPUs have nothing cached
        s.map_into(&vm)?;
    }
    let proc = Proc::new(process, space);
    let sched = Arc::new(Sched::new());
    let parking = Parking::new(sched.clone())?;
    // A host signal (one the guest handles) wakes the process's threads,
    // parked ones too.
    parking.host_signals_for(&proc);
    let host = Host { vm: &vm, machine: &machine, sched, parking, table: None, exec_host: Mutex::new(None) };
    std::thread::scope(|scope| {
        scope.spawn(|| host.sched.ticker());
        park::run_waiters(&host, scope);
        let task = Task::new(thread, proc.clone());
        host.register(&task);
        if let Err(e) = run_cpu(&host, scope, (task, start), true) {
            host.end(&proc, Err(e));
        }
    });
    // Whatever is still parked has ended with the host.
    drop(host.parking.take_all());
    proc.abi.set_exiting(false);
    let Host { exec_host, .. } = host;
    if let Some((exec, blocked)) = exec_host.into_inner().unwrap_or_else(|e| e.into_inner()) {
        // Every vCPU went with its thread. Now the VM and the memory it
        // mapped, so that none of the hypervisor is left when the process
        // is replaced.
        let space = proc.space.lock().unwrap_or_else(|e| e.into_inner()).take().expect("the process's address space");
        space.lock().unmap_from(&vm)?;
        // Never replace the process with the VM still alive: fail instead.
        vm.destroy()?;
        drop(space);
        drop(machine);
        // It won't fork again.
        fork::discard_spare();
        proc.abi.tell_parent(&[ctl::EXEC]);
        let e = linux_abi::syscall::exec_host(&proc.abi, blocked, &exec);
        // Past the point of no return, as for a Linux execve that fails
        // after the old program is gone.
        eprintln!("macwl-run: couldn't start {:?}: Linux errno {e}", exec.path);
        return Ok(Exit::Code(127));
    }
    let exit = proc.exit().take().unwrap_or(Ok(Exit::Code(0)))?;
    if let Some(sig) = exit.signal() {
        // So the parent's wait() reports the signal (the host process exits
        // with 128 + sig rather than raising it).
        proc.abi.tell_parent(&[ctl::KILLED, sig as u8]);
    }
    Ok(exit)
}

/// MACWL_STRACE=1 logs each system call and its result to stderr;
/// MACWL_STRACE=PATH, to PATH.PID, a file per process (with many, their
/// lines would mix on stderr).
fn strace_on() -> bool {
    std::env::var_os("MACWL_STRACE").is_some_and(|v| v != "0")
}

/// A line of the system call log, whole.
/// A call's arguments for MACWL_STRACE, with the path of calls that take one.
fn strace_args(space: &Space, nr: u64, args: &[u64; 6]) -> String {
    use linux_abi::nr;
    let mut out = args.map(|a| format!("{a:#x}")).join(", ");
    let path = match nr {
        nr::OPENAT | nr::NEWFSTATAT | nr::FACCESSAT | nr::FACCESSAT2 | nr::READLINKAT | nr::STATX | nr::UNLINKAT | nr::MKDIRAT => args[1],
        nr::EXECVE => args[0],
        _ => return out,
    };
    let mut bytes = Vec::new();
    let mut b = [0u8];
    while bytes.len() < 512 && linux_abi::GuestMemory::read(space, path + bytes.len() as u64, &mut b).is_ok() && b[0] != 0 {
        bytes.push(b[0]);
    }
    out.push_str(&format!(" {:?}", String::from_utf8_lossy(&bytes)));
    out
}

/// Who made a call, for a line of the system call log: with many processes
/// in the VM their lines mix, so the time (in ms since the host started),
/// pid and tid; in 1:1 mode, nothing.
fn who(sh: &Host, process: &Process, thread: &Thread) -> String {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    if sh.table.is_none() {
        return String::new();
    }
    let ms = START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3;
    format!("{ms:.1} {}/{} ", process.pid, thread.tid)
}

fn strace_line(line: std::fmt::Arguments) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("MACWL_STRACE").filter(|v| v != "1")?;
        let mut path = std::path::PathBuf::from(path).into_os_string();
        path.push(format!(".{}", std::process::id()));
        std::fs::File::create(path).ok().map(Mutex::new)
    });
    let line = format!("[strace] {line}\n");
    match file {
        Some(f) => {
            let _ = f.lock().unwrap_or_else(|e| e.into_inner()).write_all(line.as_bytes());
        }
        None => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

/// What became of a task a CPU ran.
enum Outcome {
    /// It gave up the CPU and waits its turn again.
    Preempted,
    /// It gave up the CPU to make this call on a worker.
    Blocked(u64, [u64; 6]),
    /// It gave up the CPU to wait, parked, for its call.
    Wait(u64, [u64; 6], Park),
    /// It's set aside until something lets it run again (one VM).
    Parked(host::Why),
    /// It's over: the thread ended, or the process is ending.
    Done,
}

/// Be a CPU: make a vCPU on this host thread and run tasks on it, `first`
/// and then whatever the run queue has, until the host ends. A CPU that
/// can't get a vCPU leaves its task to the others (unless it's the first).
fn run_cpu<'scope, 'env>(
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    first: (Task, Start),
    first_cpu: bool,
) -> Result<(), Error> {
    let mut cpu = match Cpu::new(sh.vm, &sh.sched) {
        Ok(cpu) => cpu,
        Err(e) if !first_cpu => {
            // Said once: the threads share the CPUs there are.
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| eprintln!("macwl-run: no vCPU for another CPU ({e}): threads share the ones there are"));
            let (task, start) = first;
            sh.sched.no_cpu(task, start);
            return Ok(());
        }
        Err(e) => {
            sh.sched.cpu_gone();
            return Err(e);
        }
    };
    profile::mark("VM ready");
    // The address space the vCPU is set up for (its serial), and the thread
    // whose registers it holds (if that thread hasn't run elsewhere since).
    let mut configured = None;
    let mut holds = None;
    sh.sched.running(cpu.slot);
    let mut next = Some(first);
    let result = loop {
        let Some((mut task, start)) = next.take().or_else(|| sh.sched.next(cpu.slot)) else { break Ok(()) };
        // Its registers are still in this vCPU only if this CPU ran it last
        // and it ran nowhere else since.
        let warm = holds == Some((task.proc.abi.pid, task.thread.tid)) && task.last_cpu == Some(cpu.slot);
        let mut after = None;
        let mut retry = None;
        if sh.table.is_some() {
            cputime::turn_starts();
        }
        let step = || {
            let space = task.proc.space();
            {
                let mut s = space.lock();
                match configured {
                    None => s.configure_vcpu(&mut cpu.vcpu)?,
                    // Another process's (or a new program's) address space:
                    // only TTBR0_EL1 differs.
                    Some(serial) if serial != s.serial() => s.switch_vcpu(&mut cpu.vcpu)?,
                    Some(_) => {}
                }
                configured = Some(s.serial());
            }
            // Address spaces that have gone (processes that ended) are
            // flushed from every TLB, so their frames and ASIDs come back.
            if sh.table.is_some() && sh.machine.any_unflushed() {
                for asid in sh.machine.unflushed_asids() {
                    flush_asid_now(&mut cpu.vcpu, asid)?;
                    sh.machine.asid_flushed(asid);
                }
            }
            let vcpu = &mut cpu.vcpu;
            match start {
                Start::Fresh { entry, sp } => {
                    vcpu.set_sys_reg(sys_reg::SP_EL0, sp)?;
                    vcpu.set_reg(reg::PC, entry)?;
                    vcpu.set_reg(reg::CPSR, 0)?; // EL0t, interrupts unmasked (there are none)
                }
                // Back on the CPU it left, with no one else in between: the
                // vCPU still has its registers.
                Start::Resume(_) if warm => {}
                Start::Resume(ctx) => ctx.restore(vcpu)?,
                Start::AfterCall { nr, arg0, ret, .. } if warm => after = Some((nr, arg0, ret)),
                Start::AfterCall { ctx, nr, arg0, ret } => {
                    ctx.restore(vcpu)?;
                    after = Some((nr, arg0, ret));
                }
                Start::Retry { nr, args, park, .. } if warm => retry = Some((nr, args, park)),
                Start::Retry { ctx, nr, args, park } => {
                    ctx.restore(vcpu)?;
                    retry = Some((nr, args, park));
                }
            }
            task.location.set_running(cpu.slot);
            task.last_cpu = Some(cpu.slot);
            task.thread.moved_host_thread();
            run_task(&mut cpu, sh, scope, &mut task, &mut configured, after.take(), retry.take())
        };
        // A panic anywhere in running the task (not only in a system call,
        // which `contained` already catches) is a runtime error like any
        // other: it ends the task's process, not the host.
        let step = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
            Ok(step) => step,
            Err(e) => {
                // A Mac program the process was about to become (1:1) is
                // forgotten: after a panic it ends, nothing more.
                *sh.exec_host.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Err(Error::Unexpected(format!("panic: {}", panic_message(&*e))))
            }
        };
        sh.sched.done(cpu.slot);
        if sh.table.is_some() {
            // The turn's CPU time less its runs in the guest: the runtime's
            // work for the process.
            cputime::take_stock(&task.proc.cpu, true);
        }
        holds = Some((task.proc.abi.pid, task.thread.tid));
        match step {
            Ok(Outcome::Done) => {}
            Ok(Outcome::Preempted) => {
                task.location.set_nowhere();
                let ctx = hv::UserContext::save(&cpu.vcpu)?;
                place(sh, scope, task, Start::Resume(Box::new(ctx)));
            }
            // Stopped in our vector after the `svc`: the registers say where
            // to return to, and the worker supplies x0.
            // Waiting, parked: off every CPU, its call made again when the
            // wait is over.
            Ok(Outcome::Wait(nr, args, park)) => {
                task.location.set_nowhere();
                let ctx = Box::new(hv::UserContext::save(&cpu.vcpu)?);
                let (watch, deadline) = (park.watch.clone(), park.deadline);
                sh.parking.park(task, Start::Retry { ctx, nr, args, park }, &watch, deadline);
            }
            Ok(Outcome::Blocked(nr, args)) => {
                task.location.set_nowhere();
                let ctx = Box::new(hv::UserContext::save(&cpu.vcpu)?);
                offload(sh, scope, Job { task, ctx, nr, args });
            }
            Ok(Outcome::Parked(why)) => {
                task.location.set_nowhere();
                let ctx = hv::UserContext::save(&cpu.vcpu)?;
                host::park(sh, scope, task, Start::Resume(Box::new(ctx)), why);
            }
            // With many processes in the VM, a runtime error ends the
            // process whose task met it, and this CPU (its vCPU may be what
            // failed); the others carry on.
            Err(e) if sh.table.is_some() => {
                eprintln!("macwl-run: pid {}: {e}", task.proc.abi.pid);
                task.location.set_nowhere();
                let proc = task.proc.clone();
                sh.end(&proc, Err(e));
                sh.thread_gone(&proc, task.thread.tid);
                break Ok(());
            }
            Err(e) => {
                sh.thread_gone(&task.proc.clone(), task.thread.tid);
                task.location.set_nowhere();
                break Err(e);
            }
        }
        place_ready(sh, scope);
    };
    sh.sched.cpu_gone();
    result
}

use cputime::thread_cpu_ns;

/// Run a task on a CPU until it ends, the process ends, or it gives the CPU
/// up. `configured` is the serial of the address space the vCPU is set up
/// for.
fn run_task<'scope, 'env>(
    cpu: &mut Cpu,
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    task: &mut Task,
    configured: &mut Option<u64>,
    after: Option<(u64, u64, i64)>,
    retry: Option<(u64, [u64; 6], Park)>,
) -> Result<Outcome, Error> {
    let Task { thread, location, proc, .. } = task;
    let proc = proc.clone();
    let process = &proc.abi;
    let mut space = proc.space();
    let (vcpu, slot) = (&mut cpu.vcpu, cpu.slot);
    let done = |thread: &Thread| {
        sh.thread_gone(&proc, thread.tid);
        location.set_nowhere();
        Ok(Outcome::Done)
    };
    // Back from a call a worker made: it returns, as a call on the CPU does.
    let mut first = match after {
        Some((nr, arg0, ret)) => finish_call(vcpu, &space, process, thread, nr, arg0, ret)?,
        None => None,
    };
    // Back from a parked wait: the call is made again, with what it had
    // (its deadline, its futex wait), and its files kept open until then.
    if let Some((nr, args, park)) = retry {
        let Park { deadline, futex, files, .. } = park;
        // What came while it waited, and what its wait registered, go:
        // the call looks for itself now.
        thread.waker.drain();
        thread.set_retry(Some(deadline));
        let next = system_call(vcpu, slot, sh, scope, &proc, &space, thread, nr, args, futex);
        thread.set_retry(None);
        drop(files);
        first = next?;
    }
    loop {
        let next = match first.take() {
            Some(next) => next,
            None => run_loop(vcpu, slot, sh, scope, &proc, &space, thread)?,
        };
        profile::mark("guest stopped");
        match next {
            Next::Preempted => return Ok(Outcome::Preempted),
            Next::Blocked(nr, args) => return Ok(Outcome::Blocked(nr, args)),
            Next::Wait(nr, args, park) => return Ok(Outcome::Wait(nr, args, park)),
            Next::Park(why) => return Ok(Outcome::Parked(why)),
            Next::Stop => return done(thread),
            Next::Exit(exit) => {
                sh.end(&proc, Ok(exit));
                return done(thread);
            }
            // The VM can't go while any vCPU is left: the other threads end,
            // every CPU leaves its vCPU behind, and `execute` does the rest.
            Next::ExecHost(exec) => {
                // Only the host process that is this Linux process alone
                // becomes a Mac program: with others in its VM, never (the
                // dispatcher refuses such an execve already).
                assert!(sh.table.is_none(), "a Mac program's execve in a host of many processes");
                stop_other_threads(sh, &proc);
                *sh.exec_host.lock().unwrap_or_else(|e| e.into_inner()) = Some((exec, thread.blocked_signals()));
                sh.end(&proc, Ok(Exit::Code(0)));
                return done(thread);
            }
            Next::ExitThread(code) => {
                end_thread(&proc, &space, thread);
                let outcome = done(thread);
                if process.thread_count() == 0 {
                    sh.end(&proc, Ok(Exit::Code(code)));
                }
                return outcome;
            }
            // The same VM and vCPU run the new program: only the address
            // space changes (its own page tables and ASID, frames from the
            // same pool), once the other threads have gone (as Linux). Other
            // CPUs set up for it when they next run a task.
            Next::Exec(program) => {
                stop_other_threads(sh, &proc);
                let new = Arc::new(Space::new(program.space));
                {
                    let mut s = new.lock();
                    s.take_tlb_stale();
                    s.configure_vcpu(vcpu)?;
                    *configured = Some(s.serial());
                }
                let old = proc.set_space(new.clone()).expect("an address space to replace");
                space = new;
                // The old address space's entries go from every TLB, and
                // then its frames and ASID can be used again. (A vfork
                // parent's stays: it's the parent's.)
                let old_asid = old.lock().asid();
                drop(old);
                flush_asid_now(vcpu, old_asid)?;
                sh.machine.asid_flushed(old_asid);
                // The thread that execs carries on as the main one.
                process.unregister_thread(thread.tid);
                if let Some(table) = &sh.table {
                    table.exec_done(&proc, thread.tid);
                }
                thread.tid = process.pid;
                process.register_thread(thread, location.waker());
                process.set_exiting(false);
                // A vfork parent waiting for this child runs on.
                host::vfork_done(sh, &proc);
                start_fresh_on(vcpu, program.entry, program.sp)?;
                profile::mark("exec'd");
            }
        }
    }
}

/// A system call returns `ret`: into x0, with the TLB flushed if it changed
/// mappings, and signals delivered (a handler with SA_RESTART restarts an
/// interrupted call). Some if that ends the process.
fn finish_call(vcpu: &mut Vcpu, space: &Space, process: &Process, thread: &mut Thread, nr: u64, arg0: u64, ret: i64) -> Result<Option<Next>, Error> {
    vcpu.set_reg(reg::X0, ret as u64)?;
    // Mappings changed: flush every vCPU's TLB before the call returns, then
    // unmapped frames can be reused.
    {
        let mut s = space.lock();
        if s.needs_flush() {
            flush_tlb_now(vcpu)?;
            s.flushed();
        }
    }
    // Signals the call raised or let through (or that came in during it)
    // are delivered as it returns.
    process.sync_host_signals();
    Ok(deliver_signals(vcpu, space, process, thread, Some((nr, arg0, ret)))?.map(Next::Exit))
}

/// Be a worker: make system calls for tasks that gave up their CPU (`first`,
/// then whatever comes), each task then waiting for a CPU again with the
/// result, until the host ends.
fn run_worker<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, first: Job) {
    let strace = strace_on();
    let mut next = Some(first);
    while let Some(Job { mut task, ctx, nr, args }) = next.take().or_else(|| sh.sched.next_job()) {
        let proc = task.proc.clone();
        let (process, space) = (&proc.abi, proc.space());
        let cpu_time = sh.table.is_some().then(thread_cpu_ns);
        let thread = &mut task.thread;
        // Reachable here first, then the check: a signal before it is seen
        // by the check, one after it interrupts the call.
        task.location.set_worker();
        thread.moved_host_thread();
        let ret = loop {
            // A signal that came while the task waited for a worker (when
            // nothing could interrupt the call) interrupts it now.
            if (interruptible(nr, &args) && process.signal_ready(thread)) || process.exiting() {
                break -linux_abi::errno::EINTR;
            }
            let Some(action) = contained(process, nr, || dispatch(process, thread, &*space, nr, args)) else {
                // A runtime error in the call: its process ends (the task
                // sees that when it's back on a CPU).
                sh.end(&proc, Ok(Exit::Signal(SIGSYS)));
                break -linux_abi::errno::EINTR;
            };
            let ret = match action {
                Action::Return(ret) => ret,
                // Only calls that return come here (see may_block).
                other => {
                    eprintln!("macwl-run: worker: {} gave {other:?}", linux_abi::nr::name(nr));
                    -linux_abi::errno::ENOSYS
                }
            };
            // As on a CPU: a blocked host signal doesn't interrupt the call.
            if ret == -linux_abi::errno::EINTR && restartable(nr) && !process.exiting() && !process.signal_ready(thread) {
                continue;
            }
            break ret;
        };
        if strace {
            let a = strace_args(&space, nr, &args);
            strace_line(format_args!("{}{}({a}) = Return({ret}) (worker)", who(sh, process, thread), linux_abi::nr::name(nr)));
        }
        if let Some(t0) = cpu_time {
            proc.cpu.add_sys(thread_cpu_ns().saturating_sub(t0));
        }
        drop(space);
        task.location.set_nowhere();
        place(sh, scope, task, Start::AfterCall { ctx, nr, arg0: args[0], ret });
        place_ready(sh, scope);
    }
}

/// Linux's SIGSYS: how a process whose system call met a runtime error ends
/// (docs/processes.md, "Fault isolation").
const SIGSYS: i32 = 31;

/// Make a system call (`call`), containing a runtime error in it: a panic
/// is logged and gives None, for the caller to end the process with SIGSYS,
/// rather than taking the host (and every other process in it) down.
fn contained<T>(process: &Process, nr: u64, call: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(v) => Some(v),
        Err(e) => {
            // The files the call held for its duration (dispatch lets go of
            // them as it returns, which it didn't).
            linux_abi::fs::end_call();
            eprintln!("macwl-run: pid {}: system call {nr} ({}) failed in the runtime ({}): the process ends with SIGSYS", process.pid, linux_abi::nr::name(nr), panic_message(&*e));
            None
        }
    }
}

/// What a caught panic said.
fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a panic".into())
}

/// Calls that may block, and that a worker can make for a task (they only
/// return: no new thread, exec, exit or signal frame). `openat` blocks only
/// for a FIFO (until the other end opens).
fn may_block(nr: u64) -> bool {
    use linux_abi::nr::*;
    matches!(
        nr,
        FUTEX | READ | READV | PREAD64 | WRITE | WRITEV | PWRITE64 | RECVFROM | RECVMSG | RECVMMSG | SENDTO | SENDMSG | SENDMMSG | ACCEPT | ACCEPT4
            | CONNECT | PPOLL | PSELECT6 | EPOLL_PWAIT | EPOLL_PWAIT2 | NANOSLEEP | CLOCK_NANOSLEEP | WAIT4 | WAITID
            | RT_SIGTIMEDWAIT | RT_SIGSUSPEND | FLOCK | FCNTL | OPENAT
    )
}

/// Whether a signal pending as the call starts interrupts it (EINTR) before
/// it waits, as Linux checks before it sleeps. Not for calls that wait with
/// a signal mask of their own (they look against that), nor `openat`
/// (only a FIFO's waits, and that one is rare: a file's open isn't
/// interruptible).
fn interruptible(nr: u64, args: &[u64; 6]) -> bool {
    use linux_abi::nr::*;
    match nr {
        RT_SIGSUSPEND | OPENAT => false,
        PPOLL => args[3] == 0,
        PSELECT6 => args[5] == 0,
        EPOLL_PWAIT | EPOLL_PWAIT2 => args[4] == 0,
        _ => true,
    }
}

/// Calls that would block on something no kqueue can watch, so that with
/// others waiting for the CPU a worker makes them: `flock` and `F_SETLKW`
/// that may wait for a lock. (Everything else that waits parks, from inside
/// the call; a FIFO's `open`, which can't be told cheaply, is taken back
/// from the CPU once others wait, and goes to a worker then.)
fn needs_worker(nr: u64, args: &[u64; 6]) -> bool {
    use linux_abi::nr::*;
    const LOCK_NB: u64 = 4;
    const LOCK_UN: u64 = 8;
    match nr {
        FLOCK => args[1] & (LOCK_NB | LOCK_UN) == 0,
        FCNTL => matches!(args[1], 7 | 38), // F_SETLKW, F_OFD_SETLKW
        _ => false,
    }
}

/// A thread is exiting: CLONE_CHILD_CLEARTID's word is zeroed and a waiter
/// on it woken (`pthread_join`).
fn end_thread(proc: &Proc, space: &Space, thread: &Thread) {
    let addr = thread.clear_child_tid;
    if addr != 0 && linux_abi::GuestMemory::write(space, addr, &0u32.to_le_bytes()).is_ok() {
        proc.abi.futex_wake(addr, 1);
    }
}

/// `execve` from a process with other threads: they end first (those
/// waiting for a CPU without running again).
fn stop_other_threads(sh: &Host, proc: &Arc<Proc>) {
    let process = &proc.abi;
    process.set_exiting(true);
    loop {
        // Again each time: a preempted task may have gone back on the queue.
        let mut gone = sh.sched.drain(process.pid);
        {
            let mut parked = proc.parked();
            gone.extend(parked.stopped.drain(..).map(|(t, _)| t));
            gone.extend(parked.vfork.drain(..).map(|(_, t, _)| t));
        }
        for task in gone {
            sh.thread_gone(proc, task.thread.tid);
        }
        if process.thread_count() <= 1 {
            break;
        }
        // Keep kicking: one may have been between checks.
        process.wake_all_threads();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Start a new program on a vCPU that ran another: nothing of the old
/// program's registers survives, and the TLB is flushed on the way to EL0
/// (the vector's flush path erets to the entry point).
fn start_fresh_on(vcpu: &mut Vcpu, entry: u64, sp: u64) -> Result<(), Error> {
    hv::UserContext { x: [0; 31], sp, pc: entry, pstate: 0, tpidr: 0, tpidrro: 0, fpcr: 0, fpsr: 0, v: [0; 32] }.restore(vcpu)?;
    vcpu.set_sys_reg(sys_reg::ELR_EL1, entry)?;
    vcpu.set_sys_reg(sys_reg::SPSR_EL1, 0)?; // EL0t
    vcpu.set_reg(reg::CPSR, 0x3c5)?; // EL1h, interrupts masked, for the flush path
    vcpu.set_reg(reg::PC, memory::VECTOR_FLUSH_PATH)?;
    Ok(())
}

/// Flush the running address space's TLB entries (on all vCPUs) now, from a
/// vCPU stopped in our vector after a system call: run the vector's flush
/// path, returning not to EL0 but to an `hvc` at EL1, then put the vCPU back
/// as it was.
fn flush_tlb_now(vcpu: &mut Vcpu) -> Result<(), Error> {
    run_at_el1(vcpu, memory::VECTOR_FLUSH_PATH, None)
}

/// Flush ASID `asid`'s TLB entries (on all vCPUs) now, from this vCPU,
/// whatever it runs: an address space that has gone.
fn flush_asid_now(vcpu: &mut Vcpu, asid: u64) -> Result<(), Error> {
    run_at_el1(vcpu, memory::FLUSH_ASID_PATH, Some(asid << 48))
}

/// Run one of the vectors page's paths at EL1 (with x0 = `x0`, if given),
/// returning to the `hvc #0` of vector 0, and put the vCPU back as it was.
fn run_at_el1(vcpu: &mut Vcpu, path: u64, x0: Option<u64>) -> Result<(), Error> {
    let (pc, cpsr) = (vcpu.reg(reg::PC)?, vcpu.reg(reg::CPSR)?);
    let (elr, spsr) = (vcpu.sys_reg(sys_reg::ELR_EL1)?, vcpu.sys_reg(sys_reg::SPSR_EL1)?);
    let saved_x0 = vcpu.reg(reg::X0)?;
    vcpu.set_sys_reg(sys_reg::ELR_EL1, memory::VECTORS_VA)?; // vector 0: `hvc #0`
    vcpu.set_sys_reg(sys_reg::SPSR_EL1, 0x3c5)?; // EL1h, masked
    vcpu.set_reg(reg::CPSR, 0x3c5)?;
    vcpu.set_reg(reg::PC, path)?;
    if let Some(v) = x0 {
        vcpu.set_reg(reg::X0, v)?;
    }
    let exit = vcpu.run()?;
    let ok = exit.reason == hv::EXIT_REASON_EXCEPTION && exit.exception.syndrome >> 26 == EC_HVC64 && exit.exception.syndrome & 0xffff == 0;
    vcpu.set_sys_reg(sys_reg::ELR_EL1, elr)?;
    vcpu.set_sys_reg(sys_reg::SPSR_EL1, spsr)?;
    vcpu.set_reg(reg::CPSR, cpsr)?;
    vcpu.set_reg(reg::PC, pc)?;
    vcpu.set_reg(reg::X0, saved_x0)?;
    if !ok {
        // A cancel (host signal kick) can land first; try again.
        if exit.reason == hv::EXIT_REASON_CANCELED {
            return run_at_el1(vcpu, path, x0);
        }
        return Err(Error::Unexpected(format!("TLB flush: exit {} syndrome {:#x}", exit.reason, exit.exception.syndrome)));
    }
    Ok(())
}

/// The Linux errno for a program `execve` couldn't load.
fn exec_errno(e: &Error) -> i64 {
    match e {
        Error::Load(loader::LoadError::Interpreter(_, io)) => linux_abi::errno::from_io(io),
        Error::Io(_) => linux_abi::errno::ENOMEM,
        _ => linux_abi::errno::ENOEXEC,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop<'scope, 'env>(
    vcpu: &mut Vcpu,
    slot: usize,
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    proc: &Arc<Proc>,
    space: &Space,
    thread: &mut Thread,
) -> Result<Next, Error> {
    let process = &proc.abi;
    let mut last_fault = (u64::MAX, 0);
    // MACWL_FAULTS=1 describes each fault that becomes a signal.
    let faults = std::env::var_os("MACWL_FAULTS").is_some_and(|v| v != "0");
    loop {
        // Another thread is ending the process, or exec'ing.
        if process.exiting() {
            return Ok(Next::Stop);
        }
        process.sync_host_signals();
        // A signal came in (perhaps kicking the vCPU out) that this thread can
        // take: act on it.
        if process.signal_ready(thread)
            && let Some(exit) = deliver_signals(vcpu, space, process, thread, None)?
        {
            return Ok(Next::Exit(exit));
        }
        // Tasks a call of this one let run again (a SIGCONT, a futex wake
        // that needs a new CPU) get one.
        place_ready(sh, scope);
        if sh.table.is_some() {
            // A stopped process's task goes no further.
            if proc.stopped.load(Ordering::Acquire) {
                return Ok(Next::Park(host::Why::Stopped));
            }
        }
        if space.lock().take_tlb_stale() {
            // Return to EL0 through the vector's TLB-flushing path (which
            // erets to ELR_EL1/SPSR_EL1). Those hold where EL0 resumes only
            // when the vCPU stopped in the vector; stopped in the program (a
            // kick), they're whatever the last exception left, so they're set
            // from where it stopped. And stopped part-way through the flush
            // path itself, the guest's x0 is in TPIDR_EL1: back into x0 first,
            // or the path, starting over, would save the borrowed ASID in its
            // place. (Another thread's fault marks the address space stale
            // at any time, so both happen: a spinning thread's pointer became
            // its ASID << 48, or it went back to an old pc.)
            let (pc, cpsr) = (vcpu.reg(reg::PC)?, vcpu.reg(reg::CPSR)?);
            if cpsr & 0xf == 0 {
                vcpu.set_sys_reg(sys_reg::ELR_EL1, pc)?;
                vcpu.set_sys_reg(sys_reg::SPSR_EL1, cpsr)?;
            } else if memory::flush_path_borrows_x0(pc) {
                vcpu.set_reg(reg::X0, vcpu.sys_reg(sys_reg::TPIDR_EL1)?)?;
            }
            vcpu.set_reg(reg::CPSR, 0x3c5)?; // EL1h, interrupts masked
            vcpu.set_reg(reg::PC, memory::VECTOR_FLUSH_PATH)?;
        }
        // Time in the guest is the process's user time (one VM).
        let ran = sh.table.is_some().then(|| proc.cpu.entering_guest(slot));
        let exit = vcpu.run();
        if let Some(since) = ran {
            proc.cpu.left_guest(slot, since);
        }
        let exit = exit?;
        match exit.reason {
            hv::EXIT_REASON_EXCEPTION => {}
            // Kicked by a host signal: handled at the top of the loop. (If it
            // caught the vCPU on its way into the vector, the hvc comes first.)
            // Or by the ticker: the task's slice is used, and others wait. It
            // can go only if it stopped in the program: stopped in the vector,
            // on its way to a system call, it carries on (and the ticker
            // kicks again).
            hv::EXIT_REASON_CANCELED => {
                if sched::take_preempt(slot) && vcpu.reg(reg::CPSR)? & 0xf == 0 {
                    return Ok(Next::Preempted);
                }
                continue;
            }
            // Nothing arms the timer yet; just resume.
            hv::EXIT_REASON_VTIMER_ACTIVATED => continue,
            other => {
                let (pc, cpsr) = (vcpu.reg(reg::PC)?, vcpu.reg(reg::CPSR)?);
                let (elr, esr) = (vcpu.sys_reg(sys_reg::ELR_EL1)?, vcpu.sys_reg(sys_reg::ESR_EL1)?);
                return Err(Error::Unexpected(format!(
                    "reason {other}, syndrome {:#x}, pc {pc:#x}, cpsr {cpsr:#x}, ELR_EL1 {elr:#x}, ESR_EL1 {esr:#x}",
                    exit.exception.syndrome
                )));
            }
        }
        let syndrome = exit.exception.syndrome;
        // A stage-2 abort: the guest touched guest physical memory whose host
        // side can't be faulted in (a shared file mapping past EOF): SIGBUS.
        if matches!(syndrome >> 26, EC_DABT_LOWER | EC_IABT_LOWER) {
            let (far, pc) = (exit.exception.virtual_address, vcpu.reg(reg::PC)?);
            const BUS_ADRERR: i32 = 2;
            match fault_signal(vcpu, space, process, thread, SigInfo::fault(7, BUS_ADRERR, far), far)? {
                Some(_) => return Ok(Next::Exit(Exit::BusError { far, pc })),
                None => continue,
            }
        }
        if syndrome >> 26 != EC_HVC64 {
            let pc = vcpu.reg(reg::PC)?;
            return Err(Error::Unexpected(format!("syndrome {syndrome:#x} at pc {pc:#x}")));
        }

        // An hvc from our vector table: the guest took an exception to EL1.
        // The hvc immediate says which vector, ESR_EL1 says what happened.
        let vector = syndrome & 0xffff;
        let esr = vcpu.sys_reg(sys_reg::ESR_EL1)?;
        let ec = esr >> 26;
        if vector != memory::VECTOR_EL0_SYNC {
            let elr = vcpu.sys_reg(sys_reg::ELR_EL1)?;
            return Err(Error::Unexpected(format!("vector {vector}, ESR_EL1 {esr:#x}, ELR_EL1 {elr:#x}")));
        }
        if ec == EC_SVC64 {
            // The guest got somewhere since any earlier fault: the same page
            // can fault again legitimately (unmapped and mapped again).
            last_fault = (u64::MAX, 0);
            let nr = vcpu.reg(reg::X8)?;
            let mut args = [0; 6];
            for (i, a) in args.iter_mut().enumerate() {
                *a = vcpu.reg(reg::X0 + i as u32)?;
            }
            if nr == linux_abi::nr::WAIT4 {
                profile::mark("wait4 called");
            }
            if let Some(next) = system_call(vcpu, slot, sh, scope, proc, space, thread, nr, args, None)? {
                return Ok(next);
            }
            continue;
        }

        let far = vcpu.sys_reg(sys_reg::FAR_EL1)?;
        let pc = vcpu.sys_reg(sys_reg::ELR_EL1)?;
        if (ec == EC_DABT_LOWER || ec == EC_IABT_LOWER) && is_translation_or_permission_fault(esr) {
            let access = if ec == EC_IABT_LOWER {
                Access::Exec
            } else if esr & (1 << 6) != 0 {
                Access::Write // WnR
            } else {
                Access::Read
            };
            last_fault = if last_fault.0 == far { (far, last_fault.1 + 1) } else { (far, 0) };
            if last_fault.1 < MAX_FAULT_RETRIES {
                let fixed = {
                    let mut s = space.lock();
                    let breaks = s.cow_breaks();
                    let fixed = s.handle_fault(far, access);
                    // A page made this address space's own is progress, not
                    // a repeat: a thread writing it in a loop while another
                    // forks again and again faults on it each time.
                    if s.cow_breaks() != breaks {
                        last_fault = (u64::MAX, 0);
                    }
                    fixed
                };
                match fixed {
                    // Resuming erets to the faulting instruction, which retries.
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(paging::OutOfMemory) => return Ok(Next::Exit(Exit::OutOfMemory)),
                }
            }
        }
        // A real fault: the program's signal handler, or the end of it.
        let info = fault_info(ec, esr, far);
        if faults {
            report_fault(vcpu, space, process, thread, &info, esr, far, pc);
        }
        last_fault = (u64::MAX, 0);
        if let Some(sig) = fault_signal(vcpu, space, process, thread, info, far)? {
            return Ok(Next::Exit(Exit::Fault { sig, esr, far, pc }));
        }
    }
}

/// System call `nr` of a task on a CPU, from the guest's `svc` or made again
/// after a parked wait (`futex`: the futex wait it parked in): Some(why) if
/// the task is to stop running here, None to carry on in the guest.
#[allow(clippy::too_many_arguments)]
fn system_call<'scope, 'env>(
    vcpu: &mut Vcpu,
    slot: usize,
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    proc: &Arc<Proc>,
    space: &Space,
    thread: &mut Thread,
    nr: u64,
    args: [u64; 6],
    mut futex: Option<linux_abi::syscall::FutexWait>,
) -> Result<Option<Next>, Error> {
    let process = &proc.abi;
    let strace = strace_on();
        let may_block = may_block(nr);
        let action = loop {
            // Blocked here, for the scheduler to see, before looking for
            // others waiting: a task queued from now on takes the CPU
            // back; one queued before, this sees.
            sched::set_inline(slot, may_block);
            // Others wait for this CPU: a call that would block on what no
            // kqueue can watch (a lock, a FIFO's open) gives it up, and a
            // worker makes it. (The others park, from inside the call: see
            // linux_abi's `block`.)
            if may_block && sh.sched.others_waiting() && needs_worker(nr, &args) {
                sched::set_inline(slot, false);
                return Ok(Some(Next::Blocked(nr, args)));
            }
            // As Linux checks before it sleeps: a signal that came in
            // while the call was on its way (when nothing could interrupt
            // it) interrupts it now, rather than after it's slept.
            if may_block && interruptible(nr, &args) && process.signal_ready(thread) {
                sched::set_inline(slot, false);
                break Action::Return(-linux_abi::errno::EINTR);
            }
            // A futex wait made again after it parked: the same wait,
            // still queued (made from scratch, it could miss a wake).
            let action = match futex.take() {
                Some(w) => Some(Action::Return(futex_finish_wait(process, thread, w).unwrap_or_else(|e| -e))),
                None => contained(process, nr, || dispatch(process, thread, space, nr, args)),
            };
            sched::set_inline(slot, false);
            // The call met a runtime error (a panic): its process ends,
            // and the host carries on.
            let Some(action) = action else { return Ok(Some(Next::Exit(Exit::Signal(SIGSYS)))) };
            // The call waits, parked: the CPU goes to another task.
            if action == Action::Return(-linux_abi::errno::PARK) {
                sched::take_evict(slot);
                let park = thread.take_park().expect("a parked call says what it waits for");
                return Ok(Some(Next::Wait(nr, args, park)));
            }
            // It can't park (a file kqueue won't watch): a worker makes it.
            if action == Action::Return(-linux_abi::errno::OFFLOAD) {
                sched::take_evict(slot);
                return Ok(Some(Next::Blocked(nr, args)));
            }
            let interrupted = matches!(action, Action::Return(r) if r == -linux_abi::errno::EINTR);
            // The CPU was taken back, for a task waiting for one, from a
            // call blocked in the host (a lock, a FIFO's open, a read that
            // lost a race for data another reader took): a worker makes it
            // again (nothing was done), or the guest gets EINTR. (Waits that
            // can park have done so, from inside the call.)
            if interrupted && sched::take_evict(slot) {
                if restartable(nr) && !process.exiting() && !process.signal_ready(thread) {
                    return Ok(Some(Next::Blocked(nr, args)));
                }
                break action;
            }
            // A host signal the guest blocks (one a signalfd reads, say)
            // broke a blocking host call, which on Linux it wouldn't: back
            // into the call, unless there's a signal to act on or the
            // process is ending.
            if interrupted && restartable(nr) && !process.exiting() && !process.signal_ready(thread) {
                continue;
            }
            break action;
        };
        if nr == linux_abi::nr::WAIT4 {
            profile::mark("wait4 returned");
        }
        if strace {
            let a = strace_args(space, nr, &args);
            strace_line(format_args!("{}{}({a}) = {action:?}", who(sh, process, thread), linux_abi::nr::name(nr)));
        }
        let ret = match action {
            // Resuming runs the eret after the hvc, back to the
            // instruction after the guest's svc.
            Action::Return(v) => v,
            Action::Exit(code) => {
                profile::mark("exit_group");
                return Ok(Some(Next::Exit(Exit::Code(code))));
            }
            Action::ExitThread(code) => return Ok(Some(Next::ExitThread(code))),
            Action::Spawn(spawn) => spawn_thread(vcpu, sh, scope, proc, space, thread, &spawn)?,
            Action::SigReturn => match signal::sigreturn(vcpu, space) {
                Ok(mask) => {
                    thread.sigreturn(mask);
                    // The restored mask may let pending signals through.
                    if let Some(exit) = deliver_signals(vcpu, space, process, thread, None)? {
                        return Ok(Some(Next::Exit(exit)));
                    }
                    return Ok(None);
                }
                Err(()) => return Ok(Some(Next::Exit(Exit::Signal(11)))), // SIGSEGV, as Linux
            },
            Action::Fork(f) => {
                profile::mark("fork");
                let ctx = hv::UserContext::save(vcpu)?;
                let ret = match &sh.table {
                    // In the VM: copy on write, the child queued.
                    Some(table) => match host::fork(sh, scope, table, vcpu, proc, space, thread, ctx, &f)? {
                        // vfork: the parent waits until its child execs
                        // or exits, set aside, off its CPU.
                        Ok(child) if f.vfork => {
                            if let Some(next) = finish_call(vcpu, space, process, thread, nr, args[0], child)? {
                                return Ok(Some(next));
                            }
                            return Ok(Some(Next::Park(host::Why::Vfork(child))));
                        }
                        Ok(child) => child,
                        Err(e) => -e,
                    },
                    None => fork::fork(space, process, thread, &ctx, &f).unwrap_or_else(|e| -e),
                };
                profile::mark("fork done, parent resumes");
                ret
            }
            // A Mac program replaces the host process: never with other
            // processes in its VM (the dispatcher refuses already; this
            // is the second lock on that door).
            Action::ExecHost(_) if sh.table.is_some() => -linux_abi::errno::ENOEXEC,
            Action::ExecHost(exec) => return Ok(Some(Next::ExecHost(exec))),
            Action::Exec(exec) => {
                profile::mark("exec: loading");
                let argv: Vec<&[u8]> = exec.argv.iter().map(Vec::as_slice).collect();
                let envp: Vec<&[u8]> = exec.envp.iter().map(Vec::as_slice).collect();
                match load(sh.machine, &exec.elf, &argv, &envp, process) {
                    Ok(program) => {
                        profile::mark("exec: loaded");
                        process.exec_done(thread, exec.exe, &exec.comm);
                        process.set_arg_area(program.args, program.env);
                        return Ok(Some(Next::Exec(program)));
                    }
                    Err(e) => -exec_errno(&e),
                }
            }
        };
        finish_call(vcpu, space, process, thread, nr, args[0], ret)
}

/// `clone` with CLONE_THREAD: start a thread like the caller (x0 = 0, its own
/// stack and TLS), on an idle CPU, a new one, or when one's free. Returns its
/// tid, or -EAGAIN if the process is ending.
#[allow(clippy::too_many_arguments)]
fn spawn_thread<'scope, 'env>(
    vcpu: &Vcpu,
    sh: &'scope Host<'env>,
    scope: &'scope Scope<'scope, 'env>,
    proc: &Arc<Proc>,
    space: &Space,
    thread: &Thread,
    spawn: &Spawn,
) -> Result<i64, Error> {
    let mut ctx = hv::UserContext::save(vcpu)?;
    ctx.x[0] = 0;
    if spawn.stack != 0 {
        ctx.sp = spawn.stack;
    }
    if let Some(tls) = spawn.tls {
        ctx.tpidr = tls;
    }
    let tid = match &sh.table {
        // Linux's tids share the pid space: the host's.
        Some(table) => match table.new_tid(proc.abi.pid) {
            Some(tid) => tid,
            None => return Ok(-linux_abi::errno::EAGAIN),
        },
        None => proc.abi.new_tid(),
    };
    let mut child = thread.spawn(tid);
    child.clear_child_tid = spawn.child_cleartid.unwrap_or(0);
    // Both are in the memory the threads share.
    for addr in [spawn.parent_tid, spawn.child_settid].into_iter().flatten() {
        let _ = linux_abi::GuestMemory::write(space, addr, &(tid as i32).to_le_bytes());
    }
    let task = Task::new(child, proc.clone());
    sh.register(&task);
    // The process is ending: the thread never runs.
    if !place(sh, scope, task, Start::Resume(Box::new(ctx))) {
        return Ok(-linux_abi::errno::EAGAIN);
    }
    Ok(tid)
}

/// A task wants a CPU: queued, or on a new CPU started for it. False if the
/// host is ending instead (the thread ends without running again).
fn place<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, task: Task, start: Start) -> bool {
    match sh.sched.add(task, start) {
        Placed::Queued => true,
        Placed::NewCpu(task, start) => {
            start_cpu(sh, scope, task, start);
            true
        }
        Placed::Dropped(task) => {
            sh.thread_gone(&task.proc.clone(), task.thread.tid);
            false
        }
    }
}

/// Start a CPU (the run queue made room for one) with `task` as its first.
fn start_cpu<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, task: Task, start: Start) {
    // (Not holding the process: the CPU outlives its first task.)
    let proc = Arc::downgrade(&task.proc);
    scope.spawn(move || {
        if let Err(e) = run_cpu(sh, scope, (task, start), false)
            && let Some(proc) = proc.upgrade()
        {
            sh.end(&proc, Err(e));
        }
    });
}

/// Put the tasks let run again by threads that couldn't (see
/// [`park::Ready`]) on the run queue, or on CPUs started for them.
pub(crate) fn place_ready<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>) {
    for ready in sh.parking.take_ready() {
        match ready {
            Ready::Place(task, start) => {
                place(sh, scope, task, start);
            }
            Ready::NewCpu(task, start) => start_cpu(sh, scope, task, start),
            Ready::Gone(task) => sh.thread_gone(&task.proc.clone(), task.thread.tid),
            Ready::Offload(job) => offload(sh, scope, job),
        }
    }
}

/// A call for a worker: an idle one, a new one, or its turn.
fn offload<'scope, 'env>(sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, job: Job) {
    match sh.sched.offload(job) {
        Offloaded::Queued => {}
        Offloaded::NewWorker(job) => {
            scope.spawn(move || run_worker(sh, scope, job));
        }
        Offloaded::Dropped(task) => sh.thread_gone(&task.proc, task.thread.tid),
    }
}

/// Describe a fault on stderr: where (and in which file, at what offset), the
/// address, and the return addresses up the frame-pointer chain.
#[allow(clippy::too_many_arguments)]
fn report_fault(vcpu: &Vcpu, space: &Space, process: &Process, thread: &Thread, info: &SigInfo, esr: u64, far: u64, pc: u64) {
    let place = |addr: u64| match process.mapped_file(addr) {
        Some((path, offset)) => format!("{addr:#x} ({}+{offset:#x})", String::from_utf8_lossy(&path)),
        None => format!("{addr:#x}"),
    };
    let lr = vcpu.reg(reg::X0 + 30).unwrap_or(0);
    eprintln!(
        "macwl-run: pid {} tid {}: signal {} at pc {}, address {far:#x} (ESR {esr:#x}), lr {}",
        process.pid,
        thread.tid,
        info.signo,
        place(pc),
        place(lr)
    );
    let mut fp = vcpu.reg(reg::X0 + 29).unwrap_or(0);
    for depth in 0..24 {
        let mut frame = [0u8; 16];
        if fp == 0 || linux_abi::GuestMemory::read(space, fp, &mut frame).is_err() {
            break;
        }
        let (next, ret) = (u64::from_le_bytes(frame[..8].try_into().unwrap()), u64::from_le_bytes(frame[8..].try_into().unwrap()));
        eprintln!("    #{depth} {}", place(ret));
        if next <= fp {
            break;
        }
        fp = next;
    }
}

/// The signal for an exception the guest can't continue past.
fn fault_info(ec: u64, esr: u64, far: u64) -> SigInfo {
    const EC_UNKNOWN: u64 = 0x00;
    const EC_PC_ALIGN: u64 = 0x22;
    const EC_SP_ALIGN: u64 = 0x26;
    const EC_FP_EXC: u64 = 0x2c;
    const EC_BRK: u64 = 0x3c;
    const SEGV_MAPERR: i32 = 1;
    const SEGV_ACCERR: i32 = 2;
    const BUS_ADRALN: i32 = 1;
    const ILL_ILLOPC: i32 = 1;
    const TRAP_BRKPT: i32 = 1;
    match ec {
        EC_DABT_LOWER | EC_IABT_LOWER if esr & 0x3f == 0x21 => SigInfo::fault(7, BUS_ADRALN, far),
        EC_DABT_LOWER | EC_IABT_LOWER if esr & 0x3c == 0x0c => SigInfo::fault(11, SEGV_ACCERR, far),
        EC_DABT_LOWER | EC_IABT_LOWER => SigInfo::fault(11, SEGV_MAPERR, far),
        EC_PC_ALIGN | EC_SP_ALIGN => SigInfo::fault(7, BUS_ADRALN, far),
        EC_FP_EXC => SigInfo::fault(8, 0, 0),
        EC_BRK => SigInfo::fault(5, TRAP_BRKPT, 0),
        EC_UNKNOWN => SigInfo::fault(4, ILL_ILLOPC, 0),
        _ => SigInfo::fault(4, ILL_ILLOPC, 0),
    }
}

/// Send a fault's signal: run the guest's handler (None), or the signal that
/// ends the process.
fn fault_signal(vcpu: &mut Vcpu, space: &Space, process: &Process, thread: &mut Thread, info: SigInfo, far: u64) -> Result<Option<i32>, Error> {
    match process.fault(thread, info) {
        Delivery::Kill(sig) => Ok(Some(sig as i32)),
        Delivery::Handler(call) => {
            let ctx = hv::UserContext::save(vcpu)?;
            if signal::deliver(vcpu, space, &call, thread.altstack(), &ctx, far).is_err() {
                return Ok(Some(11));
            }
            process.sync_host_signals();
            Ok(None)
        }
    }
}

/// Deliver pending signals the guest doesn't block: run handlers (each frame
/// on top of the last), or end the process for a default action. `syscall`
/// is the call just returning (number, first argument, result), which a
/// handler with SA_RESTART restarts if it was interrupted.
fn deliver_signals(vcpu: &mut Vcpu, space: &Space, process: &Process, thread: &mut Thread, mut syscall: Option<(u64, u64, i64)>) -> Result<Option<Exit>, Error> {
    while let Some(d) = process.next_delivery(thread) {
        match d {
            Delivery::Kill(sig) => return Ok(Some(Exit::Signal(sig as i32))),
            Delivery::Handler(call) => {
                let mut ctx = hv::UserContext::save(vcpu)?;
                if let Some((nr, arg0, ret)) = syscall.take()
                    && ret == -linux_abi::errno::EINTR
                    && call.restart
                    && restartable(nr)
                {
                    // Back to the svc, with its first argument again.
                    ctx.pc -= 4;
                    ctx.x[0] = arg0;
                }
                if signal::deliver(vcpu, space, &call, thread.altstack(), &ctx, 0).is_err() {
                    return Ok(Some(Exit::Signal(11)));
                }
                process.sync_host_signals();
            }
        }
    }
    // The mask may have changed (a handler's, or a suspend's ending).
    thread.sync_host_mask();
    Ok(None)
}

/// Calls Linux restarts after a handler with SA_RESTART (the ones that block
/// on files, sockets and children; not sleeps or waits for signals).
fn restartable(nr: u64) -> bool {
    use linux_abi::nr::*;
    matches!(nr, READ | READV | WRITE | WRITEV | PREAD64 | PWRITE64 | WAIT4 | WAITID | OPENAT | IOCTL | RECVFROM | SENDTO | RECVMSG | SENDMSG | RECVMMSG | SENDMMSG | ACCEPT | ACCEPT4 | FUTEX)
}

/// Whether an abort's fault status code (ISS bits 5:0) is a translation or
/// permission fault at any level — the kinds demand paging resolves.
fn is_translation_or_permission_fault(esr: u64) -> bool {
    matches!(esr & 0x3c, 0x04 | 0x0c)
}
