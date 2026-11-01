# Threads on vCPUs: a scheduler

Status: design (2026-10-25). ROADMAP item 5; Firefox needs it (its main
process starts more than 64 threads, and the 65th gets no vCPU).

## The problem

Hypervisor.framework allows 64 vCPUs and one VM per process, and a vCPU can
only be run by the host thread that created it. Today each guest thread is
a host thread with a vCPU of its own (`run_thread` in
`crates/runtime/src/lib.rs`), so a process has at most 64 threads: the 65th
`clone` fails with EAGAIN. Firefox treats that as fatal (`DecodePool`'s
`MOZ_RELEASE_ASSERT`). Big apps have many threads, but nearly all of them
are blocked at any moment: in `futex`, `epoll_wait`, `poll`, `read`.

A second VM would mean a second host process sharing guest memory, every
fd, and TLB flushes: not worth it.

## The idea: the runtime is the CPU scheduler

Each process gets **one vCPU per physical core**: 18 on this M5 Pro (6
Super, 12 Performance; `hw.physicalcpu`). Each vCPU belongs to a **CPU**: a
host thread that owns it and runs guest threads on it, as a core runs
threads. Guest threads are **tasks**. The runtime schedules tasks onto CPUs
the way Linux schedules threads onto cores: run queues, scheduling classes,
priorities, time slices, preemption, a context switch when a task blocks.

The guest sees 18 CPUs (`sched_getaffinity`, `/proc/cpuinfo`,
`/sys/devices/system/cpu/online`, so `nproc` and thread pools size
themselves to them), and its priorities mean what they mean on Linux.

What a core does in hardware when a thread blocks (switch to another),
the runtime can't do inside a host system call: the host call is blocking
the CPU's host thread. So a task whose system call may block, when other
tasks are waiting for a CPU, hands the call to a **worker** (a host thread
without a vCPU), and the CPU switches to the next task. When the call
returns, the task is runnable again: a wakeup, as on Linux.

## Costs (measured, M5 Pro: `crates/runtime/examples/switch_cost.rs`)

| | |
|---|---|
| Context switch: save one task's registers from a vCPU, restore another's | 1.2 µs |
| Hand-off between host threads (condvar round trip) | 3.9 µs |
| Create and destroy a vCPU | 26.5 µs |

A blocking call moved to a worker and back costs about 6–8 µs. That's only
paid when it buys something: another task was waiting for the CPU. A
process whose runnable tasks fit its CPUs runs every call inline, as now,
however many threads it has. CPUs are made once per process (lazily, as
tasks need them), not per thread, which saves 26.5 µs on every thread
creation after the first few.

## Scheduling: as Linux does it

Linux's model, since it's what the guest's programs were written against
(and what their priority calls expect):

### Classes and priorities

In order: a runnable task of a higher class always runs before one of a
lower class.

1. **Real-time**: `SCHED_FIFO` and `SCHED_RR`, priority 1–99 (higher runs
   first). FIFO runs until it blocks or yields; RR gets 100 ms slices
   (`sched_rr_get_interval`) among equal priorities. Throttled as Linux
   does by default (`sched_rt_runtime_us` 950000 of 1000000): real-time
   tasks get at most 95% of each CPU per second, so a runaway one can't
   wedge the process.
2. **Fair**: `SCHED_OTHER` (and `SCHED_BATCH`), by nice value −20..19,
   with Linux's weight table (nice 0 = 1024, each step ~1.25×). EEVDF, as
   Linux 6.6+: each task has a virtual runtime advancing at 1024/weight of
   real time and a virtual deadline (`vruntime + slice × 1024/weight`); the
   CPU runs the eligible task (not ahead of the queue's average) with the
   earliest deadline. Base slice 3 ms. `SCHED_BATCH` doesn't preempt on
   wakeup.
3. **Idle**: `SCHED_IDLE`, weight 3: runs when nothing else wants the CPU.

A new thread inherits its creator's class and priority (unless
`SCHED_RESET_ON_FORK`), as on Linux.

### Run queues

One run queue per process, shared by its CPUs (a mutex: 18 CPUs don't need
Linux's per-CPU queues and load balancing): a real-time array of 99 FIFO
lists, and an EEVDF tree for the fair tasks (a `BTreeMap` keyed by
deadline, with the eligibility test). Each CPU takes the best task when
its current one blocks, yields, exits, or is preempted.

### Preemption

- **Time slice**: a ticker (every 1 ms while any CPU's queue has waiters)
  kicks (`hv_vcpus_exit`) a CPU whose task has used its slice (fair) or is
  outranked (RR at the same priority, a higher real-time task, a fair task
  with an earlier eligible deadline).
- **Wakeup preemption**: a task made runnable that outranks a running one
  (higher class or priority; for fair tasks, an earlier deadline, as
  EEVDF's `wakeup_preempt`) kicks that CPU at once, rather than waiting for
  the tick.
- A kick can land while the vCPU is in our EL1 vector, after the `svc` but
  before its `hvc` exits to the host; `UserContext::save` would then give
  EL0's PC after the `svc` (from ELR_EL1), and the call would be lost. So a
  task is only switched out when its vCPU stopped at EL0, or after its
  call was handled; stopped in the vector, it's resumed as is.

### Blocking

A task's system call runs **inline** (on its CPU, as now) unless all of:

1. another task is waiting for a CPU (the run queue isn't empty),
2. the call is in the **blocking set**: `futex` WAIT variants, `read`,
   `readv`, `pread64`, `write`, `writev`, `recv*`, `send*`, `accept`,
   `connect`, `epoll_pwait`/`epoll_pwait2`, `ppoll`, `pselect6`,
   `nanosleep`, `clock_nanosleep`, `wait4`, `waitid`, `rt_sigsuspend`,
   `rt_sigtimedwait`, `fcntl(F_SETLKW)`, `flock`, `openat` (a FIFO), and
   reads of eventfd/timerfd/signalfd/inotify fds,
3. and it would block, where that's cheap to tell: a zero-timeout `poll`
   of the fd for I/O, the futex word's value for `FUTEX_WAIT`, a zero
   timeout for `epoll_wait`/`ppoll`.

Then the task is **switched out**: its registers saved (0.57 µs), the call
given to a worker, the CPU takes the next task. When the call returns,
the worker makes the task runnable (with its result) through the same
wakeup path as anything else, wakeup preemption included.

**Already blocked inline when others arrive.** A task blocked inline holds
its CPU. If a task becomes runnable with no CPU free, the scheduler takes
a CPU back from the lowest-ranked task blocked inline in a blocking-set
call: `INTERRUPT` (SIGEMT) to that CPU's host thread, so the host call
returns EINTR, and then, instead of handing EINTR to the guest:

- restartable calls (`read`, `write`, `wait4`, ..., the `restartable()`
  set) and `rt_sigsuspend` are re-issued on a worker (nothing was
  consumed);
- `futex` waits return 0, a spurious wakeup, which callers handle by
  looking at their condition again. (First EINTR, which was wrong: Linux
  gives that only when a signal handler ran, and glibc's `sem_wait`
  passes it on to its caller. foot's render threads took it as work
  handed out and drew garbage, crashing in pixman.) Re-issuing them, as first planned,
  loses wakeups: the waiter has left the futex's queue, a wake sent then
  goes to no one, and if the word has gone back to the value waited for
  (a mutex unlocked and locked again), the re-issued wait sleeps through
  it. Found as a hang under load: every thread in `futex`, none queued;
- `epoll_wait`, `ppoll`, `pselect6`, `nanosleep`, `clock_nanosleep`,
  `rt_sigsuspend`, `rt_sigtimedwait` return EINTR, as when a signal
  arrives on Linux (`nanosleep` with its remaining time); callers loop.

Which of the two policies (move blocking calls to workers whenever tasks
are waiting, or keep blocking inline and take CPUs back only when needed)
costs less depends on the program; the first is simpler and comes first,
the second is a measured optimization.

### CPUs and the host's cores

A CPU's host thread is scheduled by macOS, which decides which core runs
it. Its QoS class follows the task it's running, set at each switch
(`pthread_set_qos_class_self_np`): real-time and negative-nice tasks
`USER_INTERACTIVE`, nice 0 `USER_INITIATED`, positive nice `UTILITY`,
`SCHED_IDLE` and nice 19 `BACKGROUND`. So a guest's priorities reach the
hardware: macOS keeps background work off the Super cores, and an idle
task runs on whatever's spare.

This is where macwayland can do better than a VM. Howard Oakley measured
VMs on Apple silicon ([Eclectic Light, 2022](https://eclecticlight.co/2022/07/18/virtualisation-on-apple-silicon-macs-4-core-allocation-in-vms/)):
each vCPU behaves as "a relatively high QoS thread on the host", guest
threads at the lowest and highest QoS got the same cores, none went to the
E cores, so "a guest will 'steal' P cores that could force host user
processes to be allocated to E instead of P cores". A VM's host can't do
better: the guest kernel schedules its threads, and the host sees only
vCPUs. Here the runtime is the guest's scheduler and knows, at each
context switch, the priority of what the vCPU is about to run.

He also found that asking for more vCPUs than physical cores grinds host
and guest alike ("from 1 to the total number of physical cores (n)", 2 to
n − 1 usually best): the cap at `hw.physicalcpu` is that. Unlike a VM's,
an idle CPU here costs nothing (its host thread waits on a condvar, no
guest idle loop), so a process doesn't need fewer CPUs to leave the Mac
room; how busy it is decides.

## Priority system calls

All are missing today (Firefox already calls some):

- `sched_setscheduler`, `sched_getscheduler`, `sched_setparam`,
  `sched_getparam`, `sched_setattr`, `sched_getattr` (policy, priority,
  nice, `SCHED_FLAG_RESET_ON_FORK`);
- `sched_get_priority_min`/`max`, `sched_rr_get_interval`;
- `setpriority`/`getpriority` (`PRIO_PROCESS` for a thread or the
  process; `PRIO_PGRP`/`PRIO_USER` for this process only);
- `sched_yield`: to the back of its level (fair: a task's slice ends);
- `sched_setaffinity`/`sched_getaffinity`: the 18 CPUs; a narrower mask is
  honoured by the run queue (a task waits for one of its CPUs).

Permissions as on Linux for an unprivileged user: raising priority (nice
below the current, any real-time policy) needs `RLIMIT_NICE` /
`RLIMIT_RTPRIO`, which default to 0, so EPERM; lowering is always
allowed. Apps that want real-time ask rtkit over D-Bus, which isn't there
(they carry on without).

## Pieces

### Task

```rust
struct Task {
    thread: linux_abi::Thread,   // tid, signal state, waker, clear_child_tid
    ctx: Box<hv::UserContext>,   // registers, while not on a vCPU
    resume: Resume,
    sched: SchedEntity,          // class, priority/nice, weight, vruntime, deadline, slice used, affinity
    location: Arc<Location>,     // where it is, for whoever must wake it
}

enum Resume {
    Fresh,                                          // a new thread: ctx as clone made it
    AfterSyscall { nr: u64, arg0: u64, ret: i64 },  // a worker finished its call
    Preempted,                                      // slice used, or outranked
}
```

`linux_abi::Thread` already moves between host threads (`spawn_thread`
moves it to a new one). Nothing in `linux-abi` is tied to a host thread:
the futex `Waker` is a pipe, and `IN_FLIGHT` (`fs.rs`) lives for one call
on one host thread. The one per-host-thread state is the job-control
signal mask (`Thread::sync_host_mask`), applied again when a task arrives
on a host thread (as its QoS is).

### CPU loop

```
loop {
    task = best runnable task for this CPU (or idle-wait; or stop on exit/exec)
    if this vCPU last ran another task: task.ctx.restore(vcpu)    // 0.35 µs
    host QoS and job-control mask for the task; location = Running(self)
    if task.resume is AfterSyscall: x0 = ret, TLB flush if needed, deliver
        signals (as run_loop does after a call)
    run_loop(vcpu, task) until:
        Blocked(nr, args) => save ctx, give the call to a worker
        Preempted         => save ctx, back on the run queue
        Exit / ExitThread / Exec / ExecHost / Stop => as today
    account the time run (vruntime, RT throttling)
}
```

### Workers

Host threads without a vCPU, started on demand and kept idle a while (any
number: as many as tasks blocked in them). A worker runs
`linux_abi::dispatch` for the task's call (with `Space` as guest memory, as
CPUs do), `location = InWorker(self)`, then wakes the task with its
result. A woken task prefers the CPU it last ran on if that's idle (its
registers are still there: no restore; warm caches), as Linux's wakeup
affinity.

### Waking a task

`register_thread`'s waker today is bound to one vCPU and host thread
(`thread_waker`). It becomes one that reads the task's `Location` (an
atomic: kind + index):

- `Running(cpu)`: `hv_vcpus_exit` on its vCPU, and `INTERRUPT` to its host
  thread, if it's in a call inline;
- `InWorker(worker)`: `INTERRUPT` to the worker's host thread;
- `Queued`: nothing: signals are looked at when it's resumed.

`signal.rs`' fixed tables (`VCPUS`, `HOST_THREADS`, read by host signal
handlers without locks) keep the CPUs' vCPUs; `interrupt_all` (a timer's
signal) must reach workers too, so workers get a table of their own.

## What stays the same, and what changes

- **TLB flushes**: `tlbi vmalle1is` reaches every vCPU in the VM, whichever
  task it runs, so the flush-before-return rule stands. A call done on a
  worker that changed mappings is flushed for by the CPU that resumes the
  task, which checks `needs_flush` as now.
- **Signals**: `next_delivery` and `deliver_signals` run on the CPU when it
  resumes a task, so a signal that arrives while a task is queued or on a
  worker is handled when it's back on a vCPU, as a Linux thread does on
  its way back to user mode.
- **`exit_group`, fatal signals**: `set_exiting` already wakes every thread;
  with the new wakers that reaches tasks on workers too. Queued tasks are
  dropped; CPUs leave their loops, dropping their vCPUs on their own
  threads (as now).
- **`execve` (Linux program)**: other tasks end as above; idle CPUs stay
  but reconfigure their vCPU for the new address space (the page-table
  root changes): each keeps the generation of the space it configured for.
- **`execve` (Mac program)**: every CPU drops its vCPU on its own thread
  before `hv_vm_destroy`: CPUs leave their
  loops and the scope joins them, as now.
- **`fork`**: only the calling task goes to the child, which starts with one
  CPU (more as its tasks need them); its class and priority go with it.

## Steps

1. **Refactor, no behavior change**: `Task`, CPU loop, `Location`-based
   wakers, one CPU per task as today. All tests pass; the foot latency
   benchmark (`MACWL_AUTO_KEYS`) unchanged.
2. **CPUs shared by tasks** (done, 2026-10-25): CPUs outlive their task and
   take the next from a run queue (FIFO); tasks move between CPUs
   (save/restore, skipped when a task comes back to the CPU it left); a
   ticker preempts a task that has run 10 ms while others wait; `clone`
   queues the new task (an idle CPU, a new one, or its turn) and returns at
   once; `execve` and `exit_group` end queued tasks too. The cap stays at
   64 until step 3: a blocked task keeps its CPU, so with fewer CPUs than
   blocked threads the others wait for ever (GTK 3 under `MACWL_CPUS=2`
   never draws). `MACWL_CPUS=n` lowers it, for tests. Test `manythreads`
   (guest/manythreads.c): 150 threads, spinners sharing CPUs, a signal to
   one thread, `execve` and `exit` with threads running and queued, at the
   default and 2 CPUs. Lowering the cap to `hw.physicalcpu`, and showing the
   guest that many CPUs, comes with step 3.
3. **Workers** and 4. **taking CPUs back** (done together, 2026-10-26):
   with fewer CPUs than tasks blocked in place, the lower cap would hang
   (step 4 is what lets step 3 lower it). A call that may block, with tasks
   waiting, goes to a worker (unless a cheap check says it won't block);
   a task blocked in place when tasks queue has its call interrupted
   (`INTERRUPT`, and `futex`'s wait loop asks the core whether to give
   up) and re-issued on a worker, or a spurious wakeup (0) for `futex`
   waits, or EINTR for the calls a Linux signal interrupts (`nanosleep` now reports the time
   left, as Linux). The
   ticker re-sends eviction while tasks wait, as an interrupt can land
   just before a call sleeps and be lost. A signal that comes while a call
   is on its way to a host thread (when nothing can interrupt it)
   interrupts it before it sleeps, as Linux checks. The cap is
   `hw.physicalcpu` (18 here). Races found on the way: a CPU that skipped
   restoring a task it ran before, though the task had run elsewhere since
   (stack canary failures); marking a call blocked after looking for
   waiters (fixed by marking first, SeqCst against the queue's);
   re-issuing evicted futex waits (lost wakeups, above); deciding a futex
   wait would block by reading its word on the CPU and queueing it later
   on the worker (the same ABA loss: a futex wait now queues on the CPU,
   `linux_abi::futex_begin_wait`, and only its sleep goes to the worker,
   `futex_finish_wait`); a requeued futex waiter that gave up left a ghost
   in the queue it was moved to (it took a later wake: waiters now know
   their current queue); an idle CPU or worker counted for every task or
   job queued before it woke (one could be left waiting for ever: they
   now count once each, and tasks back from workers or preemption can
   start CPUs too). Found with a hunt that runs 4 `manythreads` and 4
   `threads` at a time and samples (`sample`) any run alive after 25 s:
   from 3% hung to 0 in 960 runs, at 18 and 2 CPUs.
   `manythreads` gained 200 threads on a condition variable and a 200-pipe
   ring; it and `threads` pass at 18, 2 and 1 CPUs, also run 8 at a time.
5. **The scheduler proper**: classes, EEVDF, wakeup preemption, RT
   throttling, affinity; the priority system calls; host QoS by priority.
6. **Firefox**, and whatever it hits next.

## Tests

A guest program (`guest/manythreads.c`), run with the runtime's tests:

- 200 threads, each blocked in `pthread_cond_wait`; woken one by one, and
  all at once; joined.
- 200 threads passing a token around a ring of pipes (each read blocks).
- More busy threads than CPUs (40 spinning): each finishes; with nice
  values, CPU time splits by Linux's weights (nice 0 vs nice 5: ~3:1),
  within a tolerance.
- `SCHED_FIFO` (where allowed: setting it fails with EPERM, as for an
  unprivileged Linux user) and `SCHED_IDLE`: an idle task gets no time
  while fair ones are busy; `setpriority` lowering works, raising is EPERM.
- A woken high-priority task preempts a busy low-priority one (latency
  from wake to running, under load).
- A signal to a thread that's running, queued, and blocked on a worker
  (`tgkill`), each handled by the right thread.
- `exit_group` from one thread with others running, queued and blocked;
  `execve` and `fork` from a process with more tasks than CPUs.
- `nproc` says 18; the existing tests, unchanged; the latency benchmark:
  no slower.
- Host placement (by hand, with `powermetrics --samplers cpu_power`): busy
  tasks at nice 19 or `SCHED_IDLE` load the Performance cluster and leave
  the Super cores to the host; at nice 0, the Super cores. That's the test
  that the QoS mapping does what the article found a VM can't.

## Open questions

- Two policies for blocking (move to a worker whenever tasks wait, or keep
  blocking inline and take CPUs back when needed): which costs less for
  Firefox and GTK apps. Measure both.
- Should the Super and Performance cores be two kinds of CPU the guest can
  see (Linux on big.LITTLE shows `cpu_capacity`), or one pool with macOS
  choosing by QoS? Start with one pool.
- Several guest processes each have 18 CPUs: together they can
  oversubscribe the host, which macOS sorts out (as with native apps).
