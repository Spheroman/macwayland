# Processes in one VM: a host per session

Status: design (2026-11-02), open questions decided (2026-11-02, see
"Decisions"); steps 1 to 6 done (2026-11-02: processes in one VM,
terminals and job control included, `macwl-run`s from outside joining a
host as clients, and Mac programs as processes of their own standing in
for the Linux processes that ran them; one VM the default); step 7 done
(2026-11-03: shared windows and futexes across processes); step 8 done
(2026-11-10: 1:1 mode gone, one VM the only mode). Step 9 is next.
ROADMAP items 6 and 13;
extends the thread
scheduler (`docs/mn-threads.md`) one level up, from the threads of a
process to the processes of a session.

## Decisions

The open questions were settled with one rule: **when one answer is more
powerful but takes longer to build, take it.** So:

- **Isolation: federated hosts** (section 1, "Federated hosts", step 9). One
  host per session stays the default, with per-system-call containment. Any
  program can also get a host of its own (`--new-host`, or a per-program
  setting) and still be an ordinary child in the same Linux pid space:
  `wait`, signals, process groups and job control work across hosts through
  a per-user coordinator.
- **pids are unique across all hosts**: hosts take pids in ranges handed out
  per user (section 5, "pids").
- **Fairness: hierarchical weighted fair scheduling**: Linux session
  (autogroup), then process, then thread, each level weighted, in the EEVDF
  design of `mn-threads.md` step 5 (section 4).
- **Blocked tasks wait in kqueues**, not a host thread each, which removes
  the `kern.num_taskthreads` cap (12288): step 3a, after step 3.
- **Visibility: an accurate per-process `/proc`** (`stat`, `status`,
  `statm`, `maps`, `fd`, `cmdline`, `environ`, `task/`) so `ps`, `top` and
  `htop` work inside the session, **plus a Mac-side view**: `macwl-run --ps`
  lists a host's processes over its control socket. Activity Monitor shows
  one process per host, which is fine given those (section "Visibility").
- **A host lives while it has processes or clients**, and exits shortly
  after the last goes (section 1).
- **Tty agents**, not `posix_spawn` giving each Mac program the terminal
  itself: settled by experiment at step 6 (section 7).
- **Mac programs are always started by an agent**, a terminal's or the
  host's agent without one, never by the host itself (step 6, by the
  rule).

## The problem

Hypervisor.framework allows **128 VMs system-wide**
(`kern.hv.max_address_spaces`, read-only; `hv-limits vms`) and one VM per
Mac process. Before this design (until step 8) each live Linux process was
a Mac process with a VM of its own (1:1 mode), so at most ~128 Linux processes could be
be alive at once, on the whole Mac, shared with every other hypervisor
user. A Wine session uses 10–20; a desktop with a browser (Firefox: a
parent and ten or so children) and a few terminals full of tools gets
close; a program respawning children in a loop takes the machine's VMs
from everybody.

A Mac process per Linux process cost time too. A fork launched (or took a
pre-launched) `macwl-run --fork-child`, snapshotted guest memory as a Mach
memory entry, sent the fd table over a socket and built a new VM: 2.6 ms
per fork in a loop, against 0.5 ms for the Mac's own bash (below). And the
Linux process model leaned on the Mac's: pids, process groups, sessions,
`wait`, signals and job control were the Mac's own, adjusted where they
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

Decided: **one host per session**, and a separate host when asked for
(`--new-host`, or a per-program setting), for isolation, never
automatically. A separate host is still part of the session's Linux process
model (federated hosts, below), so isolation doesn't cost `wait`, signals or
job control. Against a pool or spilling:

- **Caps.** The 128-VM limit then counts sessions (and isolated apps), not
  processes. A host uses 18 vCPUs of the 64 a VM may have. Its fds share
  `kern.maxfilesperproc` (184320): a large session uses a few thousand
  (in-VM fork shares a description instead of duplicating it, see 8). Its
  host threads share `kern.num_taskthreads` (12288): 18 CPUs plus a worker
  per blocked task, until blocked tasks wait in kqueues instead (step 3a),
  after which only calls with no readiness to wait for (`flock`, a slow
  disk read) take a thread. 1 TiB of IPA space holds the memory of any
  session the Mac's RAM can. Nothing forces a second host, so there is
  nothing to spill.
- **Spilling costs everything a host saves.** Processes that land in
  another host by chance pay for crossing it on every fork, `wait` and
  signal; a fork into another host is today's fork (2.6 ms, a memory
  snapshot), and processes sharing memory across hosts get no shared
  futexes. Federation (below) pays that only for programs that asked for a
  host of their own, at their `execve`, where nothing is shared but fds.
- **Fault isolation** is the cost of one host. Guest faults are already
  contained (they become the faulting process's signal). A runtime error
  that unwinds in a system call (a failed assertion, an `unwrap`) is caught
  (`catch_unwind` around `dispatch`) and ends that process alone, with
  SIGSYS, logged; locks recover from poisoning already
  (`unwrap_or_else(|e| e.into_inner())` throughout). What can't be
  contained (an abort, memory corruption, a Hypervisor.framework error)
  ends the host and the session with it, as with any VM: clients report it (exit status 128 + 9, with a message), and
  `session.sh` can start a new one. An app that brings the runtime down,
  or that shouldn't share it, runs in a host of its own (`--new-host`, or
  its per-program setting), federated with the session's.

### Federated hosts (step 9)

A program can run in a host of its own and still be an ordinary process of
the session: its parent `wait`s for it, signals reach it by pid and by
group, it can be in its shell's job, its terminal is the session's. The
pieces that make that work:

- **Which programs.** `macwl-run --new-host PROGRAM` for one started from
  outside; for one exec'd from inside, a per-program setting
  (`~/.config/macwl/hosts`: an executable path or name per line, matched at
  `execve`), or `MACWL_NEW_HOST=1` in the environment of the `execve`.
  Firefox's launcher, say, so a browser crash in the runtime ends the
  browser and not the session.
- **The coordinator**: one per user (`macwl-run --coordinator`, a Unix
  socket in `$TMPDIR/macwl-$UID/`), started by the first host that needs
  it, exiting when no host is connected. It knows which host owns which
  pid range, and which hosts have members of a process group or session
  that spans hosts. It forwards; it holds no process state of its own.
  (As built, step 9b: a host and the one a process moved to from it talk
  over a direct link instead, the socket the hand-off came on; the
  coordinator lists hosts.)
- **pids.** Hosts allocate pids from ranges (4096 pids) handed out by the
  coordinator (section 5, "pids"), so a pid names one process across every
  host, and the owner of any pid is a lookup of its range.
- **Moving at `execve`.** An `execve` of a program set to have its own host
  is where it moves: the host starts one (a fresh `macwl-run --host`, ~3 ms,
  as a 1:1 fork's spare child took), and sends it the exec: argv, environment,
  cwd, root and binds, credentials, rlimits, umask, signal dispositions and
  mask, the pid, parent, group and session, and the fds (`SCM_RIGHTS`; one
  open file description each, as for a Mac program). The new host gives
  the process its pid and claims it at the coordinator. The old host keeps
  a **remote stand-in** in its table, as for a Mac program (7): pid, parent,
  group, session, state. Fork never crosses hosts: only `execve` does, so
  nothing shares memory across hosts.
- **`wait` and SIGCHLD.** The stand-in is what the parent waits on: the
  child's host reports exit, stop and continue (with status and rusage) to
  the parent's host through the coordinator, which makes the stand-in a
  zombie, stopped or running, and sends SIGCHLD, as for a Mac program's
  stand-in. `wait4`, `waitid` and `WNOWAIT` then work as for any child.
- **Signals.** `kill(pid)` looks up the pid's range: local, delivered in
  memory; a stand-in, forwarded to the owning host (with the sender's pid
  and uid for `siginfo`). `kill(-pgid)` signals the local members and, if
  the group spans hosts (the coordinator says so), sends it to the
  coordinator, which fans it out to the hosts with members. `kill(-1)`
  goes to every host of the session.
- **Process groups and sessions across hosts.** A group or session is owned
  by the host of its leader. A process moving to another host stays in its
  group: the group gets registered with the coordinator as spanning both,
  and stops being so when the last remote member exits. `setpgid` on a
  child in another host is a request to that host (EACCES as Linux, since
  it has exec'd). `setsid` in the new host makes a session owned there.
- **Terminals and job control.** A pty belongs to the host that opened it
  (or the client's relay). A process in another host holds the slave as an
  fd it was passed; the pty's owner keeps the foreground group, so
  `tcsetpgrp` and the background check (SIGTTIN/SIGTTOU, EIO) of a remote
  reader or writer ask the owner (through the coordinator; the answer is
  cached with a generation number the owner bumps on `tcsetpgrp`).
  Keyboard signals to the foreground group fan out as `kill(-pgid)` does.
  Stopping and continuing a remote member parks and unparks it in its own
  host.
- **Orphans.** A process whose parent exits is reparented to its own host's
  init (or a subreaper in its host), which reaps it; a stand-in whose
  parent exits goes with it and the remote process is told it's now
  init's. Linux allows any reaper; programs only see `getppid` change.
- **A host's failure.** If a federated host dies, its processes end: the
  coordinator tells the hosts with their stand-ins, which make them exit
  with SIGKILL (as clients of a host that died do, 1). If the
  coordinator dies, hosts carry on with their local processes, and
  cross-host operations fail with ESRCH until a new one has been told
  each host's ranges and spanning groups (they re-register).

Steps 3 to 8 keep the way open, so step 9 adds rather than reworks:

- pids come from a `PidAllocator` (step 3): a local range at first, the
  coordinator's ranges later;
- the process table's entries are a kind: local, Mac stand-in (step 6),
  remote stand-in (step 9); `wait`, signals, groups and job control go
  through one routing function (`deliver(pid or group, signal, sender)`)
  that already distinguishes local from stand-in members;
- ptys and groups are found by id in the table, not by pointer, so a
  remote member is an id with an owner;
- the client protocol (step 5) is the same wire format as the exec hand-off
  above, so a host accepting a client already accepts a moved process.

### The host's lifetime

A host lives while it has Linux processes (daemons included) or connected
clients, and exits a short grace period (2 s) after the last goes, so that
a quick sequence of `macwl-run` invocations, or a restarted `session.sh`,
reuses it rather than starting a new one. `--serve` hosts have the same
rule once their first client has come.

### How a `macwl-run` finds its host

- `MACWL_HOST=PATH` (or `--host PATH`): join the host listening on the Unix
  socket at PATH. `scripts/session.sh` starts one (`macwl-run --serve
  "$XDG_RUNTIME_DIR/macwl-host"`) and exports `MACWL_HOST`, so `foot`, the
  D-Bus daemon and anything started from the session (including a Mac
  terminal that sources the session's environment) share it.
- Otherwise (or with `--new-host`): the `macwl-run` process **is** a host,
  for the program it was given and everything that program starts: today's
  behaviour, with in-VM fork for the descendants. It exits with its first
  process's status once that has ended and the others have gone (or, if
  they outlive it, when the last does), as "The host's lifetime" says.

A client, like `docker exec`, sends the host its argv, environment, working
directory, root and binds, umask, the signals it ignores and blocks, and
its fds 0–2 (over `SCM_RIGHTS`) or a terminal (below), and gets back the
Linux pid. Then (as built at step 5):

- **Exit status**: the host tells the client how its process ended; the
  client exits with the code, or raises the signal on itself, so a Mac
  shell sees a job killed by it. Signals that would dump core (a crash
  report for the client, not the program) are 128 + the signal instead.
- **Signals from the Mac side** (`kill` from a Mac shell): the client
  catches every catchable signal (but SIGTTIN and SIGTTOU, which stop the
  client itself when it touches its terminal from the background) and
  forwards it to its Linux process's group, as a terminal sends its
  foreground job's. SIGKILL ends the client: the host treats that as the
  terminal hanging up (SIGHUP and SIGCONT to its session's leader, the
  process, which as a shell hangs its jobs up; without a terminal, to the
  process's group). SIGSTOP stops only the client (and so its relay); the
  Linux side carries on until its output blocks.
- **Its terminal**, if stdin is one: the Linux process's terminal is a pty
  of the host's, whose job control the host does (6), with the Mac
  terminal's termios and size; the client puts the Mac terminal in raw
  mode, as `ssh -t` does. The host passes the client the pty's master: the
  client copies its output straight to the Mac terminal, and sends what's
  typed through the host, which writes it with the keyboard's signals for
  the foreground group (Ctrl-C and Ctrl-Z are bytes). SIGWINCH sends the
  new size, SIGHUP or the terminal's end hangs the pty up. If the client's
  own Linux process stops (Ctrl-Z, or any stop), the client restores the
  Mac terminal and stops itself (SIGSTOP), so the Mac shell sees a stopped
  job; on SIGCONT it continues the Linux process, and goes raw again if
  it's in the foreground (`fg`; after `bg` it leaves the terminal alone).
  A process continued from inside continues its client too. Without a
  terminal there's no relay: the client's fds are the process's.

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
  (every Firefox process maps libxul); and futexes on shared memory get a
  key every process agrees on, as Linux keys them by page (ROS target,
  item 5; as built, step 7, the key is the object and offset rather than
  the IPA). Private read-through file windows (libraries, fonts) are
  shared the same way. Making a window costs 0.5 µs whatever its size.

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
- **`CLONE_VM` without vfork** (not supported in 1:1): two processes
  sharing one address space, now possible.
- `fork.rs`'s spares, Mach memory entries and control sockets were 1:1
  mode's alone, and went with it (step 8).

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
- **Fairness: hierarchical weighted fair scheduling** (decided). Three
  levels, each EEVDF over weighted entities, as Linux's group scheduling
  nests `sched_entity`s: the **Linux sessions** of the host (autogroup:
  each `setsid` makes one, so a terminal's build doesn't starve another
  terminal's editor), then the **processes** of a session, then the
  **threads** of a process. A level's weight is its nice value's (Linux's
  table; a session's from its autogroup nice, `/proc/<pid>/autogroup`), and
  a thread's slice of the CPU is its share of its process's share of its
  session's. Real-time and idle classes stay flat and outside the
  hierarchy, as on Linux. This is `mn-threads.md` step 5's EEVDF with
  group entities: it lands on the host's run queue, not a process's, so
  step 5 there is built after step 3 here.
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
| pid, tid | the host's pid; tids made up from it | from the host's `PidAllocator`, 100000 and up (above macOS's 99999): a Linux pid handed to a Mac program can never name a Mac process; unique across hosts (below) |
| pid 1 | the Mac's launchd | the host's init: orphans are reparented to it (or to a `PR_SET_CHILD_SUBREAPER`), it reaps them; it gets no signals |
| `getppid` | host `getppid` | the table (1 for a client's process) |
| `setpgid`, `setsid`, `getpgid`, `getsid` | host calls, a control-socket round trip for children (they have exec'd as far as macOS knows) | the table, Linux's rules: EACCES once a child has exec'd, EPERM across sessions |
| `wait4`, `waitid` | host `wait4`, plus "killed by N" on the control socket | zombies in the table; WNOHANG, WUNTRACED, WCONTINUED, WNOWAIT, `P_PGID`; rusage from the scheduler's accounting |
| SIGCHLD | the host's, from macOS | sent by the host when a child exits, stops or continues; `SA_NOCLDWAIT`/ignored SIGCHLD reap at once |
| `kill`, `tgkill` to others | host `kill`, number translated | in memory, with sender pid and uid in `siginfo` (item 7's gap); `kill(-1)` now possible (every process of the session but init and the caller); to a Mac stand-in, host `kill` of its Mac process |
| Default actions | the host's own (dispositions mirrored onto host `sigaction`) | emulated: terminate ends the process, stop parks it (4), continue resumes |
| `SO_PEERCRED` | the peer Mac process's | the peer's Linux pid, recorded when the connection is made, if both ends are in the host |

### pids

Unique across every host of the user (decided), so a pid in a shared
`/tmp` pidfile, a lock file or a D-Bus message names one process, and
federated hosts (step 9) can route by pid. A host takes pids in **ranges of
4096**, from 100000 up to Linux's `pid_max` (4194304; nothing seen so far
minds pids that large, and Linux hands them out on big machines), and asks
for another range when one runs out. Until the coordinator exists (step
9), ranges are handed out through a per-user file (`$TMPDIR/macwl-$UID/pids`,
under `flock`) that records each range's owner by Mac pid; a range whose
owner is gone is free again. From step 9 the coordinator hands them out
from the same file, and knows which host owns which pid. Within a range, a
pid isn't reused until the range has wrapped, as Linux avoids quick reuse.

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
   fails, `execve` returns the errno, which 1:1's exec couldn't (item 13's
   "exits 127" limitation goes).
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
   a Mac program is started by the host's agent without one (as built: see
   below).

**The experiment (step 6, 2026-11-02): tty agents.** A C program on the
Mac (macOS 27.0) opened a pty and tried what `posix_spawn` alone can do:

- `POSIX_SPAWN_SETSID` with the slave opened (without `O_NOCTTY`) in the
  file actions: the child **has** the pty as its controlling terminal
  (`/dev/tty` opens; `ps` shows the tty, and the child's group in the
  foreground).
- A second child spawned the same way while the first lives: **no**
  controlling terminal (`/dev/tty` fails): a pty is one session's. So
  two Mac programs of one Linux session at once (`/bin/sleep | tr`, a
  background Mac job and a foreground one) can't both have it.
- `tcsetpgrp` from a process outside the Mac session, through the slave
  and through the master: **ENOTTY**. Only a process in the session can
  make the Mac foreground group follow `fg`, `bg` and bash taking the
  terminal back.

So `posix_spawn` with a controlling terminal works for one Mac program at
a time and can't do job control; agents can, and were built. Three more
findings shaped them: `POSIX_SPAWN_START_SUSPENDED` starts a child stopped
(its parent's `waitpid` sees the stop; SIGCONT starts it), which lets an
agent put a new Mac group in the foreground before its program first
touches the terminal; macOS sends a parent **no SIGCHLD when a child
continues** (`EVFILT_PROC`'s `NOTE_SIGNAL` fires, and `waitpid` then
reports `WCONTINUED`), so agents watch their children's signals as well as
SIGCHLD; and SIGCHLD must stay at its default in an agent (ignoring it
would reap its children before it could report them).

**As built (step 6).**

- **Agents start every Mac program** (`crates/runtime/src/mac.rs`):
  `macwl-run --tty-agent [TTY]`, started by the host with a plain
  `posix_spawn` in a Mac session of its own (`POSIX_SPAWN_SETSID`), two
  sockets to the host (requests; reports of its children). With TTY (a
  Linux session's pty) it takes the pty as its controlling terminal
  (`TIOCSCTTY`); the host has one agent per such pty (64 at most; past
  that, a session's Mac programs go to the next kind) and one without a
  terminal, for sessions with none, so the host never spawns a Mac
  program itself and never changes its own signal dispositions for one.
  An agent never makes a VM (asserted at its start and at each spawn).
- **Spawn first.** The `execve` sends the agent the program, argv, the
  environment (the Mac's `PATH` and `SHELL`, `TERM` one macOS's ncurses
  reads), the guest's cwd, its inherited fds (`SCM_RIGHTS`, in batches of
  64) with their guest numbers, its ignored signals and the thread's mask,
  and the process's Linux group; the agent spawns it (fds at their guest
  numbers by way of fds above all of them, `CLOEXEC_DEFAULT`,
  `addchdir_np`, `SETSIGDEF` for all but the ignored ones, which it
  ignores itself around the spawn, `SETSIGMASK`, `SETPGROUP`) and answers
  with the Mac pid and group, or the errno: `execve` returns that errno
  (a Mach-O macOS won't run is ENOEXEC: bash says 126, "cannot execute
  binary file"). Then the process's threads end as in an exec, its memory
  goes, and its entry becomes a stand-in (`Kind::Mac`), with `execd` set
  (`setpgid` on it: EACCES). A vfork parent runs on then.
- **Groups and the foreground.** An agent keeps a Mac group per Linux group
  (the first Mac program of a Linux group leads a new one; later ones
  join it, or lead a new one if it has gone). The host tells a terminal's
  agent each `tcsetpgrp` and waits for the answer, so the Mac foreground
  has followed before bash goes on to continue the job; a new Mac group
  for the Linux foreground group starts suspended, is put in the
  foreground, then continued.
- **Reports.** An agent `wait4`s its children (WNOHANG, WUNTRACED,
  WCONTINUED) on SIGCHLD and on each child's signals, and sends the host
  each change with the Darwin status (and CPU time on exit); the host's
  **reaper** (one thread per host process, in the thread budget) hands
  them to the table, which translates them to Linux numbers and makes the
  stand-in stop, continue (SIGCHLD to its parent, `wait`'s reports, the
  relay or client told) or end (a zombie as any other; the first
  process's exit status, a client's). Reports that come before the
  process's tasks have all gone wait in its entry.
- **Signals to a stand-in** are a host `kill` of its Mac program (by pid;
  for a group, `kill` of the Mac group, which reaches the Mac processes it
  started, as `less` under `git`), with the Darwin number; signals Darwin
  has no number for (real-time ones) don't reach it. A keyboard signal
  for a terminal's foreground group goes to the Linux members only: the
  Mac driver has signalled the Mac foreground group itself.
- **What the table shows**: `/proc/<pid>` of a stand-in has its state (S,
  or T while stopped), parent, group, session, terminal, the Mac
  program's command line and name, and its CPU time and resident memory
  from macOS (`proc_pid_rusage`); `ps` and `top` list it. `macwl-run --ps`
  shows the Mac program's pid, marked `*`, in the column that has a
  client's.
- **Never an exec.** Every process the runtime starts for this (agents,
  Mac programs) goes through one function, `mac::spawn`, which asserts
  that its flags, as set and as read back from the attributes, don't
  include `POSIX_SPAWN_SETEXEC` (unit tests `a_spawn_never_replaces_its_caller`,
  `spawn_checks_its_flags_before_it_spawns`). The dispatcher asks the
  kernel, which says `StandIn`, an action of its own (`Action::MacStandIn`,
  `Next::StandIn`). 1:1's path (`exec_host`, `Action::ExecHost`, the
  process replaced with `SETEXEC`) went with it at step 8.

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
    pids: PidAllocator,               // pids and tids: ranges unique across hosts
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
`linux-abi` stays free of Hypervisor.framework. (1:1 mode implemented the
same trait with host calls during the transition; since step 8 only
`linux-abi`'s own tests do, for a process on its own: `HostKernel`.)

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
  `ps` and `top` see every process (`/proc` from the table), and the Mac
  side has `macwl-run --ps` (below).

## Visibility

Decided: accurate per-process `/proc` inside the session, and a Mac-side
listing; Activity Monitor's one process per host is fine given those.

- **`/proc/<pid>`** for every process of the host, from the table and the
  scheduler, not from macOS: `stat` (state, parent, group, session, tty,
  flags, utime/stime from the scheduler's accounting, nice and priority,
  thread count, start time in clock ticks since the host's boot time,
  vsize and RSS from the address space's accounting), `status` (the same,
  named, plus `Umask`, signal masks, `Threads`, `VmRSS`, `VmSize`), `statm`,
  `maps` (the VMAs, with files), `fd/` and `fdinfo/`, `cmdline`, `environ`
  (read from the process's memory, as Linux does, at the addresses the
  loader recorded), `comm`, `exe`, `cwd`, `task/<tid>/` for each thread,
  and `autogroup`. The system-wide files `ps` and `top` need too:
  `/proc/stat` (CPU time per CPU from the host's CPUs, boot time,
  `processes`, `procs_running`), `/proc/loadavg`, `/proc/uptime`,
  `/proc/meminfo`. Readdir of `/proc` lists the table's pids. Stand-ins
  (Mac programs, remote ones) appear with what the table knows (state,
  identity, `cmdline`) and their Mac process's CPU and memory where macOS
  tells it (`proc_pidinfo`).
- **`macwl-run --ps [--host PATH]`** on the Mac: asks a host over its
  control socket (the client socket, step 5) for its processes and prints
  pid, parent, group, session, tty, state, CPU time, RSS and command line,
  as `ps -ef` would inside; `--ps --all` asks every host the coordinator
  knows (step 9).
- ROADMAP item 15 (`/proc` from the macOS process list) becomes this from
  step 3: the table is the truth; macOS's list only fills in stand-ins.

## Steps

Each ships with all tests passing; 1:1 mode was the default until step 6.

1. **Host state out of `linux-abi`, no behaviour change.** The `Kernel`
   trait for what the dispatcher asks of the outside (pids, `kill`, `wait`,
   groups, sessions, terminal groups), implemented with host calls as now;
   per-process `umask`, interval timers, `getrusage`; `FIRED`, `KQUEUES`,
   `WAKERS`, `HOST_PENDING` keyed by process. Tests: all existing.
   (Done, 2026-11-02.) `linux_abi::Kernel` (`syscall/kernel.rs`):
   `getppid`, `getpgid`/`setpgid`, `getsid`/`setsid`, `kill` of other
   processes and groups, `wait4`, `waitid`, `tcgetpgrp`/`tcsetpgrp`, a stop
   signal's default action, `getrusage`, CPU clocks, umask changes; each
   `Process` holds one (`HostKernel`: the host calls, as before). Each
   process has an `Inbox` for what reaches it from outside its calls (host
   signals, timer expiries, its signalfds' kqueues, its threads' wakers);
   host signal handlers find the inbox of the process host signals are for
   (`Process::receive_host_signals`). The umask is the process's, applied
   to `openat` with O_CREAT, `mkdirat`, `/dev/shm` objects and Unix socket
   `bind` (`HostKernel` also mirrors it onto the host, so Mac programs and
   anything else the host creates still get it), and carried across fork.
   Interval timers run on the timer threads as POSIX timers do, per
   process, ITIMER_VIRTUAL and ITIMER_PROF on the kernel's CPU clock; their
   signals now carry `SI_KERNEL`, as Linux's do (before, a host SIGALRM
   arrived as `SI_USER`). `CLOCK_PROCESS_CPUTIME_ID` is still the host's,
   for step 3.
2. **One pool and ASIDs, still one process per VM.** `Pool` (40-bit IPA,
   512 GiB reserved, bookkeeping growing as used, reference counts, 64 KiB
   chunks) split from `AddressSpace` (page tables, VMAs, ASID, nG on user
   pages, one `TTBR1` per VM); the vector's flush path by ASID; `execve`
   takes a new ASID instead of remapping the pool. Tests: the memory unit
   tests, the runtime's tests, `switch_cost` (cross-process rows), exec
   cost (`forkbench exec`) no slower.
   (Done, 2026-11-02.) `paging::Pool` (frames, reference counts, 64 KiB
   chunks, host pages back to macOS in runs), `paging::PhysMem` now an
   address space's view of it (a chunk cache and its quarantine),
   `memory::Machine` (the pool, the kernel half, ASIDs) shared by the
   address spaces of a VM; VMs are made with a 40-bit IPA, shared windows
   at `[512 GiB, 1 TiB)`. `TCR_EL1.AS`, the ASID in `TTBR0_EL1`, nG on user
   pages; the vector's flush path is `tlbi aside1is` on `TTBR0_EL1`'s ASID
   (x0 borrowed through `TPIDR_EL1`, which `UserContext::save` knows), and
   a second path after the table flushes a given ASID. `execve` swaps
   address spaces in the same pool, flushes the old ASID and frees it.
   Three things the design didn't settle, decided by the rule (2026-11-02):
   - **The pool is reserved and mapped in segments that double** (8 GiB,
     then 8, 16, ... up to 512 GiB), each when frames are first handed out
     from it. Reserving 512 GiB of host address space at once costs 0.3 ms
     and giving it back 0.45 ms (8 GiB: 10–20 µs), which every process
     would pay while each has a VM; so the whole 512 GiB stays available
     (a guest past 8 GiB now works, which the 8 GiB pool refused) at no cost
     until used. A fork sends one memory entry per segment in use.
   - **Chunk caches are per address space**, not per CPU: while one process
     has a VM, its page faults already hold its address space's lock; per
     CPU comes with step 3, when CPUs run several processes.
   - **A gone address space's frames are given back by a thread of their
     own**, after its ASID's flush: walking 200 MB of page tables and
     giving the frames back took 0.3 ms on the `execve` path (the walk
     mostly the host faulting in table pages a forked child never touched).
     A fork waits for it, so its snapshot of the free list is whole.
   Measured (M5 Pro, `forkbench`, 50 rounds, mean after the first, three
   runs each, interleaved with the old build): no change within noise:
   fork 0 MB 2.19–2.28 ms (was 2.23–2.34), 200 MB 2.60–2.65 (2.65–2.69);
   fork + exec 0 MB 2.19–2.22 (2.24–2.26), 200 MB 2.87–2.91 (2.83–2.84);
   `posix_spawn` 0 MB 2.22–2.26 (2.25–2.27), 200 MB 2.86–2.95
   (2.88–3.12). The `execve` itself in a 200 MB fork child (`MACWL_PROFILE`,
   loaded to exec'd) 23 µs, was 100 µs (no pool unmapped and mapped again).
   `switch_cost` on the 40-bit VM: thread 1.36–1.54, ASID 1.37–1.62 µs a
   round up to 256 pages. The foot latency benchmark (`MACWL_AUTO_KEYS=40`)
   before: key→commit p50 2.24–2.28 ms, key→glass p50 10.2–11.1 ms; not
   repeated after (later that day the shell's window wouldn't go full
   screen with either build, so it took no samples): still to do. Tests: runtime 32 unit (2 new) and 23
   integration, linux-abi 59; a guest with 8.8 GB touched forks and execs.
3. **Processes in the host, behind `MACWL_ONE_VM=1`.** The process table,
   pids from the `PidAllocator` (ranges from the per-user pids file, so
   unique across hosts from the start), init, `/proc/<pid>` from the table
   (Visibility); the host's run queue and CPUs with `TTBR0`
   switches; fork as copy on write, vfork sharing the space; exit, zombies,
   `wait4`/`waitid`, SIGCHLD; `kill`, groups, sessions, stops, all in
   memory; Mac programs refused (ENOEXEC) and processes with a controlling
   terminal not yet supported under the flag. **The first measurable win**:
   `forkbench` and bash's `( : )` and `true` loops (expect 10–50x, ~6x),
   `procstress` with 500 processes alive at once (more than 128), and Wine
   (wineserver and its clients, no terminal needed) in one VM. Tests:
   `processes`, `signals` (non-interactive parts), `procstress`,
   `manythreads` and `threads` with several processes, under the flag;
   `ps -ef`, `top -b -n 1` and `htop` (by hand) inside a session.
   (Done, 2026-11-02.) Behind `MACWL_ONE_VM=1`
   (`crates/runtime/src/host.rs`):
   - **The host's CPU loop.** Tasks carry their process (`Proc`: its
     `linux_abi::Process`, address space, exit, parked tasks, CPU time);
     a CPU about to run another process's task writes `TTBR0_EL1` only
     (vCPUs remember the address space by a serial number, as ASIDs and
     roots come back). Address spaces that have gone are flushed by ASID
     by whichever CPU next looks, between tasks. 1:1 mode is the same
     loop with one process.
   - **Fork copy on write** (`AddressSpace::fork`): page tables copied,
     every pool frame read-only and marked (a software bit) in both, held
     once more; the parent's ASID flushed before the child is queued; a
     write fault or a system call writing guest memory makes the page
     the writer's own (a copy, or in place once no one else holds it).
     Shared windows became the VM's, counted by the address spaces using
     them. vfork shares the address space and parks the parent's task off
     its CPU until the child execs or exits. Open files are shared, one
     open file description each (`FdTable::fork`); signalfds are copied.
   - **The table** (`Table`, and `TableKernel`, the `linux_abi::Kernel` of
     every process in a host): parents, groups, sessions with Linux's
     `setpgid`/`setsid` rules, zombies, `wait4`/`waitid` in memory (WNOHANG,
     WUNTRACED, WCONTINUED, WNOWAIT, groups; the waiter sleeps on its
     thread's waker, and gives up for a signal or for its CPU), SIGCHLD
     with `si_status` (SA_NOCLDSTOP; SIG_IGN and SA_NOCLDWAIT reap),
     orphans to init, stops parking a process's tasks until SIGCONT (or
     SIGKILL), `getrusage` and `wait4`'s rusage from per-process CPU time
     the CPUs and workers count. Every signal between processes goes
     through `Table::deliver` (pid, group, all, thread), with the
     sender's pid in `siginfo`; entries are a `Kind` (local or zombie
     now; Mac and remote stand-ins later).
   - **pids** from a `PidAllocator`: ranges of 4096 from 100000, handed
     out through `$TMPDIR/macwl-$UID/pids` under `flock`; tids from the
     same.
   - **The host process never execs**: the kernel says no Mac programs, so
     such an `execve` fails with ENOEXEC in the dispatcher, and the CPU
     loop refuses an `ExecHost` too (`one_vm_refuses_mac_programs`, with
     `MACWL_MAC_PROGRAMS=1`).
   - Signals from the Mac side (Ctrl-C in the terminal, a `kill` of the
     host) go to the first process's group, or to every process once it
     has gone; SIGTSTP stops the host whole until step 4. The host lives
     while it has processes (wineserver keeps it a few seconds after
     `wine` ends) and exits with the first one's status.
   - Found on the way: a thread writing a page in a loop while another
     forks again and again faulted on it more than the three repeats a
     fault is allowed: a page made the writer's own no longer counts as a
     repeat.
   - **No wakeup is lost in a host call.** A thread blocked in a host call
     is reached by an interrupt (SIGEMT to its host thread); one that
     landed after the thread looked and before the call slept was lost.
     In 1:1 the Mac process's exit covered it; in a host an ending process
     never finished (sampled: a CPU in `read_host`'s `read` of a pipe no
     one writes; 2 of 120 hunt runs hung). As in `mn-threads.md`'s fixes,
     the mark comes first and the sleep checks for it: a waker changes what
     the thread should look at, writes the thread's waker pipe, then
     interrupts it; a thread waiting in a host call looks (a signal it
     would act on, its process ending, its CPU wanted) and then sleeps in
     `poll` on what it waits for **and** that pipe
     (`linux-abi/src/syscall/block.rs`), so a wake after the look ends the
     sleep however it's timed. Reads and writes of pipes, sockets and
     terminals wait there for readiness before the call (a socket's
     SO_RCVTIMEO/SO_SNDTIMEO bounding the wait); `accept`, `recv*`,
     `send*`, `ppoll`, `pselect6`, `epoll_wait` (on its kqueue),
     eventfd/timerfd/signalfd/inotify reads, `nanosleep` and
     `clock_nanosleep`; in one VM `rt_sigtimedwait`'s timed wait is
     Darwin's `pselect` with the old mask (a timed `sigsuspend` in one
     step, where it was 10 ms steps; in 1:1 a host signal can land on
     another thread of the process, so there it still looks every 10 ms:
     without, `wineboot` stalled 5 to 25 s in 3 runs of 8). No polling: the 5 ms re-waking of an ending
     process's threads that stood in for this is gone. Hunt (procstress,
     procs wait/groups/cow and threads, four of each at a time, any run
     alive after 30 s sampled): 0 hung of 1000, at 18 and 2 CPUs. Unit test
     `a_blocked_call_wakes_without_an_interrupt` wakes a read, a sleep, a
     poll and an eventfd read by the pipe alone. Still with only the
     interrupt: `flock`, `F_SETLKW` and a FIFO's `open` (nothing to poll),
     `connect`, and a read that loses a race for data another reader took
     after `poll` said ready (it blocks as before, interruptibly).
   - **Two older races** the hunt turned up: the flush path resumed a vCPU
     stopped by a kick in the program at a stale ELR_EL1 (an old pc), and
     starting the path over part-way through saved the borrowed ASID in
     place of the guest's x0 (procs cow failed 1 run in 100 with a
     spinning thread's pointer 0 or 1 << 48; 0 in 400 after); and a child
     whose parent ignores SIGCHLD was reaped after the table's lock had
     been let go once, so the parent's `waitpid` could report it rather
     than ECHILD.
   - **Timers signal their own process.** A timer's expiry (`setitimer`,
     `timer_create`, `alarm`) kicked every vCPU and interrupted every CPU
     and worker of the host: another process's `nanosleep` ended with
     EINTR. Threads' handles (pending set, waker, how to reach them on a
     CPU or worker) now live in the process's `Inbox`, which a timer's
     thread holds, and an alert reaches those threads alone (test
     `one_vm_timers_are_their_process_alone`).
   - **Containment.** A Rust panic in a system call, on a CPU or a worker,
     is caught (`contained`): logged, the call's files let go, and the
     process ends with SIGSYS; its parent's `wait` says so and the host
     carries on. A panic elsewhere in running a task (a fault, a fork) is
     caught around the CPU's turn and ends the task's process as any
     runtime error there does. A Mac program a process was about to
     become is forgotten after a panic, so no panic leads to a host exec.
     Debug builds answer call 0x4d570001 (`MACWL_TEST_PANIC`) with a panic
     (test `one_vm_contains_a_runtime_error`).
   - **`/proc` from the table** (Visibility): `/proc` lists every process
     of the host; `/proc/<pid>/{stat,status,statm,cmdline,environ,comm,
     maps,mounts,mountinfo,exe,cwd}`, `fd/` and `task/<tid>/` for each,
     from the table (`Kernel::proc_status`: parents, groups, state, CPU
     times, start time) and from the process itself
     (`Kernel::with_process`: its fds, cwd, regions, and its memory, from
     which `cmdline` and `environ` are read where the loader put them, as
     Linux does). `comm` is the base name of the path `execve` was given,
     or what the main thread set (PR_SET_NAME). Zombies show `Z`, with
     their name. Also `/proc/stat` (the Mac's CPUs' ticks, `btime`,
     process counts) and `/proc/sys/kernel/pid_max`. procps 4.0.7 (nixpkgs
     aarch64-linux) works inside a host: `ps aux`, `ps -ef`, `top -b -n1`,
     `pgrep`, `pidof` and `kill $(pidof sleep)`. In 1:1 mode `/proc` lists
     the process itself (its directories can be listed now too). Test
     `one_vm_proc_of_every_process`.
   - **The rest of the process model**: `clone` with CLONE_VM alone (no
     vfork) shares the parent's address space (one VM only; 1:1 says
     EINVAL as before); `PR_SET_CHILD_SUBREAPER`: orphans go to the
     nearest live ancestor that is one, and an orphaned zombie is
     reported to it as its own; CPU time per process with user and system
     apart (`getrusage`, `wait4`'s rusage, `times`, `/proc/<pid>/stat`),
     and `CLOCK_PROCESS_CPUTIME_ID` (and `clock_getcpuclockid` of another
     process) from it; pidfds (`pidfd_open`, ready in `poll`/`epoll` once
     the process has ended; `pidfd_send_signal`; `pidfd_getfd`, which
     shares the other process's open file; `waitid(P_PIDFD)`); `capget`
     (no capabilities) and `capset` (only dropping none), `prctl`'s
     PR_CAPBSET_READ (every capability to 40, EINVAL past) and
     PR_CAP_AMBIENT (empty), `get_mempolicy` (one node), `setsockopt`
     SO_REUSEPORT. Test `one_vm_process_model`.
   Choices the design left open, taken by the rule (2026-11-02):
   - **Pid reuse**: a host takes pids in turn across its ranges and asks
     for another range when three quarters of what it has are in use,
     else goes round its ranges again (skipping live pids): no reuse soon
     after a pid is freed, and a long-lived host doesn't hoard ranges.
   - **Stops park tasks**, off their CPUs (rather than blocking them in
     place), and so does a vfork parent: neither holds a CPU, so a host
     with one CPU still runs the child.
   - **Signals a process ignores** (SIG_IGN, or a default of ignore) end
     none of its waits, as on Linux where they're dropped as they're sent;
     they stay pending for `sigtimedwait` and signalfd.
   - **Mac-side signals go to the first process's group**, as a terminal
     sends its foreground job: `bash -c 'sleep 100'` ends on Ctrl-C.
   Measured (M5 Pro, `forkbench`, 50 rounds, mean after the first, two
   runs): fork + exit + wait 0 MB 65–69 µs (1:1: 2.2 ms), 200 MB 183–186 µs
   (2.6 ms); fork + exec 0 MB 120–124 µs (2.2 ms), 200 MB 230–233 µs
   (2.7 ms); `posix_spawn` 0 MB 100–109 µs (2.2 ms), 200 MB 97–98 µs (2.7
   ms). The estimates said 0.05–0.25 ms and ~0.5 ms with dynamic loading:
   met, 32x for a small fork. 300 processes alive at once in one host,
   one Mac process. arm64 Wine: `wineboot -i` exits 0 with 801 files in
   system32, `wine cmd.exe /c 'echo hi & ver'` prints both (2 s to the
   output from a cold host, against 0.9 s in 1:1; the host then waits
   about 4 s for wineserver and services to go). Tests: runtime 35 unit and
   28 integration, all of them in both modes (`MACWL_ONE_VM=1 cargo test`),
   among them `one_vm_copy_on_write`, `one_vm_zombies_and_wait`,
   `one_vm_signals_and_groups`, `one_vm_more_processes_than_vms`,
   `one_vm_refuses_mac_programs` (`guest/procs.c`); linux-abi 60.
   **Wine in one VM against 1:1.** The figures first compared weren't
   like for like. A one-VM host's time ends when wineserver and its
   services have gone, ~8 s after `wineboot` itself ends (the host's
   lifetime rule), where 1:1's `macwl-run` ends with `wineboot`: the
   ~14 s against ~5 s. And a 1:1 `wine` often met a wineserver the last
   one left running (in 1:1 they outlive `wine`, sometimes for minutes:
   0.56 s to `hi`), or a prefix whose registry wineserver never wrote
   (killed before it saved: ~0.9 s to `hi` in both modes, as Wine then
   skips work). Measured like for like (cold: no host or wineserver left
   from before; the same complete prefix; to `wineboot`'s own end and to
   `hi`), before this round: one VM 5.65-5.70 s and 2.01-2.15 s, 1:1
   5.45-5.49 s and 1.94-2.08 s. Sampling the host found two costs of the
   runtime's, both modes': `signal_ready` walking 64 dispositions on
   every blocking call (its busiest function, 426 samples in 2 s against
   897 in the guest), and an unmap's check of every shared window against
   every region (194). Both are gone, and a swap every blocking call made
   on a line all of a process's threads read is now a load. After: one VM
   5.59-5.63 s and 1.89-1.90 s, 1:1 5.40-5.45 s and 1.78-1.88 s. One VM
   is still 2-4% behind; nothing of the runtime's own shows above 1% in
   the samples (the guest and Wine's own file system calls are the rest),
   and 36 CPUs don't change it. Where the rest goes (one run queue and 10
   ms slices for every process's threads, TLB reach with 18 vCPUs
   switching between processes) is for the scheduler, `mn-threads.md`
   step 5. A warm one-VM `wine` needs the host to outlive it for the next
   `macwl-run` to join: clients, step 5.
   Choices taken by the rule (2026-11-02, second round):
   - **Every host wait sleeps on the thread's waker pipe too**, rather
     than re-sending interrupts until the thread is seen to have gone (or
     a per-call signal mask, which no blocking `read` has): it closes the
     race for every wait with something to poll, at one `poll` on the way
     into a read that would block.
   - **A timer reaches its process's threads through their handles**, not
     by kicking the host's vCPUs and filtering afterwards.
   - **CPU time is counted where it's spent**: each vCPU run timed as user
     time (runs in progress included, so a spinning thread's clock moves),
     system time from the host thread's CPU time less those runs, taken
     every millisecond as the vCPU stops, rather than once per task's turn
     (which left a thread that never yields at zero).
   - **`cmdline` and `environ` are read from the process's memory**, as
     Linux does (a program that rewrites its `argv` shows the change),
     rather than from copies kept at `execve`; other processes' state is
     reached through `Kernel::with_process`, by the same code as
     `/proc/self`.
   - **pidfds are host fds that become ready**: a socket pair whose far end
     the table drops as the process ends (one VM), a kqueue watching the
     Mac process's exit (1:1), so `poll` and `epoll` work on them
     unchanged.
   Left for later steps: the loader's own mappings of a program aren't
   named in `maps`; per-thread signal masks show as 0 in `status`; `htop`
   (it needs a terminal: step 4); CLONE_FILES, CLONE_FS and CLONE_SIGHAND
   without vfork; the calls above that still wait with only the interrupt.

   3a. **Blocked tasks wait in kqueues** (decided), so a host's blocked
   threads cost no host thread each and `kern.num_taskthreads` no longer
   caps them. A call that would block on readiness (`read`, `write`,
   `recv*`, `send*`, `accept`, `connect`, `ppoll`, `pselect6`,
   `epoll_wait`, eventfd/timerfd/signalfd/inotify reads) parks its task with
   a one-shot registration on a host-wide kqueue (`EVFILT_READ`/`WRITE` on
   its host fds, `EVFILT_TIMER` for its timeout, `EVFILT_USER` for a signal
   or the process ending); a few waiter threads run `kevent` and make
   parked tasks runnable, and the call is made again on a CPU, where it
   now won't block (or times out, or returns EINTR). Futex waits, `wait4`,
   `waitid`, `rt_sigsuspend`, `rt_sigtimedwait` and sleeps are in the host's
   memory once step 3 has the process table: they park with no kqueue at
   all, woken by the wake, the child's exit, the signal, or a timer.
   Workers remain only for calls with nothing to wait on (`flock`,
   `F_SETLKW`, `openat` of a FIFO, file I/O on a slow disk), bounded by a
   pool.

   **Done, 2026-11-02.** Blocked threads park in kqueues or in memory, as
   above; workers remain for the rest, bounded. With it:

   - **A host thread budget and a watchdog** (`linux_abi::host_threads`).
     Every thread the runtime starts takes one of a budget of 1024
     (`MACWL_MAX_HOST_THREADS` lowers it, to 32 at least); workers get only
     what's left after the host's own threads (CPUs, waiters, ticker, a few
     singletons), and a call whose worker can't start waits for one. A
     watchdog counts the process's real threads (`proc_pidinfo`) at every
     thread start and from the ticker, and ends the process over a ceiling
     of the budget and half again (1536 at most). Host threads stay in the
     hundreds at most, whatever the guest does: tests never grow them
     further.
   - **Timers share one host thread**: POSIX and interval timers are
     watched by a timer service per process (a deadline heap), not a thread
     each.
   - **Real-time signals queue** (each one sent is delivered), and a timer
     whose signal is pending takes later expiries as its overrun, as on
     Linux. (200 timers on SIGRTMIN starved each other before.)
   - **`flock` and `F_SETLKW` are restartable**: interrupted to take a CPU
     back, they're made again on a worker, not EINTR to the guest.

   Measured (M5 Pro, `guest/wakebench.c`, p50 round trip, before → after,
   1:1; one VM the same within noise): pipes between processes 12.4 → 11.2
   µs, between threads 12.8 → 13.7, Unix sockets 11.2 → 11.1, futex 11.7
   → 12.5, eventfd 34.4 → 12.1, and a 50 µs `nanosleep` 1143 → 56 µs (it
   slept on a coarser timer). 300 threads blocked in `read` and futex waits
   (`guest/blockmany.c`) take 22–23 host threads in either mode.

   Tests: `blockmany` with 300 threads and `guest/hostbudget.c` (100
   threads waiting in `flock`, 200 timers) under a budget of 32, so the
   watchdog would end them at 48 host threads; host_threads and signal
   queue unit tests. Stress: 520 runs of threads, manythreads, procstress,
   blockmany and hostbudget, 4 at a time, both modes, at the default CPUs
   and 2: no failures, no hangs. Runtime 35 + 34 in 1:1 and one VM,
   linux-abi 67; arm64 Wine boots and runs `cmd.exe` in both modes.
4. **Terminals and job control.** Ptys' sessions and foreground groups,
   keyboard signals on master writes, SIGTTIN/SIGTTOU, SIGHUP on hangup,
   stops parked; the relay for a host started in a Mac terminal. Tests: bash
   job control in `signals` under a pty; foot through the headless
   compositor (`MACWL_TYPE`): Ctrl-C, Ctrl-Z, `jobs`, `bg`, `fg`, vim.

   **Done, 2026-11-02.** In one VM the host's ptys are no Mac process's
   controlling terminal (host opens get O_NOCTTY); the table keeps each
   pty's session and foreground group, with Linux's rules
   (`drivers/tty/tty_jobctrl.c`), behind the `Kernel`'s terminal calls:

   - **Controlling terminals**: a session leader opening a slave without
     O_NOCTTY, or `TIOCSCTTY`, takes it; fork inherits it, `setsid` drops
     it; `TIOCNOTTY` (a leader's foreground group then gets SIGHUP and
     SIGCONT); `/dev/tty` opens it, or ENXIO; `TIOCGSID`;
     `/proc/<pid>/stat`'s `tty_nr` and `tpgid`.
   - **`tcsetpgrp`/`tcgetpgrp`**: the session's own terminal, groups of the
     session (a background caller's `tcsetpgrp` is a change: SIGTTOU unless
     blocked, bash's case).
   - **Keyboard signals**: a write to a master is typed input. With ISIG
     each VINTR, VQUIT and VSUSP (after ISTRIP; VLNEXT quoting the next
     byte in canonical mode with IEXTEN, as `n_tty`) is written, then its
     signal goes to the foreground group, so a Ctrl-C after typed text
     comes after it; the Mac driver echoes and flushes as before. The line
     discipline's rules are unit tests (`tty.rs`).
   - **SIGTTIN/SIGTTOU**: a background read, a TOSTOP write, or a change
     (`tcsetattr`, `tcflush`, `tcflow`, `tcsendbreak`, `tcsetpgrp`) of the
     controlling terminal sends the signal to the caller's group and the
     call is made again once that's been dealt with (Linux's ERESTARTSYS);
     EIO for a read with SIGTTIN blocked or ignored, or from an orphaned
     group; a write or change with SIGTTOU blocked or ignored goes ahead.
   - **Hangups**: a master's last close gives the session leader SIGHUP
     and SIGCONT (and the session loses its terminal); a session leader
     ending gives its foreground group SIGHUP; a group orphaned with a
     stopped process in it gets SIGHUP and SIGCONT, and SIGTSTP, SIGTTIN
     and SIGTTOU do nothing to an orphaned group.
   - **SIGWINCH** to the foreground group when `TIOCSWINSZ` changes the
     size. A new pty starts at 0x0, as on Linux (macOS kept the size the
     last pty of its number had, so a first resize could be no change).
   - **Stops park** the process's tasks (step 3), so a stopped job holds no
     CPU.
   - **The relay** (`crates/runtime/src/relay.rs`): a `macwl-run` started
     with a terminal as stdin gives its first process a pty of the host's
     in place of that terminal (its fds 0-2 that were it, one open file),
     with the terminal's settings and size, as its controlling terminal;
     the Mac terminal goes raw and two host threads join it to the
     master, input through the keyboard rules above. When the first
     process stops, the terminal gets its settings back and the host stops
     with SIGSTOP, so the Mac shell sees its job stop; continued (`fg`,
     `bg`), the terminal is raw again and the first process's group gets
     SIGCONT. The Mac terminal's SIGWINCH and SIGHUP are the pty's resize
     and hangup; its going away (EOF) hangs the pty up. The first
     process's group isn't orphaned: the Mac shell stands as its parent in
     the session (`Entry::mac_parent`). Only `macwl-run` turns it on (a
     test harness running hosts in-process doesn't).

   Found on the way, fixed in both modes:
   - A call a signal interrupted with no handler run for it (a stop, then
     SIGCONT) is made again, as Linux restarts ERESTARTSYS: `cat` stopped
     by Ctrl-Z and continued saw EINTR and exited.
   - Generating SIGCONT discards pending stop signals, and a stop signal a
     pending SIGCONT, as Linux does.
   - `kill`, `tkill` and `tgkill` read their pids as ints: glibc's
     `kill(-pgid)` leaves the register's upper half zero, which read as a
     huge pid (bash's `fg` got ESRCH in one VM).
   - 1:1: since step 3a a read waited for input before it was made, so a
     background process's terminal read was never stopped by the Mac
     driver until something was typed; a background read is now made at
     once.

   Choices the design left open, taken by the rule (2026-11-02):
   - **Linux's rules throughout, not the Mac driver's**, including the
     orphaned-group rules (EIO, discarded stops, SIGHUP and SIGCONT to an
     orphaned group with a stopped member), ERESTARTSYS restarts after
     stops, and VLNEXT state kept across writes per pty, rather than
     only the common cases.
   - **Hangup as Linux does it**: the master's close signals the session
     leader (SIGHUP, SIGCONT), and the leader's end the foreground group,
     rather than signalling the foreground group directly; a shell then
     hangs its jobs up itself, as on Linux.
   - **Signals between the bytes**: a write to a master is split at each
     signal character, and the signal sent once its byte is in, rather
     than one signal pass after the whole write.
   - **The relay stops the whole host** when the first process stops, so
     the Mac shell's job control works on `macwl-run` as on any program;
     step 5's clients stop only themselves.

   Tests (`crates/runtime/guest/tty.c`, both modes):
   `terminals_and_job_control` (the rules process by process, with
   `one_vm_` copy), `bash_job_control` (nixpkgs arm64 bash typed at
   through the master: `cat &` stopped by SIGTTIN and `fg`, Ctrl-Z,
   `jobs`, `bg`, Ctrl-C ending only the foreground job, a hangup ending
   bash by SIGHUP and its jobs), `one_vm_relays_its_terminal` (size,
   Ctrl-Z stopping `macwl-run` with the terminal's settings back, SIGCONT,
   Ctrl-C, hangup); 4 line-discipline unit tests. In 1:1 two checks are
   skipped: the Mac driver doesn't count a process forked after its
   session took the terminal (a spare launched ahead) as having it, so a
   resize's SIGWINCH and a non-leader's `TIOCNOTTY` skip it there. foot
   (nixpkgs) through the headless compositor, typed with `MACWL_TYPE`,
   running bash: `sleep 100` Ctrl-Z, `jobs` (Stopped), `bg`, `jobs`
   (Running), `fg`, Ctrl-C (`$?` 130), `cat &` (Stopped), `fg`, typed
   to, Ctrl-D, vim (`-u NONE`) Ctrl-Z, `jobs`, `fg`, `:wq` (saved): the
   same in one VM and 1:1. Stress: the tty tests 30-80 runs 4-8 at a time
   per mode, no failures; runtime 35 unit + 39 integration in both modes,
   10 suite runs. Wine boots and runs `cmd.exe` in both modes (and in one
   VM from a terminal, through the relay). `wakebench` p50s unchanged
   (within 0.5 µs either way, both modes).
5. **Clients.** `--serve`, `MACWL_HOST`, the client protocol, exit status,
   forwarded signals, the relay, hangup when a client dies; `session.sh`
   starts one host. Tests: a client's exit code and signal status; `kill
   -TERM`/`-INT` of a client from the Mac reaching its Linux process; a
   client killed with SIGKILL hanging its process up; two clients in one
   host seeing each other's processes in `/proc`; `macwl-run --ps`; the
   host exiting 2 s after its last process and client go, and staying for
   a daemon.

   **Done, 2026-11-02.** `macwl-run --serve PATH` (`host/serve.rs`) is a
   host with no program of its own: its VM is made with nothing in it, and
   one host thread serves every client (accepts them, starts their
   processes, types their terminals' input, routes their signals, tells
   them of stops, continues and ends). A client (`client.rs`; `MACWL_HOST`
   or `--host PATH`, `--new-host` to be a host anyway) never makes a VM.
   The protocol (`proto.rs`): framed `linux_abi::wire` messages over the
   socket, fds alongside; what to run is a `Launch` (`launch.rs`, also
   what a `macwl-run` that is the host builds now). A client's process is
   init's child, leading a group and session of its own, with the client
   standing as its Mac parent (its group isn't orphaned). `macwl-run --ps`
   prints pid, parent, group, session, tty, state, CPU time, RSS, the
   client's Mac pid and the command line (from the process's memory).
   `scripts/session.sh` with `MACWL_ONE_VM=1` starts one host and exports
   `MACWL_HOST`; dbus-daemon and foot join it, and bash and everything it
   starts are foot's children there (`MACWL_SESSION_COMPOSITOR=headless`
   runs it through the headless compositor, for tests).

   Choices the design left open, taken by the rule (2026-11-02):
   - **Fds are passed, not relayed**: a client's fds 0–2 go to the host
     (`SCM_RIGHTS`) and are the process's own, so a pipe or file is used
     with no copy and no added latency, and the process keeps it after its
     client has gone (a killed client's process still writes to the
     client's stdout). With a terminal, the pty's master is passed back to
     the client, which copies its output to the Mac terminal directly; only
     typed input goes through the host, which must see each byte for the
     keyboard's signals. The client, not the host, touches the Mac terminal,
     so the Mac driver's job control applies to it as to any program (a
     background client reading the terminal gets SIGTTIN).
   - **No host thread per client**: one service thread polls every client
     (up to 256 at once; more are turned away), and a client's own relay
     threads (3) are in the client.
   - **The socket is the user's alone**: a directory the host makes for it
     is mode 700, the socket mode 600, the peer's uid checked with
     `getpeereid` on both ends (a client won't talk to another user's
     host), and a lock file (`PATH.lock`) makes one host per path.
   - **A client with no host starts one**: a plain `posix_spawn` (no
     `SETEXEC`) of `macwl-run --serve PATH --idle-exit` in a session of its
     own, its output to `PATH.log`, from a process that has never made a VM
     (asserted). Two clients racing to start one: the second host finds the
     lock taken and exits (status 3), and both clients join the first. A
     host that goes as a client connects is tried again.
   - **The exit status is raised**, as a shell expects, rather than 128 + the
     signal: the Mac shell then says "terminated" or "interrupt" as for any
     job. Core-dumping signals stay a status (raising them would make crash
     reports for the client).
   - **Signals go to the process's group**, as a terminal sends them to its
     foreground job (the client is that job in the Mac shell), and the
     signals the client was started ignoring or blocking (`nohup`, a
     script's `&`) start out ignored or blocked in its process, as across an
     `execve`.
   - **Lifetime**: 2 s after the last process and client (`--ps` queries
     don't count); a `--serve` host waits for its first client, one a
     client started (`--idle-exit`) doesn't. As it goes, it removes the
     socket and lets the lock go before ending, so a new host can take the
     path at once. Socket paths are limited to 103 bytes (macOS's
     `sun_path`).

   Measured (M5 Pro): a client of a warm host starts and ends a static
   program in 4.4 ms and coreutils' `true` in 5.5 ms, against 5.2 and 5.8
   ms for a 1:1 `macwl-run` and 5.2 and 6.0 for a one-VM one (most of it
   macOS starting the `macwl-run` binary). `wakebench` p50s unchanged in
   both modes (pipes between processes 11.1–12.6 µs, threads 13.5–13.9,
   futex 12.4–12.5, a 50 µs sleep 55.5–56.9), and the same in a client's
   process. arm64 Wine as a client of a one-VM host: `wineboot -i` exits
   0 with 801 files in system32 in 6.4 s (1:1: 6.3 s; a one-VM `macwl-run`
   of its own, which waits for wineserver and services to go: 14.0 s), and
   `wine cmd.exe /c 'echo hi & ver'` then prints both in 0.65 s (1:1:
   0.61 s); the host goes about 9 s later, once wineserver has.

   Tests (`guest/client.c`; all hosts started by the tests):
   `client_exit_status` (codes, SIGTERM and SIGKILL raised, SIGSEGV as
   139, fds passed, a missing program, a host started by a client and gone
   once idle, its directory mode 700), `client_forwards_mac_signals` (`kill
   -TERM`, `-INT`, `-HUP`, `-USR1` of the client reach the process),
   `client_killed_hangs_its_process_up`, `clients_share_a_host` (procps
   `ps` in one client lists the other's process; `macwl-run --ps`),
   `host_lifetime` (waits for a first client; goes 2 s after the last; a
   daemon keeps it until 2 s after it ends), `client_relays_its_terminal`
   (size, Ctrl-Z stopping the client with the terminal's settings back,
   SIGCONT, Ctrl-C ending the client by SIGINT, a resize, a hangup),
   `session_shares_one_host` (`session.sh` headless: procps `ps` typed into
   foot's bash lists dbus-daemon, foot, bash and itself; `--ps` shows the
   two clients; everything gone once bash exits); `Launch`'s wire format.
   Runtime 36 unit + 46 integration in both modes, linux-abi 71.
   Seen once in about ten one-VM suite runs, under the full suite's load:
   foot in `session_shares_one_host` aborted with glibc's "The futex
   facility returned an unexpected error code" (a futex call returning an
   error glibc treats as impossible). Not reproduced alone or in seven
   further suite runs, with every futex error logged; still to find.
6. **Mac programs as stand-ins.** Spawn first, the reaper, signals, tty
   agents, Mac groups following Linux ones. Tests: item 13's list
   (scripted where it was by hand: Mac `uname`, `false`, `env`, mixed
   pipes, exit codes and signals through `wait`, fds by number, cwd, Ctrl-Z
   on Mac `sleep`, a `sleep | tr` pipeline and `vim` in foot, `bg`/`fg`,
   Ctrl-C, `git log` through `less`, 20 `posix_spawn`s in a row, a Mac
   binary that fails to exec now failing `execve` with its errno). Whether
   a tty agent is needed, or `posix_spawn` can give a Mac program its
   controlling terminal itself (`POSIX_SPAWN_SETSID` and opening the slave
   first in the child's file actions), is settled here by trying both
   against that list (still open, see Open questions). Then
   **one VM becomes the default**; `MACWL_ONE_VM=0` keeps 1:1 for a release.

   **Done, 2026-11-02**, as section 7 says ("As built"), with tty agents,
   chosen by the experiment recorded there. **One VM is the default**:
   every test passes in both modes. Mac programs are still opt-in
   (`--mac-programs`, `MACWL_MAC_PROGRAMS=1`, read by the host), and run
   only in one VM; `scripts/session.sh` turns them on for its host, and
   leaves them off with `MACWL_ONE_VM=0`.

   Choices the design left open, taken by the rule (2026-11-02):
   - **Agents for every Mac program**, a terminal's or one without a
     terminal, rather than the host spawning those of sessions without
     one: the host never touches its own signal dispositions for a spawn
     (ignored signals are set in the agent, which has one thread), every
     Mac program has a parent that reaps only its own children, and a Mac
     program of a session without a terminal has none (its `/dev/tty`
     fails, as on Linux), whatever terminal the host has.
   - **Mac groups follow Linux groups for signals too**: a signal to a
     Linux group reaches its Mac programs' Mac groups, so the Mac
     processes those started (`less` under `git`) get it, as members of
     the job.
   - **The foreground follows before `tcsetpgrp` returns**, and a new Mac
     group in the foreground is put there before its program runs
     (started suspended), so a Mac `vim` never meets a terminal whose
     foreground it isn't.
   - **Stand-ins in `/proc` and `ps` with what macOS knows**: the Mac
     program's CPU time and resident memory, its command line.

   Tests (all with `MACWL_ONE_VM=1` and Mac programs on, whatever the
   suite runs with): `one_vm_mac_programs` (item 13's list scripted in
   `tests/mac_programs.sh`: Mac `uname`, `sw_vers`, `false`, `env`; pipes
   both ways and Mac to Mac; exit codes; TERM, KILL, USR1 and USR2
   through `wait` in Linux numbers; fds by guest number, redirects, cwd; a
   Mac binary that can't run failing `execve` (126); SIGSTOP, SIGCONT,
   SIGTSTP and `bg`, SIGINT to a Mac job; the stand-in in `/proc` and
   procps' `ps`; `git`; `pbcopy`/`pbpaste`, the user's pasteboard put
   back; 50 Mac programs in a row), `one_vm_spawns_mac_programs` (musl
   `posix_spawn` of Mac programs 20 times, through a pipe, with exit
   statuses; a spawn of a Mach-O macOS won't run fails with ENOEXEC),
   `one_vm_mac_programs_on_a_terminal` (interactive Linux bash relayed
   from a pty: a Mac program's `/dev/tty`; Ctrl-Z, `jobs`, `bg`, `fg`,
   Ctrl-C on Mac `sleep`; a `/bin/sleep | tr` pipeline stopped and
   continued; a background Mac `cat` stopped by SIGTTIN; Mac `vim`
   suspended, redrawn after `fg`, saving; `git log` through Mac `less`,
   quit with `q`), `session_runs_mac_programs` (foot in `session.sh`
   through the headless compositor, typed with `MACWL_TYPE`: Ctrl-Z on
   Mac `/bin/sleep`, `jobs` Stopped, `bg` Running, `fg`, Ctrl-C (130); a
   `/bin/sleep | tr` pipeline stopped; Mac `vim` stopped, `fg`, saved),
   `stand_ins_in_the_hosts_listing` (`macwl-run --ps` lists a stand-in
   with its Mac pid; a client whose process exec'd a Mac program exits
   with its status), `one_vm_refuses_mac_programs_when_off`; unit tests of
   the spawn check, Darwin statuses in Linux numbers and spawn errnos.
   Runtime 41 unit + 51 integration, the default (one VM) and
   `MACWL_ONE_VM=0`; linux-abi 71. The two `session.sh` tests run one at
   a time (each builds and signs the release binaries).

   Measured (M5 Pro): Mac `/usr/bin/true` from Linux bash, 100 in a row:
   0.85–1.0 ms each (the Mac's own bash: 1.0 ms; Linux coreutils' `true`
   in one VM: 0.73–0.88 ms). The first Mac program of a host costs about
   3.5 ms more (11.2 ms for a whole `macwl-run bash -c /usr/bin/true`
   against 7.6 ms with a Linux `true`): its agent's launch. (1:1's Mac
   programs weren't measured again: they run only in one VM now.)
   `wakebench` p50s, before and after, 1:1 then one VM: pipes between
   processes 11.1–13.7 → 11.1–11.3 and 12.7–13.9 → 13.7–14.1 µs
   (interleaved runs of both builds: 12.7–15.0 against 13.7–16.7, noise),
   threads 13.8–14.2 → 13.7 and 13.8–14.0 → 13.8–15.0, futex 12.5–12.7 →
   12.5–12.6 and 12.5–12.9 → 12.5–12.6, a 50 µs sleep 56.2–57.3 → 56.2–56.9
   either way. arm64 Wine: `wineboot -i` exits 0 with 801 files in
   system32, and `wine cmd.exe /c 'echo hi & ver'` prints both, in one VM
   with Mac programs on and in 1:1 with them off.
   Seen while the two `session.sh` tests ran at once (before they were
   made to take turns): foot once aborted with glibc's "The futex facility
   returned an unexpected error code" with no `futex returned` line from
   the runtime, and once printed "failed to read timeout timer:
   Interrupted system call". The futex report now covers a wait returning
   more than 0 as well; neither was seen in 20 later runs of the session
   tests. And in 1:1, Wine's processes left after `wine cmd.exe` printed
   "corrupted double-linked list" (the build before step 6 does too).
7. **Shared windows and futexes across processes.** Windows deduplicated by
   object and range, private file windows shared, non-private futexes keyed
   by IPA (ROS target, item 5). Measured: Firefox's start and memory with
   and without; Fast DDS's shared-memory transport.

   **Done, 2026-11-03.**
   - **Windows by object and range** (`shared.rs`). A window knows its
     object: a regular file by device and inode, a memfd or `/dev/shm`
     object by an id in its header (random, made with the object; macOS's
     `fstat` gives shm objects no inode, and `fstat` in the guest now shows
     the id as the inode), shared anonymous memory by a number of its own.
     A mapping of a range that a window of the same object and access
     already shows is that window, one more user; a fork's windows are
     shared as before. So `MAP_SHARED` of a file, POSIX shm, a memfd or a
     `wl_shm` pool by several processes is one host mapping and one place
     in guest physical memory, and private read-only file mappings
     (libraries, fonts) likewise share one read-only window per file;
     a private page's first write still makes it the process's own.
   - **One futex table for the host process** (`linux-abi`'s `futex.rs`),
     hashed into 256 buckets with a lock each, as Linux's: a private futex
     is keyed by its process and address, a non-private one in a
     `MAP_SHARED` region by the window's object and the offset in it
     (`GuestMemory::shared_futex_key`), the same in every process mapping
     it. Waits, wakes, bitsets and requeues work across processes; a
     thread's exit wakes its CLONE_CHILD_CLEARTID word as a shared futex,
     as Linux does; FUTEX_CMP_REQUEUE now compares with both queues
     locked (it compared before taking the lock).
   - **Robust lists**: `set_robust_list` (and `get_robust_list`) per
     thread, walked as Linux's `exit_robust_list` does as each thread
     ends, at the process's end for the rest (a SIGKILL included), and at
     `execve`: each lock the thread still holds gets FUTEX_OWNER_DIED and a
     waiter is woken (EOWNERDEAD for the next locker). Atomic updates of
     guest words go through `GuestMemory::update_u32`, a compare-and-swap
     on the host's view of the page.
   - **PI futexes**: FUTEX_LOCK_PI, LOCK_PI2, TRYLOCK_PI and UNLOCK_PI:
     the owner's tid in the word, FUTEX_WAITERS when others wait, the lock
     handed to the first waiter on unlock, EDEADLK and EPERM as Linux;
     restarted after a signal handler, never EINTR (Linux's
     ERESTARTNOINTR). A robust PI lock whose owner dies goes to its first
     waiter marked FUTEX_OWNER_DIED, whether or not the list marks it PI
     (musl doesn't; Linux hands PI locks on regardless). No priority is
     inherited: threads have no priorities yet (`mn-threads.md` step 5).
   - **FUTEX_WAKE_OP, `futex_waitv`** (up to 128 futexes, Proton's fsync
     waits on it) **and futex2's `futex_wait`, `futex_wake`,
     `futex_requeue`**. FUTEX_WAKE for 0 wakes one, as Linux; the
     CLOCK_REALTIME flag on an op that takes no clock is ENOSYS.
     FUTEX_WAIT_REQUEUE_PI and CMP_REQUEUE_PI stay unimplemented: no C
     library has used them since glibc 2.25's condition variables.
   - **1:1** (`MACWL_ONE_VM=0`): processes are Mac processes with VMs of
     their own, so windows aren't shared between them and a wake can't
     reach another's waiters. A wait on a shared futex there looks at its
     word now and then (every 1 ms, doubling to 32 ms) and ends as a
     spurious wake when it has changed; a PI lock's wait tries to take the
     lock. Process-shared mutexes, condition variables, semaphores,
     barriers, PI locks and robust locks whose holder exits work, slowly
     (a cross-process futex round trip 1.03 ms). Not in 1:1: wake and
     requeue counts across processes (FUTEX_CMP_REQUEUE, WAKE_OP and
     `futex_waitv` woken from another process), and a robust lock whose
     holder is killed with SIGKILL (macOS ends that Mac process outright,
     and nothing walks its list).

   Choices the design left open, taken by the rule (2026-11-03):
   - **Shared futexes keyed by object and offset, not IPA**: the same
     futex even where one object has two windows (a mapping of a range no
     window shows whole), and in 1:1, where each process has its own IPAs.
   - **A window serves any range it contains**, not only the same range:
     a library's segments, mapped one by one inside the range the loader
     first reserved, are its one window.
   - **One hashed table for every futex of the host process**, private and
     shared alike, as Linux has, rather than a table per process plus one
     for shared memory: a requeue between any two keys, and a waiter
     leaving its queue, need no knowledge of whose queue it is.
   - **PI futexes, WAKE_OP, `futex_waitv` and the futex2 calls**, all
     implemented; robust lists walked at every way a thread can end.
   - **1:1 polls** rather than leaving cross-process futexes unsupported
     there, until 1:1 goes (step 8).

   Measured (M5 Pro):
   - **A file mapped by many processes**, as a library is (`guest/shmfutex
     library`, each process mapping it privately and reading every page):
     20 processes, 64 MiB: host mappings of the file 20 → 1, host RSS 1.33
     GB → 83 MB (1:1: 22 Mac processes, 1.49 GB); 100 processes, 128 MiB:
     100 → 1 mappings, RSS 13.2 GB → 205 MB, all mapped and read in
     0.26–0.27 s → 0.06–0.07 s (the VM's stage 2 faults once, not once per
     process). Physical footprint the same (68–74 MB): clean file pages
     aren't counted in it, but they were mapped and faulted per process.
   - **Latency** (`wakebench`, p50/p99 µs): a futex round trip between
     processes in shared memory (`futex-proc`) 12.8/18.5, the same as
     between threads (12.7/17.5); a process-shared mutex handed to a
     process blocked in `pthread_mutex_lock` (`mutex-proc`) 16.6/25.3.
     Before: neither ever woke (the wake went to the wrong table). 1:1:
     1034/1089 and 797/810 (polling). The rest unchanged: pipes between
     processes 13.6 (14.2 before), threads 13.6 (13.7), futex 12.7 (12.3),
     a 50 µs sleep 56.2 (56.3); 1:1 12.3, 13.5, 12.7, 56.2 (11.1, 13.6,
     12.5, 57.6). `forkbench` fork, interleaved with the old build: 0 MB
     61–68 µs (66–75), 200 MB 198–204 µs (188–209).
   - **Firefox** 158 (nixpkgs aarch64-linux, `--headless --screenshot` of
     a local page, content sandbox off): 10 processes, 4 of them exec'd
     Firefox; the screenshot in 0.6–0.7 s with and without, host RSS at its
     peak 418–448 MB against 487–505 MB before (four runs each), 26–28
     host threads. In 1:1 0.8 s. With either build the one-VM host stays
     after the screenshot until killed (1:1 exits): still to look at. One
     run of the new build, the first after building, ended early with
     Hypervisor.framework reporting an unknown exit (reason 3) in the
     parent; not seen again in the next eight.
   - **Fast DDS** isn't run: nixpkgs' `fastdds` 3.6.2 is marked broken,
     so it isn't in the binary cache, and nothing here builds aarch64-linux
     packages. Its shared-memory transport keeps process-shared mutexes
     and condition variables in `/dev/shm`, which `process_shared_sync`
     covers.

   arm64 Wine in both modes: `wineboot -i` exits 0 with 801 files in
   system32, `wine cmd.exe /c 'echo hi & ver'` prints both, and 30 s of
   leftovers print nothing of corruption.

   Tests (`guest/shmfutex.c`, both modes unless said): `process_shared_sync`
   (a process-shared mutex, condition variable, semaphore and barrier
   among four forked processes, and between a process and a program
   exec'd afresh, through `shm_open` and through a memfd passed over a
   Unix socket), `process_shared_sync_between_clients` (the same between
   two clients of one host), `robust_mutexes` (plain and PI robust
   mutexes whose holder exits, with the next locker waiting or coming
   later: EOWNERDEAD; one VM also with the holder killed by SIGKILL),
   `pi_mutexes`, `futex_ops_across_processes` (CMP_REQUEUE with its
   counts and a stale value, WAKE_OP, `futex_waitv`; one VM),
   `shared_windows` (a file, POSIX shm, a memfd and shared anonymous
   memory: writes seen both ways, one side unmapping, a private
   mapping's first write its own while unwritten pages follow the file),
   `one_window_for_a_library_in_many_processes` (30 processes, as many
   host mappings as one; one VM); unit tests
   `one_window_per_object_and_access` and `pi_robust_wake_op_and_waitv`.
   Runtime 42 unit + 59 integration in both modes, linux-abi 73.
8. **1:1 mode goes.** A host for one program (`--new-host`, the default for
   a `macwl-run` without `MACWL_HOST`) is the isolated mode, so `fork.rs`'s
   spares, Mach memory entries and control sockets, and `exec_host`'s
   `SETEXEC` path, are deleted.
   (Done, 2026-11-10.) `MACWL_ONE_VM` is gone, and `macwl_runtime::run`
   is always `host::run`; `Host::table` is the table, no longer an
   `Option`, so every "with many processes" branch of the CPU loop is the
   only one (CPU time per turn, ASID flushes between tasks, a stopped
   process parking, a runtime error ending its process alone). Deleted
   with 1:1: `execute` (a VM per process), `Next::ExecHost` and the
   host's `exec_host`; `fork.rs` (`macwl-run --fork-child`, spare
   children) and `mach.rs` (memory entries), the pool's and address
   space's snapshots and their encoding, shared windows' encoding (and the
   fd each window kept for a child: the mapping holds what it maps);
   in `linux-abi`, `Action::ExecHost`, `MacPrograms::ExecHost` and
   `exec_host`, the control sockets (`ctl`, `Children`, `tell_parent`,
   `setpgid` by proxy), `Process::encode_for_fork`/`decode_forked`, the
   fd table's encoding (and the eventfd, timerfd and inotify copies it
   made), and the waits for host children. The waiters' host-signal
   wakeup (1:1's `host_signals_for`) went too: a host's signals reach its
   processes through the table (`route_host_signals`). `HostKernel` stays
   as the kernel of a process on its own, for `linux-abi`'s tests: no
   children (`wait4`, `waitid`: ECHILD), the host's own calls for the
   rest, Mac programs refused. `Proc::space` is always there. Tests: the
   1:1-only cases and modes went (`tgkill_reaches_another_process_thread`
   in one mode, the `one_vm_` duplicates of `terminals_and_job_control`
   and `bash_job_control`, `guest/tty.c`'s 1:1 branches);
   `fork_state_round_trips` became
   `a_fork_child_inherits_its_parents_state` and
   `inotify_goes_to_a_forked_child` forks in the host. `scripts/session.sh`
   always runs one host. In code, 2,273 lines deleted and 269 added.
   Runtime 39 unit + 57 integration, linux-abi 73.
9. **Federated hosts** (section 1). The coordinator (pid ranges moved into
   it, the directory of ranges and spanning groups), `--new-host` and the
   per-program setting, moving at `execve` with remote stand-ins, `wait`
   and SIGCHLD across hosts, signals by pid and group, groups and sessions
   spanning hosts, terminals and job control with remote members, orphans,
   a host's failure. Tests: bash in foot running a program set to its own
   host: `wait`, exit status and killed-by-signal status; Ctrl-C, Ctrl-Z,
   `bg`, `fg` with it in the foreground job alongside local processes (a
   pipeline across hosts); `kill -TERM -$pgid` reaching both hosts;
   `ps -ef` in either host listing both (`/proc` asks the coordinator for
   remote pids); a federated host killed with SIGKILL: its processes reaped
   as killed in the session; `macwl-run --ps --all`.
   Built in parts, each with its tests:
   - **9a, the coordinator** (done, 2026-11-10). `macwl-run --coordinator`
     (`coord.rs`) serves `$TMPDIR/macwl-$UID/coordinator`; every host has
     a link to it (a thread, `coord::Link`): linked while one runs, and
     waiting on a kqueue on the socket's directory while none does, so a
     coordinator started later finds every host linked within
     milliseconds, with no polling. A host starts one only when it needs
     one: a session that never federates has no coordinator process.
     `macwl-run --ps --all` (starting one) lists every host's processes,
     each host answering from its table (`Table::listing`, moved out of
     `--serve`'s service so that any host can); just started, the
     coordinator waits up to 0.5 s for the hosts the pid file says run.
     It goes 2 s after its last host. Pid ranges stay in the pid file,
     which the coordinator reads for the hosts it expects. Test
     `ps_all_lists_every_host`: two hosts, 23 ms for a listing that
     starts the coordinator.
   - **9b, moving at `execve`** (done, 2026-11-10; `host/fed.rs`). An
     `execve` of a program set to have its own host (`MACWL_NEW_HOST=1`
     in the environment it passes, or its path or name in
     `~/.config/macwl/hosts`), from a process not alone in its host,
     starts `macwl-run --adopted` with a socket to it as fd 3 and hands it
     the process: pid, parent, group, session and terminal, and what an
     exec keeps (`Process::encode_for_move`: filesystem view and cwd,
     dispositions reset as by an exec, the mask, rlimits, umask, `prctl`
     flags, argv and environment, minus `MACWL_NEW_HOST` so it doesn't
     move again), with its fds (`SCM_RIGHTS`; files, pipes, sockets and
     terminals; kqueue-backed kinds aren't passed). The new host loads
     the program and answers; a program it can't load fails the `execve`
     with the errno (bash: 126). The process then stands in for itself
     in the old host, as for a Mac program (`StandIn`, now either kind:
     `Elsewhere::Mac` or `Elsewhere::Host`; `Kind::Mac` became
     `Kind::StandIn`): its parent `wait`s for it there, `--ps` marks it
     with its host's Mac pid and `@`. **The socket stays as a direct link
     between the two hosts**, not the coordinator's to forward (a change
     from section 1: one hop, and the other host's death is the link's
     end): the new host reports stops, continues and the end
     (`Peer::changed`, from `child_changed` and `ended` when the parent
     is remote, `Entry::remote_parent`), the old one sends signals by pid
     or thread and the new parent of an orphan. The new host dying ends
     the process as killed by SIGKILL; the old one dying makes it its
     host's init's. A terminal master it took along stays held in the old
     host too. Moving costs ~3.8 ms an `execve` (`true` in a loop: 5.8 ms
     a run, against 2.0 ms). Tests `moved_to_a_host_of_its_own` (exit
     codes, 143 for SIGTERM by pid, 137 for SIGKILL from inside, `$PPID`,
     STOP and CONT seen by `jobs`, a refused load, the hosts file, its
     parent's host killed) and `a_moved_hosts_death_is_a_sigkill`.
   - **9c, groups and signals across hosts** (done, 2026-11-10). Linked
     hosts form a tree (a link is made only by a move, from the host
     moved from), so a signal for many is sent on along it, never back.
     Each host tells each of its links what it reaches (`Reach`: the
     groups, with their sessions, that have members there or beyond, and
     the pid ranges there or beyond), its own and through its other
     links, whenever that changes (a fork, an end, a reap, `setpgid`,
     `setsid`, a link coming or going; the coordinator plays no part).
     `Table::deliver_from` routes on it: a group's signal reaches its
     local members and goes on to the links that reach the group (a
     moved process's stand-in isn't signalled itself, its host being one
     of those), and `kill(-pgid)` is ESRCH only when no host has the
     group; `kill(-1)` goes on to every link; a pid or tid that isn't here
     is asked of the link that reaches its range (`ASK`, answered: the
     result, ESRCH included, is the remote host's). A moved process's
     `setpgid` or `setsid` in its new host tells the old one
     (`MEMBER`), whose stand-in follows; `setpgid` into a group whose
     members are all in another host of the session works. Not yet:
     orphaned groups counted across hosts, `getpgid`/`getsid` of another
     host's process. Test `signals_across_hosts` (`kill -TERM -$pgid` of
     a job half moved; from a moved process: its parent by pid, a pid no
     host has, `kill 0`, `kill -1`).
   - **9d, terminals and job control across hosts** (done, 2026-11-10).
     Keyboard signals needed nothing new: they're a group's signal (9c),
     which goes on to the hosts with members, the pty raising them passed
     along so Mac programs whose agent has it aren't signalled twice.
     A terminal's job control stays its host's: `Reach` carries each
     pty's session and foreground group from the host that has it, so a
     moved process's background check (SIGTTIN, SIGTTOU, EIO for an
     orphaned group), `tcgetpgrp` and `tcgetsid` answer from that, and
     `tcsetpgrp` from another host is asked of the terminal's
     (`ASK_FOREGROUND`, `Table::set_foreground`), which accepts a group
     of the session's in any host. The foreground changing is advertised
     at once, on the same links the job's SIGCONT then takes, so the new
     foreground is known there first. (This is the doc's cache with a
     generation, pushed rather than pulled.) Test
     `job_control_across_hosts`: bash on a terminal, a pipeline across
     hosts stopped with Ctrl-Z, `jobs`, `fg`, Ctrl-C; a moved `cat &`
     stopped by SIGTTIN, then `fg` and read; a moved interactive bash's
     own job control on the other host's terminal.
   - Next: 9e (a coordinator that restarts), 9f (`/proc` and `ps -ef`
     listing other hosts' processes; the session test in foot).

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
- The existing runtime and `linux-abi` tests, in both modes while 1:1
  existed (until step 8).

## Open questions

Decided (see "Decisions"): fault containment (per-call containment, and
federated hosts for apps that want their own), pids across hosts (unique,
by ranges), fairness (hierarchical), blocked tasks (kqueues, step 3a),
Activity Monitor (`/proc` and `macwl-run --ps`), the host's lifetime (while
it has processes or clients), Mac programs and their terminal (tty agents,
step 6). Still open:

- **Memory pressure**: macOS's memory pressure handling sees one large
  process; the host's own out-of-memory policy (step 3) decides which Linux
  process goes.
