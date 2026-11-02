# Processes in one VM: a host per session

Status: design (2026-11-02). ROADMAP items 6 and 13; extends the thread
scheduler (`docs/mn-threads.md`) one level up, from the threads of a
process to the processes of a session.

## The problem

Hypervisor.framework allows **128 VMs system-wide**
(`kern.hv.max_address_spaces`, read-only; `hv-limits vms`) and one VM per
Mac process. Today each live Linux process is a Mac process with a VM of its
own (`crates/runtime/src/fork.rs`), so at most ~128 Linux processes can be
alive at once, on the whole Mac, shared with every other hypervisor user. A
Wine session uses 10–20; a desktop with a browser (Firefox: a parent and ten
or so children) and a few terminals full of tools gets close; a program
respawning children in a loop takes the machine's VMs from everybody.

A Mac process per Linux process costs time too. A fork launches (or takes a
pre-launched) `macwl-run --fork-child`, snapshots guest memory as a Mach
memory entry, sends the fd table over a socket and builds a new VM: 2.6 ms
per fork in a loop, against 0.5 ms for the Mac's own bash (below). And the
Linux process model leans on the Mac's: pids, process groups, sessions,
`wait`, signals and job control are the Mac's own, adjusted where they
differ (a control socket for `setpgid` on a child, another for "killed by
signal N").

## The idea: the runtime is the kernel of a session

A **host** is one Mac process with one VM that runs every Linux process of a
session (a `scripts/session.sh` session, or a `macwl-run` invocation and
everything it starts). The runtime already owns stage 1, the guest's page
tables: so each Linux process gets its own page tables and **ASID**, and a
CPU switching from a task of one process to a task of another writes
`TTBR0_EL1`, as Linux does when it switches `mm`s. Hypervisor.framework sees
one stage-2 space, one VM, 18 vCPUs.

The host keeps the process table (pids, parents, process groups, sessions,
zombies), sends and delivers signals, and does job control, in memory, as a
kernel does. The CPUs of `docs/mn-threads.md` become the host's: they run
tasks of every process. Fork copies page tables inside the VM, copy on write.

What stays a Mac process:

- the **host** (one per session);
- **clients**: a `macwl-run` started from outside (a Mac terminal, a script,
  `session.sh`) that joins a host stands in for the one Linux process it
  started: its exit status, signals sent to it from the Mac side, its
  terminal;
- **Mac programs** (item 13), each its own Mac process, standing in the
  process table for the Linux process that exec'd it;
- **tty agents**: a small helper per Linux session that has a terminal and
  runs Mac programs, so that they get it as their controlling terminal.

The host process never execs: Mac programs run as separate processes, so
the VM's process is never replaced and its image stays the runtime.

## Costs (measured, M5 Pro, macOS 27.0)

### Switching between processes

`crates/runtime/examples/switch_cost.rs`: one vCPU runs two guest programs,
each at the same virtual addresses (in different frames) under page tables
of its own, making a system call per round (an exit to the host and back);
after each, the host resumes the same task, or switches to another thread of
the same process (save + restore), or to the other process (save + restore +
`TTBR0_EL1`, with ASIDs, or with `tlbi vmalle1` on the way in instead). Each
run also reads N pages, a TLB entry each. Every round checks that the code
that ran was the right process's (with ASIDs but without the nG bit on user
pages it isn't: the check fails at once).

| µs per round, pages read per run: | 0 | 16 | 256 | 2048 |
|---|---|---|---|---|
| No switch (a system call's round trip) | 0.73 | 0.74 | 0.92 | 2.74 |
| Another thread, same process | 1.40 | 1.41 | 1.55 | 3.38 |
| Another process, own ASID | 1.40 | 1.41 | 1.66 | 7.82 |
| Another process, TLB flushed instead | 1.71 | 1.81 | 2.52 | 8.30 |

- A switch between processes with ASIDs costs what a switch between threads
  does: the `TTBR0_EL1` write is 0.008 µs (Hypervisor.framework applies it
  on the next run), and both processes' TLB entries survive.
- Without ASIDs every switch refills the TLB: +0.3 µs for nothing touched,
  +1 µs for 256 pages, and it grows with the working set.
- At 2048 pages each, two processes (4096 pages) don't fit the TLB together
  whatever we do; threads of one process (2048 pages) do. That's the
  hardware, not the design.
- Context switch (save + restore) in the same run: 0.54–0.60 µs (1.2 µs in
  `mn-threads.md`, measured earlier); a condvar hand-off between host
  threads 3.0 µs; a vCPU created and destroyed 12–13 µs.

### Making processes today

`crates/runtime/guest/forkbench.c` (`forkbench MODE N MB`): N rounds of
fork + `_exit` + `waitpid`, fork + `execve` (itself, which exits at once) +
wait, or `posix_spawn` (vfork + exec) + wait, from a process with MB of
private memory touched. Mean of the rounds after the first:

| | 0 MB | 200 MB |
|---|---|---|
| fork + exit + wait | 2.6 ms (first 3.8–4.2) | 3.5 ms (first 12–15) |
| fork + exec + exit + wait | 2.6 ms | 3.7 ms |
| `posix_spawn` + exit + wait | 2.6 ms | 3.8 ms |

Shell work, nixpkgs' Linux bash 5.3 and coreutils 9.11, 100 rounds:

| | Linux bash in macwl-run | the Mac's bash, natively |
|---|---|---|
| `( : )` (fork + exit + wait) | 2.7–3.1 ms | 0.49 ms |
| `coreutils/true` (fork + exec + exit + wait, glibc, dynamic) | 3.1–3.2 ms | 1.2 ms (`/usr/bin/true`) |

Where the time goes (`MACWL_PROFILE=1`, a fork after the first):

- the memory snapshot (`MAP_MEM_VM_COPY`): 0.04 ms for a small process,
  2.3–3.1 ms for 200 MB (10 ms the first time), with the parent stopped;
- the spare child: in a loop, the next spare is still launching (~2 ms of
  macOS starting a non-Apple binary), and that bounds the rate;
- handing over and rebuilding: 0.2 ms; the child's VM ready 0.25–0.4 ms
  later;
- the child's exit until the parent's `wait4` returns: 0.4–0.5 ms (a Mac
  process and its VM torn down);
- an `execve` itself, in the child: 0.13 ms for a static program; glibc's
  dynamic loading adds ~0.3 ms (bash's `true` minus `( : )`).

### A VM for many processes

`crates/runtime/examples/vm_cost.rs`:

| | |
|---|---|
| IPA size: default, largest allowed | 36, 40 bits |
| Create + destroy a VM (IPA 36 or 40 bits) | 25–31 µs |
| `hv_vm_map` of a reserved, untouched pool: 8 / 64 / 256 / 512 / 1000 GiB | 33 µs / 0.21 / 0.80 / 1.2 / 1.8 ms |
| `hv_vm_unmap` of the same | 9 µs / 0.06 / 0.24 / 0.45 / 1.0 ms |
| Map + unmap a shared window, 16 KiB to 64 MiB | 0.5 µs |
| Copy a process's page tables, every private page made copy-on-write | 4–6 ns per page: 7 µs (4 MB), 34 µs (32 MB), 0.22 ms (200 MB), 1.2 ms (1 GiB) |
| A copy-on-write fault's host work (copy the frame, repoint the entry) | 1.2 µs, plus the exit (0.73 µs) |

Limits that bind a host rather than a process: `kern.maxfilesperproc`
184320 host fds; `kern.num_taskthreads` 12288 host threads; 64 vCPUs (we
use `hw.physicalcpu`, 18); 1 TiB of IPA space at 40 bits.

### What fork would cost inside the VM (estimates, checked at step 3)

- **fork + exit + wait, small** (bash, ~4 MB resident): page tables 7 µs;
  the process and fd table cloned (reference counts) a few µs; the parent's
  ASID flushed; the child queued on a CPU; then its copy-on-write faults, ~2
  µs each (a subshell writes tens of pages: 40–200 µs); exit frees its
  tables; the parent's `wait` woken (3 µs). **~0.05–0.25 ms, against 2.6–3.1
  ms: 10–50x.**
- **200 MB**: 0.22 ms of page tables instead of a 2.3–10 ms stall and a Mac
  process; pages the child writes are copied as it writes them.
- **fork + exec of `true`**: the exec and dynamic loading (~0.4 ms) are
  what's left: **~0.5 ms against 3.1 ms**, about Linux's own and under the
  Mac's 1.2 ms. `posix_spawn` (vfork) copies nothing at all: the child runs
  in the parent's address space until it execs, as on Linux.

## 1. Topology: one host per session

Three shapes were weighed:

- **One host per session**: every Linux process of a session in one Mac
  process. Pids, signals, `wait`, groups, sessions and terminals are all in
  one process table, in memory.
- **A pool of hosts with a coordinator**: processes spread over hosts; pids,
  signals, `wait`, process groups and terminals cross hosts through the
  coordinator.
- **Hybrid**: a host fills up and spills into a new one; cross-host
  processes through a coordinator.

Recommended: **one host per session**, and a separate host when asked for
(`--new-host`), for isolation, never automatically. Against the others:

- **Caps.** The 128-VM limit then counts sessions (and isolated apps), not
  processes. A host uses 18 vCPUs of the 64 a VM may have. Its fds share
  `kern.maxfilesperproc` (184320): a large session uses a few thousand
  (in-VM fork shares a description instead of duplicating it, see 8). Its
  host threads share `kern.num_taskthreads` (12288): 18 CPUs plus a worker
  per blocked task; a session with thousands of blocked threads gets close,
  which is a reason to block in kqueues instead of workers later (open
  questions), not to spill. 1 TiB of IPA space holds the memory of any
  session the Mac's RAM can. Nothing forces a second host, so there is
  nothing to spill.
- **Spilling costs everything a host saves.** The coordinator would carry
  pids, `wait`, signals to groups, `setpgid`, the controlling terminal and
  job control between hosts: a distributed kernel, the problem one VM
  removes. A fork into another host is today's fork (2.6 ms, a memory
  snapshot), and processes sharing memory across hosts get no shared
  futexes.
- **Fault isolation** is the cost of one host. Guest faults are already
  contained (they become the faulting process's signal). A runtime error
  that unwinds in a system call (a failed assertion, an `unwrap`) is caught
  (`catch_unwind` around `dispatch`) and ends that process alone, with
  SIGSYS, logged; locks recover from poisoning already
  (`unwrap_or_else(|e| e.into_inner())` throughout). What can't be
  contained (an abort, memory corruption, a Hypervisor.framework error)
  ends the host and the session with it, as with any VM: clients report it (exit status 128 + 9, with a message), and
  `session.sh` can start a new one. An app that brings the runtime down,
  or that shouldn't share it, runs in a host of its own (`--new-host`).

### How a `macwl-run` finds its host

- `MACWL_HOST=PATH` (or `--host PATH`): join the host listening on the Unix
  socket at PATH. `scripts/session.sh` starts one (`macwl-run --serve
  "$XDG_RUNTIME_DIR/macwl-host"`) and exports `MACWL_HOST`, so `foot`, the
  D-Bus daemon and anything started from the session (including a Mac
  terminal that sources the session's environment) share it.
- Otherwise (or with `--new-host`): the `macwl-run` process **is** a host,
  for the program it was given and everything that program starts: today's
  behaviour, with in-VM fork for the descendants. It ends when its first
  process does and the others have gone (or, if they outlive it, when the
  last does).

A client, like `docker exec`, sends the host its argv, environment, working
directory, root and binds, and either its fds 0–2 (over `SCM_RIGHTS`) or a
terminal (below), and gets back the Linux pid. Then:

- **Exit status**: the host tells the client how its process ended; the
  client exits with the code, or 128 + the Linux signal (as `macwl-run`
  does now, rather than raising it).
- **Signals from the Mac side** (`kill` from a Mac shell): the client
  catches every catchable signal and forwards it to its Linux process.
  SIGKILL ends the client: the host treats that as the terminal hanging up
  (SIGHUP to the foreground process group of its terminal, if it had one,
  and to the process itself). SIGSTOP stops only the client (and so its
  relay); the Linux side carries on until its output blocks.
- **Its terminal**, if stdin is one: the client puts it in raw mode and
  relays bytes both ways to a pty in the host, as `ssh -t` does: the Linux
  process's terminal is that pty, whose job control the host does (6). Its
  initial termios and size are the Mac terminal's; SIGWINCH sends the new
  size. Ctrl-C and Ctrl-Z are bytes the relay passes on, and the host
  signals the foreground process group. If the client's own Linux process
  stops (Ctrl-Z), the client restores the Mac terminal and stops itself
  (SIGSTOP), so the Mac shell sees a stopped job; on SIGCONT it goes raw
  again and continues the Linux process. Without a terminal there's no
  relay: the client's fds are the process's.

A host that is itself the `macwl-run` started in a Mac terminal does the
same relay on a thread of its own, so job control works one way everywhere.

## 2. Memory: one pool, ASIDs

- **One pool of frames for all processes**, not slices: copy-on-write fork
  and shared windows need frames any address space can map, and slices
  bring back a fixed per-process limit. The VM is created with a **40-bit
  IPA** (`hv_vm_config_set_ipa_size`; the largest here; 36 is the default):
  frames in `[0, 512 GiB)`, shared windows in `[512 GiB, 1 TiB)`. The pool is
  reserved (`MAP_NORESERVE`) and mapped once: 1.2 ms when the host starts.
  Its bookkeeping (`host_pages`, and the new per-frame reference counts)
  grows with the frames handed out (`next_fresh`) instead of being sized
  for 512 GiB up front.
- **Frames go to processes in 64 KiB chunks** (the fault-around size), so a
  process's memory fills whole 16 KiB host pages, and when it exits or
  unmaps, whole host pages are free and go back to macOS
  (`MADV_FREE_REUSABLE`, as now). Each CPU keeps a few chunks, so a page
  fault doesn't take the pool's lock.
- **Accounting** per address space (frames mapped, shared frames counted
  once each, as Linux's RSS/PSS), for `/proc` and for an out-of-memory
  killer that ends the largest process, not the host.
- **ASIDs**: `TCR_EL1.AS = 1` (16 bits), the ASID in `TTBR0_EL1[63:48]`, the
  nG bit on every user page, the kernel half (vectors) global, one
  `TTBR1_EL1` per host. Measured to work under Hypervisor.framework (each
  process ran its own code at the same address, every round). One ASID per
  address space; `execve` takes a new one. An address space's ASID is
  flushed (`tlbi aside1is`) when it's freed, so a reused one finds nothing
  stale. 65535 live address spaces before running out: no rollover needed;
  if it ever happens, Linux's scheme (a new generation, `vmalle1is`, ASIDs
  handed out again).
- **The flush-before-return rule stays**, per address space: a call that
  changed a process's mappings flushes **its ASID** before returning
  (`tlbi aside1is`, broadcast to every vCPU, so its threads on other CPUs
  are covered as now), and other processes' entries survive. The vector's
  flush path finds the ASID itself: `TLBI ASIDE1IS` takes it in bits 63:48
  of its operand, as `TTBR0_EL1` holds it, so `mrs` it (into a register
  saved in `TPIDR_EL1`, which nothing else uses) and flush. Freed frames are
  quarantined per address space until that flush, then go back to the pool.
- **Shared mappings** (`shared.rs`) become the host's: a `MAP_SHARED` of an
  object (a file range, a shm object, shared anonymous memory a fork
  passed on) that another process already has is the same window at the
  same IPA, counted by the address spaces using it. No host `mmap` per
  process; stage-2 faults once per host rather than once per process
  (every Firefox process maps libxul); and the IPA is a key for futexes on
  shared memory, as Linux keys them by page (ROS target, item 5). Private
  read-through file windows (libraries, fonts) are shared the same way.
  Making a window costs 0.5 µs whatever its size.

## 3. Fork: copy on write, inside the VM

`clone` as fork makes a new process in the host's table:

1. its address space: the parent's VMAs, and page tables copied, private
   pages read-only in both and their frames' counts raised (4–6 ns a page);
   shared windows mapped as they are; a new ASID;
2. the parent's ASID flushed before `clone` returns (its entries are now
   read-only), as the flush-before-return rule already does;
3. its `Process`: fd table, cwd, signal dispositions, rlimits, umask,
   credentials cloned; the fd table shares open files (Linux's semantics:
   one open file description, one offset);
4. one task, the caller's registers with x0 = 0, queued on the host's CPUs.

A write to a copy-on-write page faults: a frame with a count above one is
copied (1.2 µs of host work plus the exit); a frame no one else has any
more is just made writable again.

- **vfork** (`CLONE_VM | CLONE_VFORK`, every `posix_spawn`): the child runs
  in the parent's address space, nothing copied, and the parent's task
  waits until the child execs or exits, as on Linux.
- **`CLONE_VM` without vfork** (not supported today): two processes sharing
  one address space, now possible.
- `fork.rs`'s spares, Mach memory entries and control sockets are used only
  by 1:1 mode while it lasts (9).

The estimate (above): fork + exit + wait 10–50x faster, fork + exec of a
dynamic program ~6x, and no parent stall that grows with its size.

## 4. Scheduling: the host's CPUs, every process's tasks

- The run queue (`Sched`) and CPUs move from the process (`Shared`) to the
  host. A task knows its process (its `Process`, address space and ASID);
  a CPU about to run a task of another process than the one its vCPU last
  ran writes `TTBR0_EL1`. Nothing else in the vCPU's setup differs between
  processes (`TCR`, `SCTLR`, `VBAR`, `TTBR1` are the host's), so each CPU
  configures its vCPU once. A switch between processes costs what a switch
  between threads does (1.4 µs a round either way, measured).
- **Fairness: per task**, host-wide, as Linux schedules threads without
  cgroups. Step 5 of `mn-threads.md` (classes, EEVDF, wakeup preemption,
  host QoS by priority) lands on the host's run queue, not a process's: do
  step 3 here first, or build step 5 on a run queue that's already the
  host's. Whether a session should weigh processes (Linux's autogroup) is
  an open question.
- **18 CPUs for the session**, not 18 for each process: a session can't
  oversubscribe the Mac's cores any more (today N busy processes run up to
  18 N host threads). A busy background app now shares the session's 18
  with the foreground app, so priorities and QoS (step 5) matter more.
- **Stopped processes** (SIGSTOP, SIGTSTP, Ctrl-Z): their tasks are kicked
  off their CPUs and parked (not queued) until SIGCONT; a task in a worker
  finishes its call and parks. `Shared::end`, `stop_other_threads` and the
  exiting flag become per process: one process ending drains its tasks from
  the host's queue only.
- Signal tables (`VCPUS`, `HOST_THREADS`: 64 slots, one per vCPU) are the
  host's already; `WORKERS` (512) grows, as workers are now the session's.
  One ticker per host.

## 5. pids, groups, sessions, wait, signals

All emulated; the host's process table is the truth. Mac processes appear in
it only as the stand-ins for clients' and Mac programs' Linux processes.

| | Today | One VM |
|---|---|---|
| pid, tid | the host's pid; tids made up from it | from one allocator, from 100000 up (above macOS's 99999): a Linux pid handed to a Mac program can never name a Mac process |
| pid 1 | the Mac's launchd | the host's init: orphans are reparented to it (or to a `PR_SET_CHILD_SUBREAPER`), it reaps them; it gets no signals |
| `getppid` | host `getppid` | the table (1 for a client's process) |
| `setpgid`, `setsid`, `getpgid`, `getsid` | host calls, a control-socket round trip for children (they have exec'd as far as macOS knows) | the table, Linux's rules: EACCES once a child has exec'd, EPERM across sessions |
| `wait4`, `waitid` | host `wait4`, plus "killed by N" on the control socket | zombies in the table; WNOHANG, WUNTRACED, WCONTINUED, WNOWAIT, `P_PGID`; rusage from the scheduler's accounting |
| SIGCHLD | the host's, from macOS | sent by the host when a child exits, stops or continues; `SA_NOCLDWAIT`/ignored SIGCHLD reap at once |
| `kill`, `tgkill` to others | host `kill`, number translated | in memory, with sender pid and uid in `siginfo` (item 7's gap); `kill(-1)` now possible (every process of the session but init and the caller); to a Mac stand-in, host `kill` of its Mac process |
| Default actions | the host's own (dispositions mirrored onto host `sigaction`) | emulated: terminate ends the process, stop parks it (4), continue resumes |
| `SO_PEERCRED` | the peer Mac process's | the peer's Linux pid, recorded when the connection is made, if both ends are in the host |

Signals from the Mac reach the host as host signals for the host itself
(SIGTERM: end the session; SIGCHLD: a Mac child changed state); none is
mirrored onto a Linux process's disposition any more. Clients forward
theirs (1).

## 6. Terminals and job control

Today a Linux process is a Mac process in a Mac session, so the Mac's tty
driver does job control: foreground process group, SIGINT/SIGQUIT/SIGTSTP
from the keyboard, SIGTTIN/SIGTTOU, SIGWINCH, hangup. In a host that can't
work: one Mac process has one session and one controlling terminal, and a
host runs many Linux sessions (a bash per foot window). So:

- **Every Linux controlling terminal is a pty in the host** (or none): ptys
  the guest opens (foot's) and the relays' (1). The host pty does the line
  discipline (canonical mode, echo, output processing) as now; the host
  keeps, per pty, its Linux session and foreground process group.
- **Keyboard signals**: whoever writes the master (foot, or a relay) does so
  through the host. If the pty's termios (read from the pty, so whatever
  last set it counts, a Mac program included) has ISIG, the host signals the
  Linux members of the foreground group on VINTR, VQUIT and VSUSP, as Linux
  does at that moment. The Mac driver does the rest natively (flush, echo
  of `^C`) and signals the Mac foreground group (Mac programs, below).
- **SIGTTIN/SIGTTOU**: a Linux process not in its terminal's foreground
  group reading it, writing it with TOSTOP, or changing it (`tcsetattr`,
  `tcsetpgrp`, ...) gets SIGTTIN/SIGTTOU, or EIO for an orphaned group or a
  blocked/ignored signal, as Linux rules (bash's `tcsetpgrp` dance depends
  on them).
- `TIOCSCTTY`, `TIOCNOTTY`, `setsid`, `tcsetpgrp`/`tcgetpgrp`
  (`TIOCSPGRP`/`TIOCGPGRP`), `TIOCGSID`: the table, not the host. Closing
  the master hangs up: SIGHUP and SIGCONT to the session.
- SIGWINCH: on `TIOCSWINSZ` on the master, to the foreground group.
- `sync_host_mask` (the job-control signals mirrored onto host threads) goes.

## 7. Mac programs: stand-ins

`execve` of a Mac program from Linux process P (pid p):

1. **Spawn first**: plain `posix_spawn` (no `SETEXEC`) of the Mac program,
   with P's fds at their guest numbers (file actions, `CLOEXEC_DEFAULT`;
   without `SETEXEC` the sources are still open when the actions run, so
   the close-on-exec juggling goes), P's working directory, environment
   (the Mac's `PATH` and `SHELL`, `TERM` adjusted, as now), signal mask, and
   ignored signals (`posix_spawnattr_setsigdefault`/`setsigmask`). If it
   fails, `execve` returns the errno, which it can't today (item 13's "exits
   127" limitation goes).
2. **P becomes a stand-in**: its tasks end and its address space is freed,
   as in an exec; its table entry stays, with pid, parent, group and session,
   marked as Mac process M.
3. **Wait**: the host (or the tty agent) is M's parent. SIGCHLD to the host
   wakes a reaper that `waitpid`s Mac children (`WNOHANG | WUNTRACED |
   WCONTINUED`), translates the status (signal numbers to Linux's) and
   makes P exit, stop or continue in the table: P's parent's `wait`, SIGCHLD
   and job control see it as any child.
4. **Signals to P** (`kill`, `kill` to its group, keyboard signals for its
   group) are host `kill`s of M with the Darwin number. Signals from M to
   Linux processes: Mac programs see only Mac pids, and Linux pids (100000
   and up) aren't any; a Mac `kill 123456` fails with ESRCH and harms
   nothing.
5. **Terminals**: a Mac program whose Linux session has a terminal must
   have it as its controlling terminal (`less`, `git`'s pager, reads keys
   from `/dev/tty`) and be stopped by the driver if it reads it from the
   background. The first Mac program of such a session starts a **tty
   agent** (`macwl-run --tty-agent`, a small helper with no VM: `setsid`,
   `TIOCSCTTY` on the pty's slave). The agent spawns that session's Mac
   programs, putting each in a Mac process group for its Linux group, and
   makes the Mac foreground group follow the Linux one: the Mac group of the
   foreground job if it has Mac programs, else the agent's own (which
   ignores keyboard signals). Ctrl-Z then stops Mac `vim` natively
   (it restores the terminal first, as it does), the reaper sees it stop,
   Linux bash's `wait` says Stopped, `fg` continues it. Without a terminal,
   the host spawns Mac programs itself.

Everything item 13 tested keeps working this way: exit codes and signals
through `wait`, fds by guest number, pipes mixing Mac and Linux programs,
SIGSTOP/SIGCONT, Ctrl-Z, `bg`, `fg`, Ctrl-C on Mac jobs, `posix_spawn` of
Mac programs, Mac `git` with Mac `less`, full-screen Mac `vim`, `brew`,
Xcode's tools. A `setpgid` on a child gone Mac still gets EACCES (it has
exec'd).

## 8. Per-process state that is the host's today

From the code (`grep` in `crates/linux-abi/src` for host `chdir`, `umask`,
`sigaction`, `setrlimit`, `getpid` and friends):

| State | Today | One VM |
|---|---|---|
| cwd | already the guest's path per `Process` (`fs.cwd`), no host `chdir`; Mac programs get it with `addchdir_np` | as is |
| umask | host `umask` (`syscall.rs`, `UMASK`) | per process, applied to the mode of `openat` with O_CREAT, `mkdirat`, `mknodat`, Unix socket `bind`; the host's umask 0 |
| rlimits | per `Process` already (`prlimit64`) | as is; RLIMIT_NOFILE over guest fd numbers (already the guest's), RLIMIT_NPROC over the table |
| uid, gid | the Mac user's (`setids` allows only no-ops) | as is: the same for every process |
| Host fd table | per Mac process; each guest fd owns a host fd (`FdEntry.host`); fork sends them all over `SCM_RIGHTS` | one for the host: fork shares open files (`Arc`) instead of duplicating host fds; close-on-exec is the guest entry's flag already; a Mac program gets only the fds it inherits |
| `/proc/self` | emulated per `Process` | as is, plus `/proc/<pid>` for every process of the host from the table (item 15, without asking macOS) |
| Signal dispositions | mirrored onto host `sigaction` (`sync_host_signals`); default stops are `raise(SIGSTOP)` of the host; `HOST_PENDING` is the host's | the host's own handling only (5); pending sets per process |
| Interval timers | host `setitimer` (`misc.rs`) | per process, on POSIX timers' machinery (`timer.rs`), whose fired list (`FIRED`) is keyed by pid and tid instead of tid |
| `getrusage` | host `RUSAGE_SELF`/`RUSAGE_CHILDREN` | the scheduler's per-task CPU time, summed per process and over reaped children; `CLOCK_PROCESS_CPUTIME_ID` likewise |
| `getpid`, `getppid`, groups, sessions, `kill`, `tcsetpgrp` | host calls (`syscall.rs`, `process.rs`, `signal.rs`, `io.rs`) | the table (5, 6) |
| Process-wide statics in linux-abi | `signalfd`'s `KQUEUES`, `futex`'s `WAKERS`, `timer`'s `FIRED`, `signal`'s `HOST_PENDING` | per process (they assume one process per Mac process); `KICK`, `MUST_YIELD`, `net`'s in-flight table stay the host's |

## Pieces

### Host

```rust
struct Host {
    vm: Vm,
    pool: Pool,                       // frames, refcounts, per-CPU chunks; IPA [0, 512 GiB)
    windows: SharedWindows,           // IPA [512 GiB, 1 TiB), shared by address spaces
    kernel: PageTable,                // TTBR1: the vectors, global
    asids: AsidAllocator,
    sched: Sched,                     // run queue, CPUs, workers (mn-threads.md), host-wide
    procs: Mutex<ProcessTable>,       // pid -> Arc<Proc>; groups; sessions; ttys
    pids: PidAllocator,               // pids and tids, 100000 up
    reaper: MacChildren,              // Mac programs (stand-ins) and tty agents
    clients: Listener,                // MACWL_HOST socket
}

struct Proc {
    pid: i64,
    abi: linux_abi::Process,          // fds, fs, signals, rlimits, umask, ...
    space: Arc<Space>,                // page tables, VMAs, ASID (shared by vfork, CLONE_VM)
    state: State,                     // Running, Stopped, Zombie(status), Mac(pid)
    parent: i64, pgid: i64, sid: i64,
    children: Vec<i64>,
    client: Option<ClientLink>,       // the Mac process standing in for it
}
```

`linux_abi::Process` keeps the per-process state; what it asks the host
(`kill`, `wait`, groups, the terminal) goes through a trait the runtime
implements (the `Kernel` the dispatcher gets with the process), so
`linux-abi` stays free of Hypervisor.framework, and 1:1 mode can implement
the same trait with host calls during the transition.

### Task

Gains its process: `Task { proc: Arc<Proc>, thread, ctx, ... }`. The CPU loop
of `mn-threads.md` adds, before resuming a task:

```
if vcpu.space != task.proc.space: TTBR0_EL1 = root | asid << 48   // 0.008 µs
```

### Clients and agents

`macwl-run` gains three modes: `--serve PATH` (a host with no program of its
own, for `session.sh`), client (when `MACWL_HOST` is set), `--tty-agent`
(started by the host). The wire format is `linux_abi::wire`, as fork's
state transfer is now; fds go with `SCM_RIGHTS`.

## What stays the same, and what changes

- **System calls, memory, files**: the dispatcher, VMAs, page faults,
  fault-around, shared windows, `GuestMemory` are per process as now; only
  the frames come from the host's pool.
- **The flush-before-return rule**: the same, by ASID.
- **Threads**: unchanged in kind; there are more of them on the same CPUs.
- **`execve` of a Linux program**: a new address space and ASID; the old
  ASID flushed and freed. No VM unmap and remap of the pool (today's exec
  does `hv_vm_unmap` and `hv_vm_map` of 8 GiB).
- **Wayland**: unchanged. The compositor is a Mac process; buffers are
  shared windows (`wl_shm`), now shared by IPA when two Linux processes map
  the same one. Nothing new on the input-to-glass path.
- **Mac programs**: a separate process (posix_spawn) instead of the
  process itself; the stand-in carries its Linux identity.
- **Activity Monitor** shows the session as one process (its CPU and memory
  are the session's), instead of a `macwl-run` per Linux process; Linux
  `ps` and `top` see every process (`/proc` from the table).

## Steps

Each ships with all tests passing; 1:1 mode stays the default until step 6.

1. **Host state out of `linux-abi`, no behaviour change.** The `Kernel`
   trait for what the dispatcher asks of the outside (pids, `kill`, `wait`,
   groups, sessions, terminal groups), implemented with host calls as now;
   per-process `umask`, interval timers, `getrusage`; `FIRED`, `KQUEUES`,
   `WAKERS`, `HOST_PENDING` keyed by process. Tests: all existing.
2. **One pool and ASIDs, still one process per VM.** `Pool` (40-bit IPA,
   512 GiB reserved, bookkeeping growing as used, reference counts, 64 KiB
   chunks) split from `AddressSpace` (page tables, VMAs, ASID, nG on user
   pages, one `TTBR1` per VM); the vector's flush path by ASID; `execve`
   takes a new ASID instead of remapping the pool. Tests: the memory unit
   tests, the runtime's tests, `switch_cost` (cross-process rows), exec
   cost (`forkbench exec`) no slower.
3. **Processes in the host, behind `MACWL_ONE_VM=1`.** The process table,
   pids from 100000, init; the host's run queue and CPUs with `TTBR0`
   switches; fork as copy on write, vfork sharing the space; exit, zombies,
   `wait4`/`waitid`, SIGCHLD; `kill`, groups, sessions, stops, all in
   memory; Mac programs refused (ENOEXEC) and processes with a controlling
   terminal not yet supported under the flag. **The first measurable win**:
   `forkbench` and bash's `( : )` and `true` loops (expect 10–50x, ~6x),
   `procstress` with 500 processes alive at once (more than 128), and Wine
   (wineserver and its clients, no terminal needed) in one VM. Tests:
   `processes`, `signals` (non-interactive parts), `procstress`,
   `manythreads` and `threads` with several processes, under the flag.
4. **Terminals and job control.** Ptys' sessions and foreground groups,
   keyboard signals on master writes, SIGTTIN/SIGTTOU, SIGHUP on hangup,
   stops parked; the relay for a host started in a Mac terminal. Tests: bash
   job control in `signals` under a pty; foot through the headless
   compositor (`MACWL_TYPE`): Ctrl-C, Ctrl-Z, `jobs`, `bg`, `fg`, vim.
5. **Clients.** `--serve`, `MACWL_HOST`, the client protocol, exit status,
   forwarded signals, the relay, hangup when a client dies; `session.sh`
   starts one host. Tests: a client's exit code and signal status; `kill
   -TERM`/`-INT` of a client from the Mac reaching its Linux process; a
   client killed with SIGKILL hanging its process up; two clients in one
   host seeing each other's processes in `/proc`.
6. **Mac programs as stand-ins.** Spawn first, the reaper, signals, tty
   agents, Mac groups following Linux ones. Tests: item 13's list
   (scripted where it was by hand: Mac `uname`, `false`, `env`, mixed
   pipes, exit codes and signals through `wait`, fds by number, cwd, Ctrl-Z
   on Mac `sleep`, a `sleep | tr` pipeline and `vim` in foot, `bg`/`fg`,
   Ctrl-C, `git log` through `less`, 20 `posix_spawn`s in a row, a Mac
   binary that fails to exec now failing `execve` with its errno). Then
   **one VM becomes the default**; `MACWL_ONE_VM=0` keeps 1:1 for a release.
7. **Shared windows and futexes across processes.** Windows deduplicated by
   object and range, private file windows shared, non-private futexes keyed
   by IPA (ROS target, item 5). Measured: Firefox's start and memory with
   and without; Fast DDS's shared-memory transport.
8. **1:1 mode goes.** A host for one program (`--new-host`, the default for
   a `macwl-run` without `MACWL_HOST`) is the isolated mode, so `fork.rs`'s
   spares, Mach memory entries and control sockets, and `exec_host`'s
   `SETEXEC` path, are deleted.

**1:1 as a fallback**: yes during the transition (the flag, steps 3–6), not
permanently: isolation is a host of its own, which needs none of 1:1's
machinery. Step 2 is a refactor with no win of its own beyond a cheaper
`execve`; step 3 is where fork gets fast and the 128 cap stops counting
processes.

## Tests

- `forkbench` (fork, exec, spawn; 0 and 200 MB), bash's `( : )` and `true`
  loops: step 3 meets the estimates, and the latency benchmark (foot,
  `MACWL_AUTO_KEYS`) is no slower.
- `switch_cost`: cross-process switches with ASIDs within 0.1 µs of thread
  switches; every round ran the right process's code.
- Copy on write: a child writing every page of a 200 MB parent sees its own
  copy, the parent its own; after the child exits the parent's pages are
  writable again without copies (counted); `MAP_SHARED` memory stays shared
  across fork; a vfork child's writes are the parent's.
- More processes than VMs: `procstress` with 500 alive at once, in one host;
  `hv-limits vms` still finds 127 free VMs while it runs.
- Process model: `wait4`/`waitid` for exit, signal, stop, continue (each
  option), zombies, orphans reparented to init and reaped, a subreaper,
  `kill` to a pid, a group, 0, -1; `setpgid`/`setsid` errors as Linux's
  (EACCES after exec, EPERM across sessions); sender pid in `siginfo`.
- Terminals: background reads and writes get SIGTTIN/SIGTTOU, `tcsetpgrp`
  from the background with SIGTTOU blocked works (bash's case), Ctrl-C/Z
  reach only the foreground group, hangup on master close.
- Clients: exit status, Mac-side signals, terminal relay, death of a client.
- Mac programs: item 13's list (step 6), plus `execve` errors.
- Isolation: a guest that faults ends alone; a runtime error unwinding in
  a system call (a test-only call that fails an assertion) ends only its
  process.
- The existing runtime and `linux-abi` tests, in both modes while 1:1 exists.

## Open questions

- **Fault containment**: is `catch_unwind` per call enough, or should some
  apps (a browser) get a host of their own by default (`--new-host` in their
  launcher)?
- **pids across hosts**: hosts are separate pid spaces. Should their pids
  also be unique across hosts (a random starting point per host, or a
  shared counter), so a pid in a shared `/tmp` pidfile can't name another
  host's process? Do any programs choke on pids above 100000?
- **Fairness between processes**: per task (Linux without cgroups), or
  per session/process weights (autogroup)?
- **Blocked tasks**: a worker host thread each, as now, caps a host at
  ~12000 blocked threads (`kern.num_taskthreads`). Waiting for fds in a
  kqueue instead of a thread would lift it; when does a real session need
  that?
- **Mac programs and the controlling terminal**: can `posix_spawn` give one
  its terminal (`POSIX_SPAWN_SETSID` and opening the slave), or is the tty
  agent required? Its launch costs ~2–3 ms, once per terminal session that
  runs a Mac program.
- **Activity Monitor**: the session is one process there. Should the host
  report per-Linux-process CPU and memory somewhere the Mac can show (a
  `ps`-like tool, or the shell's UI)?
- **Memory pressure**: macOS's memory pressure handling sees one large
  process; the host's own out-of-memory policy (step 3) decides which Linux
  process goes.
- **The host's lifetime**: should a session's host outlive its last client
  (a daemon left running), and for how long?
