# macwayland roadmap

A native Wayland session on macOS: a full-screen shell running a Smithay
compositor through Metal, with Linux apps running as ordinary Mac processes and
showing up as Wayland windows. The priority is latency: within a frame of a
native Mac app, measured.

## Architecture

```
 Linux app (unmodified arm64 binary)
   │  Linux system calls
   ▼
 Track B runtime: Hypervisor.framework, no guest kernel      ← static apps work
   │  Wayland over a host unix socket; buffers as IOSurfaces
   ▼
 macwl-compositor (Smithay, wayland_frontend only)           ← working
   │  committed buffers
   ▼
 macwl-shell (AppKit + CAMetalLayer, full screen)            ← working
   │
   ▼
 WindowServer → display
```

**Why Track B and not a VM (Track A).** A microVM with a real Linux kernel
(libkrun + virtio-gpu cross-domain) would be more compatible, but it costs
guest memory, slows file I/O, and forwards GPU work through Venus/virglrenderer
and then MoltenVK. Track B runs each Linux process directly under
Hypervisor.framework with system calls implemented on top of XNU. Guest memory
is host memory, files go straight to APFS, and GPU commands can be handed to
Metal through shared memory. It's far more work and the riskier bet. Track A
remains the fallback if B stalls.

## Done

| Piece | Where | Result |
|---|---|---|
| Spike 2: Smithay on macOS | `crates/compositor`, `crates/hello-client` | Smithay 0.7 builds unpatched with `wayland_frontend` only. A native wl_shm/xdg_shell client connects, passes a `shm_open` fd via SCM_RIGHTS, and commits frames. |
| Spike 1: full-screen Metal shell | `crates/shell` | Hosts the compositor and blits client frames. On-demand rendering (present immediately on input or commit). |
| Native latency baseline | `crates/native-probe` | Measures drawRect, CALayer and CAMetalLayer paths via ScreenCaptureKit `displayTime`, cross-checked against `MTLDrawable.presentedTime` (agree to <1 ms). |
| B1: vCPU limit | `crates/hv-limits`, `scripts/sign-hv.sh` | 64 vCPUs per VM, one VM per process (M4, macOS 27.2; same on M5 Pro, macOS 26.6). |
| `linux-abi` building blocks | `crates/linux-abi` | arm64 syscall numbers, Darwin→Linux errno, `stat`/`timespec`/open-flag layouts, fd table, guest→host path mapping, `GuestMemory` interface. 8 unit tests. |
| B0 scaffold: core runtime | `crates/runtime` (`macwl-run`) | Loads a static ELF, runs it at EL0 under our own page tables (see item 3); `svc` traps to an EL1 vector that does `hvc` to the host, which calls `linux_abi::dispatch` (`write`, `writev`, `exit_group`, `set_tid_address`, `brk`). Tested with hand-assembled ELFs: hello world, -ENOSYS, null-pointer fault. |
| B2: Linux Wayland client on the Mac | `crates/runtime`, `scripts/build-guest.sh` | The unmodified `hello-client`, built for `aarch64-unknown-linux-musl`, runs under `macwl-run`, connects to macwl-compositor, shares its pool through `/dev/shm` and `SCM_RIGHTS`, and draws in the full-screen macwl-shell (~90 fps). Frames arrive pixel-exact (test `b2_linux_wayland_client_draws_on_the_mac_compositor`). |

### Latency: input event → on glass

Current machine: M5 Pro, built-in 120 Hz ProMotion display. p50 / p90 over
several runs of 30–40 samples, ms. The M4 Air column (60 Hz) is from before.

| Path | M5 Pro, 120 Hz | M4 Air, 60 Hz (p50) |
|---|---|---|
| macwl-shell, on-demand rendering | 5.3–6.9 / 8.1–11.0 | 24.7–31.6 |
| Native CAMetalLayer, no capture (`presentedTime`) | 6.2–6.3 / 10.4–10.6 | — |
| Native CAMetalLayer under ScreenCaptureKit (`presentedTime`) | 22.4 / 24.8 | 25.0–29.8 |
| Native CAMetalLayer / CALayer / drawRect (ScreenCaptureKit) | 14.7–15.7 / 12.2 / 15.4 (p50) | 25.0–29.8 / 27.5–27.9 / 26.5–27.8 |
| macwl-shell, CAMetalDisplayLink pacing | 23.9 / 32.4 | 40.1–57.5 |
| foot under macwl-run: key → its next frame | 8.0–8.3 / 9.6–10.5 | 27.5–29.6 |

Findings:
- On 120 Hz the shell's median answer is under one refresh (8.3 ms), with a
  minimum of ~3.5 ms. It matches native Metal measured the same way.
- **The window must actually be full screen.** Then WindowServer shows it
  directly; an ordinary window is composited, two refreshes slower (~22 ms).
  macOS drops `toggleFullScreen` while another full-screen window's exit
  animation runs, so a shell launched right after one quit used to stay a
  window (every second back-to-back benchmark run). The shell now asks again
  until it's full screen, and `--auto` waits for it.
- **Screen capture forces compositing too**, so ScreenCaptureKit can't
  measure the direct path: under capture, native Metal's `presentedTime` is
  22 ms against 6 ms without. Its `displayTime` also runs ~7 ms ahead of
  `presentedTime` here (on the M4 they agreed to <1 ms). Use
  `native-probe metal --no-capture` as the baseline. `native-probe` still
  waits a fixed 3 s for full screen, so an occasional run is mixed.
- Frame callbacks follow the display: a client gets ~120 a second (113 over a
  600-frame run including startup), and `wl_output` now reports the screen's
  real refresh rate instead of 60 Hz.
- From the M4: display-link pacing costs about a frame; `preferredFrameLatency`
  below 1 silently clamps to 2; 3 drawables adds a frame; framebuffer-only and
  thread QoS make no difference; keyboard events reach the app ~2.4 ms after
  their timestamp, mouse clicks ~13 ms.

Reproduce: `macwl-shell --latency --auto 30` (`MACWL_SAMPLES=1` prints each
sample), `native-probe metal --no-capture 30`, `native-probe <draw|layer|metal> 30`
(needs Screen Recording permission for the terminal), and
`MACWL_AUTO_KEYS=40 scripts/session.sh` for foot. Leave a few seconds between
`native-probe` runs.

#### foot: where the time goes (M4, 60 Hz)

- foot commits its answer ~3 ms after the key (1.7–5 ms on the M5 Pro); the
  rest is presentation, as for native apps. Copying a full-screen frame out of
  the client's buffer (~3 ms) and uploading it (~6 ms) cost about half a
  refresh; damage tracking cut both to a character's worth (p50 32.6 → 27.5 ms).
- Frame callbacks don't change key latency (foot is idle when a key comes,
  with its callback in hand), but do change throughput: an 8 ms timer had a
  client drawing 113 frames/s on a 60 Hz display; callbacks sent once the
  shell had drawn a frame, 38/s (each waited for a drawable, ~1.6 refreshes);
  callbacks on a CADisplayLink tick (on its own thread), 60/s. That's what's
  used.

## Track B: remaining work

Roughly in dependency order. Items 1 and 2 are the minimum to print "hello"
from a static Linux binary.

1. **Core runtime (B0)** — Scaffolded (see Done). A static musl hello world
   (`crates/runtime/guest/hello.c`) runs: build guests with
   `scripts/build-guest.sh` (cross gcc from `flake.nix`, fetched from
   cache.nixos.org), test with `cargo test -p macwl-runtime -- --include-ignored`.
   References: Noah's
   `src/main.c` (Intel, but same overall design), Apple's Hypervisor.framework
   documentation, and `crates/hv-limits` for linking and signing.
2. **System call dispatcher** — The part of
   `linux-abi` that maps each call onto macOS. `write` and `exit_group` for
   hello world; a few dozen more for a musl shell. Noah's `src/fs`, `src/proc`,
   `src/sys` and `src/mm` show the semantics (watch for x86-64 vs arm64
   differences: arm64 has `openat` but no `open`, and some layouts differ).
3. **Memory management.** Done for private memory: 4-level page tables with
   4 KiB pages over an 8 GiB pool of guest-physical frames (so the host's 16 KiB
   pages only matter for the one stage-2 mapping), VMAs as the source of truth,
   pages filled on first touch (64 KiB fault-around, ~5 µs per fault), TLB
   flushed via a return path in the vector page when mapped entries change.
   `brk`, anonymous `mmap` (hints, `MAP_FIXED`, `MAP_FIXED_NOREPLACE`),
   `munmap`, `mprotect`, and private file mappings (copied in).
   Shared mappings (`MAP_SHARED`, file or anonymous) are host `mmap`s placed
   in guest physical memory above the private pool (`crates/runtime/src/shared.rs`),
   so a host process mapping the same file — the compositor reading a `wl_shm`
   buffer — sees the guest's writes live. Known gaps: past-EOF faults are only
   detected per 16 KiB host page (Linux: per 4 KiB, so up to 12 KiB past EOF
   reads as zero instead of SIGBUS); `mremap` is missing. Freed frames are
   quarantined until every vCPU's TLB has been flushed (the vCPU making the
   change flushes synchronously before its call returns), then reused;
   16 KiB host pages left entirely free go back to macOS
   (`MADV_FREE_REUSABLE`: a guest that unmaps 256 MiB drops from 261 MB to
   7.6 MB in Activity Monitor). `madvise` DONTNEED/FREE drop pages.
4. **Dynamic linking.** Done: the loader maps a program's PT_INTERP (musl's
   or glibc's dynamic linker) below the stack gap and starts there with
   AT_BASE/AT_ENTRY set; the dynamic linker loads libraries itself through
   private file `mmap`. Two ways to find them:
   - **Host root** (default): the guest sees the Mac's files, so Nix-built
     programs, whose libraries are `/nix/store` paths, run as they are
     (tested: GNU hello, coreutils from nixpkgs' aarch64-linux, glibc 2.44).
   - **Guest root**: `macwl-run --root DIR` makes an extracted distribution
     the guest's `/`, with the home directory, `/nix`, `/tmp`, the Wayland
     runtime directory and the current directory bind-mounted at their own
     paths (`--bind HOST[:GUEST]` adds more). Tested with the Ubuntu 24.04 and
     Alpine 3.24 base images: their own `ls`, `cat`, `id`, `find`, `df`,
     busybox `sh` and bash scripts (with items 6 and 7), and the Linux
     `hello-client` drawing on the compositor from inside Alpine.
   Not a sandbox: guest programs run with the user's own permissions, can
   reach the bound home directory, and `..`/`*at()` tricks reach any host
   file. For untrusted binaries, run `macwl-run` under a macOS sandbox
   profile (not done yet).
   Not yet: a vDSO (glibc falls back on system calls), the user's own
   entry in the root's `/etc/passwd`, `/proc/self/mounts` (`df` warns).
   `MACWL_STRACE=1` logs every system call, for finding what a program needs
   (`MACWL_STRACE=PATH`: to `PATH.PID`, a file per process).
5. **Threads.** Done: `clone(CLONE_THREAD)` starts each guest thread on a
   host thread with its own vCPU in the one VM (up to 64, Hypervisor.framework's
   limit). They share the address space and `Process` behind short locks, so a
   thread blocked in a system call holds nothing up; per-thread state (tid,
   signal mask, alternate stack, thread-directed signals, `clear_child_tid`)
   is the core's `Thread`. A real `futex` (wait, wake, bitsets, requeue)
   sleeps on a per-thread wake pipe that wakes, signals, timeouts and exit
   all end. `exit` ends one thread (CLEARTID: what `pthread_join` waits for),
   `exit_group` or a fatal signal all of them; `execve` stops the others
   first; fork takes only the caller. Signals: a host signal kicks every vCPU,
   a signal to one thread (`tgkill`) also breaks it out of a blocking host
   call. Tested with musl pthreads (guest/threads.c: mutexes, condvars, TLS,
   48 threads, signals to a thread blocked in `read`, fork and exec from a
   threaded process) and glibc: Ubuntu's `sort --parallel=4` runs 2.3x
   faster than with one thread. Not yet: robust futexes, PI futexes,
   `FUTEX_WAKE_OP`, futexes shared between processes, threads with their own
   fd table or cwd.
   **Before Firefox and big GTK apps: more than 64 threads** (design:
   `docs/mn-threads.md`).
   Hypervisor.framework allows 64 vCPUs and one VM per process (B1), and
   today each guest thread has a vCPU. A second VM would mean a second host
   process sharing guest memory, TLB flushes and every fd: not worth it.
   Instead, multiplex (M:N): a vCPU is only needed while a thread runs guest
   code. Keep a pool of up to 64 runner host threads, each owning a vCPU
   (a vCPU must run on the thread that created it). When a guest thread makes
   a blocking call, save its registers, run the call on a worker host thread,
   and let the runner take another ready guest thread; when the call
   returns, the guest thread queues for a vCPU. Big apps' threads are mostly
   blocked, so they'd rarely wait.
6. **Processes.** Done: `clone` as fork and vfork (`posix_spawn`), `execve`
   (with `#!` scripts), `wait4`/`waitid`, `kill` to other processes. Ubuntu's
   bash runs scripts with pipelines, subshells and background jobs
   (`crates/runtime/src/fork.rs`, test `processes`). How:
   - A host `fork()` can't be used: Hypervisor.framework state doesn't survive
     it (the child's `hv_vcpu_create` crashed in `os_log` ~6% of the time). A
     fork child is a fresh `macwl-run --fork-child` that's handed the process.
   - Guest memory goes over as a copy-on-write Mach memory entry
     (`MAP_MEM_VM_COPY`), in RAM. Not a file: macOS writes dirty file-backed
     pages back to disk, ~4 GB/min for a busy 1 GiB guest
     (`examples/disk_writes.rs`). Shared mappings, fds and the rest go over a
     Unix socket with `SCM_RIGHTS`.
   - Launching any non-Apple binary costs ~2–3 ms on macOS, so a process that
     has forked keeps a spare child launched ahead of time.
     fork + exec + wait of `/bin/true` takes ~2.5 ms in a tight loop (native
     Linux: ~0.5 ms); fork alone ~0.5 ms with a spare ready. `exec` reuses
     the VM. `MACWL_PROFILE=1` prints where the time goes.
   - Wait statuses carry Linux signal numbers (a guest killed by a signal
     tells its parent over the control socket; host signals are translated).
   Not yet: threads in a forked process (item 5), `CLONE_VM` without vfork,
   pidfds, `kill(-1)` (refused: on a Mac that's the whole login session).
7. **Signals.** Done: handlers run, in Linux arm64 signal frames on the guest
   stack (siginfo, ucontext with registers and FP/SIMD state, frame record),
   undone by `rt_sigreturn`; handlers without SA_RESTORER (glibc's) return
   through a trampoline page standing in for the vDSO's. Masks, pending
   signals, `rt_sigsuspend`, `rt_sigpending`, SA_RESTART (interrupted calls
   restart), SA_ONSTACK, SA_RESETHAND, SA_NODEFER. Faults go to SIGSEGV,
   SIGBUS, SIGILL, SIGTRAP handlers if there are any. Signals from outside
   are host signals: for each one the guest handles, a host handler marks it
   pending and kicks the vCPU out (`hv_vcpus_exit`), so even a busy loop gets
   it; ignored and default ones are left to the host, so Ctrl-C and job
   control stops work natively. `EPIPE` raises SIGPIPE (`yes | head` ends);
   `setitimer`/`getitimer` give SIGALRM. bash and busybox `sh` run traps,
   background jobs and `kill %1` (test `signals`, guest/signals.c).
   `signalfd` (`crates/linux-abi/src/syscall/signalfd.rs`): a kqueue whose
   user event the host signal handler (and anything making a signal
   pending) triggers; signals a signalfd watches are caught on the host
   whatever their action, so a blocked SIGTERM or SIGCHLD waits to be read
   rather than the host acting on it. A blocking call that such a signal
   interrupts goes back in, as on Linux, where blocked signals interrupt
   nothing (it returned EINTR before). Test `signalfd`, guest/signalfd.c.
   Not yet: real-time signal queueing (`rt_sigqueueinfo`, more than one of
   a signal pending), `rt_sigtimedwait`, sender details in siginfo for
   signals from other processes, signals sent to one thread read from a
   signalfd.
8. **Filesystem breadth.** Done: file I/O, `getdents64`, the `*at` path
   calls, `fcntl`, pipes, `ppoll`, `fallocate`. With a separate guest root,
   paths resolve with guest semantics (absolute symlinks stay inside it), at ~15x the cost of a
   native `stat`; with the host's `/` as root they pass straight through
   (~2x native, dispatcher only). `/proc/self/{exe,cwd,fd/N,maps,mounts,mountinfo}`
   are emulated, and the machine's `/proc/{cpuinfo,meminfo,mounts,uptime,loadavg,version}`
   are made from macOS's figures (`crates/linux-abi/src/hostinfo.rs`; mounts
   are the host's browsable ones with the Mac's `/` as root, else the root and
   binds); a root without `/etc/os-release` gets one naming macwayland and the
   macOS version. `/dev/*` are the host's devices. Also `statx`, `statfs`,
   `sendfile`, `fcntl` record locks (OFD locks taken as POSIX ones), and
   termios ioctls (translated, since Linux and Darwin flag bits differ).
   Missing: per-process `/proc` (item 14), xattrs (reported as unsupported),
   reflinks and `copy_file_range` (callers fall back to read and write).
   `memfd_create` and `/dev/shm/*` (musl's `shm_open`) are macOS POSIX shm
   objects (`crates/linux-abi/src/shm.rs`), which stay in RAM: as files they
   were written back to the SSD every ~15 s (a full-screen Wayland client's
   buffers: several GB an hour; measured with `examples/disk_writes.rs`, 1 GiB
   rewritten for a minute: 3.6 GB written as a file, nothing above idle as
   shm). macOS sizes shm objects once and can't read/write/seek them, so each
   is 8 GiB of address space (free until touched) with the Linux size in a
   header in its last page, and file I/O is emulated through mappings. Seals
   are accepted, not enforced.
9. **Unix sockets with fd passing.** Done: Unix sockets (paths, plus Linux
   abstract names as private host files), IPv4/IPv6 addresses, `sendmsg`/
   `recvmsg` with `SCM_RIGHTS` both ways, common socket options, `SO_PEERCRED`.
   Tested end to end under the hypervisor: a guest sends a memfd to a host
   "compositor", which maps it and writes back into the same buffer. Missing:
   `SCM_CREDENTIALS`, `sendmmsg`/`recvmmsg`, autobind, host socket paths over
   103 bytes (Darwin's `sun_path` limit).
10. **B2 milestone.** Done (see Done).
11. **GPU.** Split Vulkan driver (state tracking and shader translation in the
    guest, Metal calls on the host) over a shared-memory command ring, plus
    IOSurface swapchains.
12. **Real apps.** In progress. Done: `epoll` (on kqueue; a fork child gets
    its own kqueue watching the same fds), `eventfd` and `timerfd` (kqueue
    based, so epoll can watch them), pseudo-terminals (`/dev/ptmx` with
    Linux's pty ioctls, `/dev/pts/N` as `/dev/ttysNNN`; a master's size
    waits until its slave opens, which is when macOS allows it), read-only
    shared file mappings (fontconfig's caches), POSIX timers. **foot**
    (nixpkgs' Linux build) runs full-screen in macwl-shell
    (`scripts/session.sh`) with keyboard input, bash with job control, vim,
    and fastfetch. **GTK 3 and GTK 4** (nixpkgs' `gtk3-widget-factory`,
    `gtk4-widget-factory` with `GSK_RENDERER=cairo`) draw their whole window
    on the compositor. Nix's GTK wrappers expect the environment NixOS sets:
    `GDK_PIXBUF_MODULE_FILE` (librsvg's `loaders.cache`, for SVG icons) and,
    for GTK 4's widget factory, `GST_PLUGIN_SYSTEM_PATH_1_0`
    (gst-plugins-base). **Pointer input and popups** (2026-10-25): mouse
    moves, buttons and wheel/trackpad scrolling reach the client, and
    popups (menus, combo boxes, tooltips) are drawn over their window,
    placed by their positioner (kept on screen), with xdg_popup grabs: a
    click outside closes a menu. Tested headless (foot: drag to select,
    wheel through scrollback; GTK 3: open a combo box, choose an item,
    click outside to close it, scroll a text view) and in macwl-shell
    (the same in GTK 3, input sent to the shell's process with
    `CGEventPostToPid`). **`inotify`** (2026-10-25,
    `crates/linux-abi/src/syscall/inotify.rs`): each watch is an FSEvent
    stream on its directory, its reports turned into Linux events (a
    directory watch keeps its entries, to tell creates from moves in, and
    pairs a rename's halves by inode). Tested with inotify-tools'
    `inotifywait` (create, write, rename, mkdir, delete) and GLib, which
    now has file monitors. Differences: a write is reported when the file
    is closed (that's when macOS says), `IN_ACCESS`/`IN_OPEN`/
    `IN_CLOSE_NOWRITE` never come, a moved watch keeps watching the old
    path. `close_range` and `signalfd` (item 7) too. A root without one
    (the Mac's `/`) gets `/etc/machine-id` (and `/var/lib/dbus/machine-id`):
    the Mac's hardware UUID, hashed, so the same each run.
    `scripts/session.sh` starts a D-Bus session bus (nixpkgs' `dbus-daemon`
    under macwl-run; `DBUS_SESSION_BUS_ADDRESS` reaches guests) and gives
    the shell what GTK apps expect NixOS to set: Adwaita and hicolor icon
    themes (`XDG_DATA_DIRS`), the SVG pixbuf loader, GStreamer's plugins.
    rofi exits at once: it needs the compositor to offer wlr-layer-shell.
    **Firefox** (nixpkgs' 158, `firefox-unwrapped`, sandboxes off, 2026-10-25):
    first crashed in `mozilla::image::DecodePool::DecodePool` (a
    `MOZ_RELEASE_ASSERT` that its image threads start): its main process
    wants more than 64 threads, and the 65th got no vCPU. With the
    scheduler (item 5, `docs/mn-threads.md`) it runs on, starts its child
    processes, opens its window and **renders a page** (a `file://` one;
    it refuses a `data:` URL from the command line). It draws into a
    subsurface: the compositor composites subsurface trees (synchronized
    ones with their parent's commit, stacked above or below it) and hands
    each surface to the shell as a layer (`crates/compositor/tests/
    subsurfaces.rs`). No toolbars: every window is configured fullscreen,
    which Firefox takes as its fullscreen mode. Fixed on the way:
    - `SOCK_SEQPACKET` socketpairs (its sandbox broker) are host datagram
      pairs, whose 2 KB message limit is lifted with bigger buffers; a
      closed peer reads as the end of the file and writes get EPIPE;
    - memfd seals are kept with the object and enforced, and reported (it
      checks `F_SEAL_SHRINK` before mapping);
    - XNU's garbage collector for fds in flight flushes a socket sent
      through a socket that is itself in flight (its fork server gets one
      end of a pair, the child's IPC socket goes through the other): the
      child read the end of the file, every child exited with "channel
      error". A sender now keeps what it sends reachable until a receipt
      fd with the message is closed by the receiver (`net::in_flight`);
    - poll said POLLPRI for readable pipes, as Darwin does and Linux
      doesn't, so its socket thread rebuilt its wakeup pipe for ever;
    - private read-only file mappings were copies of the whole file (it
      maps the 23 MB system font thousands of times at startup, and every
      process maps libxul): now a window onto the file, copied only if
      made writable (30x faster to map);
    - fontconfig had no `Sans` (GTK's default), so "Sans" was Hiragino;
    - `flock`, `readahead`, `getpriority` and the `sched_get*` queries.
    Still unimplemented, not in the way yet: `quotactl`, `ioprio_get`,
    `pidfd_open`, `capget`, `prctl(PR_SET_TIMERSLACK)`. One process
    ends with "unexpected VM exit: reason 3" when the run is killed (not
    looked at yet). Its sandbox
    needs seccomp-bpf (the dispatcher can enforce filters itself, SIGSYS
    included); namespaces and `chroot` it can do without.
    Diagnostics added for it: `MACWL_FAULTS=1` (each fault that becomes a
    signal: pc, address, and the files and offsets of the frame-pointer
    chain), `/proc/self/maps` names mapped files, and a thread that gets no
    vCPU says so.
13. **Mach-O passthrough.** Let Linux programs run native Mac binaries, as
    WSL runs Windows ones, so a shell in foot can run macOS tools (`open`,
    `pbcopy`, `git`, Xcode's) next to Linux ones. **Works** (tested
    below; on in `scripts/session.sh`, opt-in in `macwl-run`).
    - Done in code: `execve` reads a file's first 256 bytes; a Mach-O
      (thin 64-bit, or universal with < 20 architectures, which tells it
      from a Java class file) becomes `Action::ExecHost`, as does a `#!`
      script whose interpreter is one. `linux_abi::syscall::exec_host`
      replaces the process with it for real: `posix_spawn` with
      `POSIX_SPAWN_SETEXEC` (same pid, so the parent's `wait`, the
      terminal and job control carry on), `CLOEXEC_DEFAULT` plus `adddup2`
      putting each inherited guest fd at its guest number (by way of fds
      above all of them, so no move clobbers a later source),
      `addchdir_np` to the guest cwd, the guest's ignored signals kept
      ignored and the rest defaulted, the guest's mask as its mask. The
      environment is the guest's but with the Mac's `PATH` and `SHELL`
      (those macwl-run started with), so Mac `git` finds Mac `less` and
      `vim`. A parent's `setpgid` on a child that has gone Mac gets EACCES
      (Linux's answer for an exec'd child). Off by default;
      `--mac-programs` / `MACWL_MAC_PROGRAMS=1` turn it on.
    - **The VM goes first:** before `SETEXEC` (see `execute` in
      `crates/runtime/src/lib.rs`) the exec'ing thread stops the others,
      each vCPU is destroyed on its own thread, then guest memory is
      unmapped, the VM destroyed, the memory, fork memory entries and spare
      child dropped; the exec is refused if `hv_vm_destroy` fails, so the
      Mac program starts from a clean process with nothing of the guest
      left in it. With
      `SETEXEC`, close-on-exec fds are closed *before* the file actions
      run, so `dup2` from one fails with EBADF, and every host fd is
      close-on-exec: the flag is cleared on the fds passed on.
    - `#!/usr/bin/env NAME` (and `/bin/env`) with a plain NAME looks NAME
      up on the guest's `PATH` itself, as Linux `env` would, so portable
      scripts run on Linux when a Linux NAME exists, falling back to `env`
      itself (on the Mac's `/`, Mac `env` and the Mac's PATH) when none
      does. Absolute interpreters (`#!/bin/sh`, `#!/bin/bash`) keep
      following the path: on the Mac's own `/` they're Mac programs, which
      Mac scripts (Homebrew's `brew`) rely on. Under a Linux root `env` is
      Linux's anyway.
    - **Tested** (2026-10-24, M5 Pro): from Linux bash, Mac
      `uname`/`sw_vers`/`false`/`env`; pipes mixing the two both ways;
      exit codes and signals through `wait` (TERM, KILL, USR1, with
      Darwin numbers translated); fds by guest number (`exec 5>f;
      /bin/sh -c 'echo >&5'`), redirects, cwd; SIGSTOP/SIGCONT, SIGTSTP
      and `bg`, SIGINT to a Mac job; musl `posix_spawn` (vfork) of Mac
      programs, output through a pipe, 20 in a row; a Mac binary that
      fails to exec (the process exits 127 with a message, since the VM is
      gone by then); and, as `scripts/session.sh` sets things up, Mac
      `git` with Mac `less` as its pager, `pbcopy`/`pbpaste`, and full-screen
      Mac `vim` on a pty.
    - **Tested from inside foot** (2026-10-25, typed through the headless
      compositor's `MACWL_TYPE`, which now has `\cX` for Ctrl+X, `\e` for
      Escape and `\p` for a pause): Ctrl-Z stops Mac `/bin/sleep`, a
      `/bin/sleep | tr` pipeline and Mac `vim` (`jobs` says Stopped; `bg`,
      `fg` and Ctrl-C work after it; vim redraws, and saves after `fg`).
      Homebrew's `brew` (a Mac bash script): `--version`, `--prefix`,
      `list`, `info --json`, `config`. Xcode 27.1: `xcrun`, `clang` and
      `swiftc` building binaries that then run from Linux bash (exit codes
      kept), Mac `make`, `xcodebuild -version`, `lldb` running a program.
    - `TERM`: macOS's ncurses (5.7) has no `foot` entry and can't read
      Linux's (ncurses 6's format), so a Mac program whose `TERM` has no
      entry it can read gets `TERM=xterm-256color`.
    - `scripts/session.sh` turns Mac programs on and puts the Mac's `PATH`
      after the Linux directories.
    - Limits: Mac programs can't start Linux ones (macOS's `execve` has no
      hook for ELF files); under `--root` they see the Mac's file system,
      not the guest's, and path arguments aren't translated. A Mac binary
      that fails to exec can't return the error to the caller (the VM is
      torn down first), so `execve`/`posix_spawn` "succeed" and the process
      exits 127 (Linux bash would say 126). Still off by default in
      `macwl-run` itself.

14. **The long tail: a real kernel for what's missing?** System calls the
    runtime doesn't implement could go to the real Linux kernel, run as a
    library in user space (LKL, the Linux Kernel Library), rather than
    being written one by one. The hard part is shared state: `io_uring`,
    seccomp, namespaces work on the process's fds, memory and signals,
    which the runtime owns, so the kernel would need them mirrored.
    Subsystems that stand alone (a filesystem driver, a network stack)
    fit best. An idea, not started (2026-10-26).
15. **Per-process `/proc`.** `ps`, `top`, `htop`, `pgrep`, `kill -l` by name
    and the like list `/proc` and read `/proc/<pid>/{stat,status,cmdline,
    comm,environ}`, plus the system-wide `/proc/stat` (CPU time) for
    percentages. Today only the process's own `/proc/self` (or its own pid)
    exists. Guest pids are host pids, so the macOS process list
    (`proc_listpids`, `proc_pidinfo`, `KERN_PROCARGS2`) can supply them:
    state, parent, process group, session, tty, CPU times, memory sizes,
    start time, and argv/environment where macOS allows reading them (only
    for our own user's processes). Open questions: whether to list only
    guest processes (macwl-run ones, with their Linux names and argv rather
    than `macwl-run --fork-child`) or every Mac process too; and keeping
    `readdir` on `/proc` cheap enough for `top` refreshing every second.

## Compositor and shell: remaining work

Needed whichever way Linux apps end up running.

- Retina scaling (client buffers currently drawn 1:1 in device pixels).
- Trackpad scrolling: macOS's own momentum (the glide after a flick) is
  forwarded as finger scrolling, so every app glides like a Mac app,
  terminals included; clients are told scrolling stopped only when it
  ends, stamped so they have no speed to glide on with themselves. Finger
  deltas go out in libinput's units (macOS's points / 2.5: GTK 4 scales
  them up by 2.5), so GTK 4 follows the fingers 1:1; GTK 3 scales by
  `page_size^(2/3) / 10` and stays faster. Known quirk: macOS's glide
  keeps going past a list's end, which GTK takes as up to 100 px of
  invisible overscroll; a light flick back right after hitting the end
  spends itself undoing that (GTK cancels its spring back when the new
  flick starts). Letting apps glide themselves would fix it but take the
  glide from terminals; chosen against (2026-10-25).
  `MACWL_LOG_SCROLL=1` prints macOS's trackpad events.
- `wl_seat`: keyboard (US layout) and pointer input are forwarded; still
  to do, an xkbcommon keymap from the macOS layout, pointer gestures,
  and relative pointer/pointer constraints (games).
- Cursors: what the client under the pointer asks for is the Mac's
  cursor. Named ones (cursor-shape-v1: GTK 3 and 4, foot) are the Mac's
  own (I-beam, pointing hand, resize arrows, ...); a client's own image
  (`wl_pointer.set_cursor`) becomes an `NSCursor` at its scale, with its
  hotspot. Off every window, the arrow. `MACWL_CURSOR_SHAPE=0` leaves
  cursor-shape-v1 out, so clients draw their own (to test that path).
  Not yet: animated cursors (a cursor surface's frame callbacks).
- What's drawn: a window and its popups, as layers the host composites
  (the window copied in, popups blended over it). Subsurfaces aren't drawn
  yet (GTK 4's GL renderer, video players use them).
- IOSurface-backed buffers instead of copying shm pixels into a texture (only
  damaged areas are copied now, which covers typing; scrolling and video still
  copy whole frames).
- Window management: windows stack in the order they map; the top one
  without a parent fills the screen, and dialogs (windows with a parent:
  GTK's file chooser) take their own size, centred over their parent by
  window geometry, above it. The keyboard goes to the top window, back
  down the stack as windows close or unmap, and to a window clicked (or
  the dialog it has open). Still to do: switching between full-screen
  windows, moving and resizing dialogs (`xdg_toplevel.move`), modality.
- Clipboard (compositor mode): the Wayland clipboard and the Mac's are one,
  as text, both ways: a client's copy goes onto the Mac's pasteboard; a
  Mac copy (seen by polling `changeCount` every 250 ms) is offered to
  clients. Keys stay Linux's: Ctrl+Shift+C/V in terminals, Cmd is Super.
  Not yet: images and other types, the primary selection (middle-click
  paste), drag and drop.
- **Single-app mode** (a Linux app as its own Mac window, looking native),
  when it exists, should take Mac keybindings instead: Cmd+C/V/X/Z/A/Q
  translated into what the app expects (Ctrl+... for most, Ctrl+Shift+C/V
  for terminals), Cmd+Q and Cmd+W as the Mac's. (Hayashi can provide
  this; see below.)
- Mac apps as clients via ScreenCaptureKit, with input forwarded back.

## Target: Hayashi on macOS

[Hayashi](https://github.com/Spheroman/hayashi) is a Smithay compositor with
nested hierarchical workspaces, the same library as macwl-compositor. Rather
than growing a window manager here, the goal is for macwayland to provide a
reusable macOS backend that any Smithay compositor can sit on, with Hayashi as
the first user.

Hayashi today: Smithay 0.7 with `backend_drm`, `backend_udev`, `backend_libinput`,
`backend_session_libseat`, `xwayland` and `backend_winit` + `renderer_gl`. All of
its Linux coupling is in `src/backend/winit.rs` and the Cargo features; the
window-management core (`state.rs`, `tree/`, `input/`, `handlers/`) is
backend-agnostic.

Plan:

1. **Extract `macwl-backend`** from macwl-shell: full-screen AppKit window,
   CAMetalLayer presentation with on-demand rendering, and NSEvent input
   translated into Smithay `InputEvent`s (macOS virtual keycodes → evdev
   keycodes, so xkbcommon keymaps and existing keybinds work unchanged).
2. **Run calloop inside the AppKit run loop.** Hayashi uses
   `calloop::EventLoop::run`, but AppKit owns the main thread. Register
   calloop's kqueue fd as a run-loop source and dispatch it when readable.
3. **Gate Hayashi's Linux features by target** in its `Cargo.toml` (DRM, udev,
   libinput, libseat, XWayland on Linux only) and add `src/backend/macos.rs`
   using `macwl-backend`.
4. **Render with Smithay's pixman (software) renderer first.** Verified to
   build and run on macOS (pixman from Homebrew). `space::render_output` works
   with any Smithay renderer, so Hayashi's render code barely changes. Composite
   into a CPU buffer, upload, present.
5. **Metal renderer for Smithay.** Implement Smithay's `Renderer` traits on
   Metal with IOSurface imports. Replaces pixman for both Hayashi and
   macwl-compositor.

Clients: macOS-native Wayland clients (like `hello-client`) work as soon as
step 4 lands. Real apps (foot, waybar, browsers) arrive with Track B. Until
then Hayashi on macOS is mainly useful for developing its window-management
logic on the Mac.

## Target: Windows games (Proton with D3DMetal)

Steam in the session with a **Proton-D3DMetal** compatibility tool: Valve's
Proton (arm64 Wine, FEX for the game's x86 code) with DXVK and VKD3D-Proton
replaced by proxy DLLs that tunnel Direct3D calls to Apple's D3DMetal in an
x86_64 helper process on the Mac, presenting through IOSurfaces. Design and
steps: [docs/d3dmetal-proton.md](docs/d3dmetal-proton.md). Step 0 done
(2026-11-01): D3DMetal runs in a plain Mac process through UTM's
d3dmetal-native (51 of 55 of its tests pass). Step 1 done: a D3D11
triangle reaches macwl-compositor as `wl_shm` buffers over D3D shared
textures (shm fds), so no new protocol is needed. Next, the tunnel. Needs FEX under
macwl-run for real games, but the graphics side can be built and tested
with arm64 Wine and Windows-on-ARM test programs first.

## Target: ROS 2 and Gazebo

ROS 2 Jazzy with Gazebo Harmonic, as a robotics workstation. Until this
works, a VMware Fusion VM (Ubuntu 24.04 arm64, OpenGL 4.3 on Metal) is what
gets used. Audited against the runtime 2026-10-28; in the order they'd be hit:

1. **DDS discovery.** Every node finds the others over UDP multicast.
   - `AF_NETLINK` (`NETLINK_ROUTE`): glibc's `getifaddrs()` asks netlink,
     and Fast DDS and Cyclone list interfaces with it. `socket()` refuses
     every family but Unix, IPv4 and IPv6 today. `RTM_GETLINK` and
     `RTM_GETADDR` answered from the Mac's `getifaddrs` should do.
   - Socket options (`crates/linux-abi/src/syscall/net.rs` maps only a
     few): `IP_ADD_MEMBERSHIP`/`IP_DROP_MEMBERSHIP`, `IP_MULTICAST_IF`/
     `_LOOP`/`_TTL` and their IPv6 forms, `SO_REUSEPORT` (several
     participants bind 7400), `IP_PKTINFO`.
   - Milestone: `demo_nodes_cpp` talker and listener, with
     `FASTDDS_BUILTIN_TRANSPORTS=UDPv4`.
2. **`ros2 launch` and the CLI.** Python, many subprocesses, `setsid`,
   `killpg`, SIGINT passed on to children; the CLI talks to the `ros2
   daemon` over local TCP. Probably mostly there after items 6 and 7; find
   the rest with `MACWL_STRACE`.
3. **Root.** `docker export` of an arm64 `ros:jazzy` container as
   `--root` sidesteps apt and dpkg; nix-ros-overlay works with the host root.
4. **RViz2.** Qt + Ogre, X11 only in practice (`QT_QPA_PLATFORM=xcb`), so
   it needs XWayland on macwl-compositor. With that, Mesa's llvmpipe should
   draw it without any GPU work (threads, executable mappings).
5. **Futexes shared between processes.** Fast DDS's shared-memory transport
   (on by default) and iceoryx (Cyclone's) keep process-shared mutexes and
   condvars in `/dev/shm`. `futex.rs` ignores `FUTEX_PRIVATE_FLAG`, and each
   process is its own VM, so a wake never crosses processes: key non-private
   futexes by (shm object, offset) in a table every macwl-run process sees.
   `set_robust_list` is a no-op (`syscall.rs`), so a node dying holding such
   a lock hangs the rest; robust lists need walking on exit. Until then,
   UDPv4 only (item 1).
6. **vDSO.** DDS and executors call `clock_gettime` constantly; each is a
   trap today. Throughput, not correctness.
7. **Gazebo.** Ogre2 wants OpenGL 3.3+. With item 11's Vulkan driver, GL
   comes from Mesa's Zink on top, which asks a lot of Vulkan (more than a
   swapchain), plus a WSI handing out IOSurface-backed buffers through
   XWayland (DRI3-like). Harmonic's experimental
   `--render-engine-api-backend vulkan` would skip Zink.

Before the GPU: run `gz sim -s` (physics needs no GPU; camera and lidar
sensors do) under macwl-run, and Gazebo's GUI natively from Homebrew
(`osrf/simulation`). They talk over gz-transport (ZeroMQ, UDP multicast
discovery), so item 1 is all it needs.

## Target: NixOS dotfiles

The [dotfiles flake](https://github.com/Spheroman/dotfiles) (`master` is NixOS,
`darwin` is this Mac) as a concrete "daily use" goal.

- **System layer (NixOS modules)** — greetd, NVIDIA, systemd services, the
  system side of Hyprland/COSMIC/KDE. Doesn't apply: Track B has no kernel or
  systemd. (Track A would run real NixOS; that's the main thing B gives up.)
- **CLI and dev tools** (`shell.nix`, `neovim.nix`, `packages/cli.nix`) — run
  natively through nix-darwin already. Better there than through macwayland;
  finishing the deferred cross-platform home-manager refactor covers them.
- **Linux GUI apps from `users/jackw`**, in the order they should come online:
  1. foot, rofi — software-rendered, so usable soon after B2.
  2. waybar and other GTK apps — software-rendered via cairo.
  3. Zen browser — software WebRender first, accelerated after GPU work.
  4. Hyprland, nested full-screen inside macwl-shell (keeps the whole Hyprland
     config, keybinds and grimblast). Needs GLES 3, so only after GPU work.
     Alternatives: Hayashi (above), or a native port of COSMIC's Smithay-based
     `cosmic-comp`.
  - Not portable: OBS with `cudaSupport`, anything NVIDIA-specific.
- **Packaging.** Don't run Nix inside the guest (its build sandbox needs Linux
  namespaces). Build an `aarch64-linux` home-manager configuration on the Mac
  (cache.nixos.org substitutes, plus nix-darwin's `nix.linux-builder` for
  anything that must build) and copy the closure into the guest root's
  `/nix/store`.
- **Dotfiles changes.** A `hosts/macwayland` home-manager configuration that
  imports `users/jackw` and disables hardware-specific parts; anything started
  by systemd user units moves to Hyprland `exec-once`.

## Constraints

- No SIP changes on a daily-driver Mac. Only public APIs, TCC permissions
  (Accessibility, Screen Recording) and ad-hoc signing with the hypervisor
  entitlement.
- Replacing WindowServer outright is out of scope except on a spare machine.
- Compare performance against native baselines on the same machine rather
  than absolute numbers.
