# macwayland

Unmodified arm64 Linux programs running on macOS as ordinary Mac processes,
showing their windows in a native Wayland session. No Linux VM, no guest
kernel.

Each Linux process runs directly under Apple's Hypervisor.framework, and its
system calls are implemented on top of XNU. Guest memory is host memory, and
files go straight to APFS. Windows go to a [Smithay](https://github.com/Smithay/smithay)
compositor, which a full-screen AppKit and Metal shell puts on screen. The
priority is latency: measured to be within a frame of a native Mac app.

**Status: experimental.** It works well enough to live in a terminal, but
expect missing system calls and rough edges.

## What runs

- **foot**, with bash, job control, vim, fastfetch
- **GTK 3 and GTK 4** apps (software rendering), with pointer input, popups,
  cursors and a clipboard shared with the Mac's
- **Firefox**, which starts its child processes and renders a page (no
  toolbars or sandbox yet)
- **Mac programs from Linux ones**, opt-in: Mac `git`, `brew`, `pbcopy` or
  Xcode's `clang` started from Linux bash run natively in the same process,
  the way WSL runs Windows programs
- Programs from Nix (`aarch64-linux`) out of the box, or an extracted distro
  root with `--root`

Not yet: GPU acceleration, Retina scaling, wlr-layer-shell (rofi, waybar).
See [ROADMAP.md](ROADMAP.md) for the full list and design notes.

## How it works

```
 Linux app (unmodified arm64 binary)
   │  Linux system calls
   ▼
 macwl-run: Hypervisor.framework, no guest kernel
   │  Wayland over a host Unix socket
   ▼
 macwl-compositor (Smithay)
   │  committed buffers
   ▼
 macwl-shell (AppKit + CAMetalLayer, full screen)
   │
   ▼
 WindowServer → display
```

| Crate | What it is |
|---|---|
| `crates/runtime` | `macwl-run`: loads ELF programs and runs them under Hypervisor.framework, with fork, threads, signals and an M:N scheduler of guest threads on vCPUs |
| `crates/linux-abi` | The Linux system call layer: files, sockets, epoll, ptys, inotify (on FSEvents), futexes, `/proc` and so on |
| `crates/compositor` | The Wayland compositor (Smithay, `wayland_frontend` only) |
| `crates/shell` | `macwl-shell`: the full-screen Metal shell that hosts the compositor |
| `crates/native-probe` | The native latency baseline, measured with ScreenCaptureKit |
| `crates/hv-limits`, `crates/hello-client` | A vCPU-limit probe and a minimal Wayland test client |

`third_party/smithay` is Smithay 0.7.0 with a small macOS fix (see its
`MACWAYLAND.md`).

## Requirements

- An Apple silicon Mac. Tested on macOS 26.6 and 27.
- Rust (stable, edition 2024).
- `libxkbcommon`, from Nix (`nix profile install nixpkgs#libxkbcommon`, or
  home-manager) or Homebrew (`brew install libxkbcommon`). The compositor's
  build script finds it through `pkg-config` or the usual Nix and Homebrew
  locations; set `XKBCOMMON_LIB_DIR` if it's somewhere else.
- [Nix](https://nixos.org), for Linux programs and the guest cross-toolchain.

There's no need to turn off SIP or join the Apple Developer Program. Binaries
are ad-hoc signed with the `com.apple.security.hypervisor` entitlement:
`cargo run` and `cargo test` do this automatically
(`scripts/hv-runner.sh`); otherwise run `scripts/sign-hv.sh <binary>`.

## Try it

The full session (foot with a Linux bash, Mac programs on, a D-Bus session
bus). Cmd+Q quits:

```sh
scripts/session.sh
```

To run a single Linux program against the Mac's own file system (fine for
programs from Nix, whose libraries are `/nix/store` paths):

```sh
cargo build --release -p macwl-runtime
scripts/sign-hv.sh target/release/macwl-run
target/release/macwl-run "$(nix build nixpkgs#legacyPackages.aarch64-linux.hello --no-link --print-out-paths)/bin/hello"
```

`macwl-run --root DIR` uses an extracted distribution as `/` instead
(`docker export` of an arm64 container works).

Useful environment variables: `MACWL_STRACE=1` traces system calls,
`MACWL_FAULTS=1` explains faults that become signals, and
`MACWL_MAC_PROGRAMS=1` (or `--mac-programs`) lets Linux programs exec Mac
binaries.

## Tests

```sh
scripts/build-guest.sh   # builds the Linux test programs (uses the flake's cross-toolchain)
cargo test --workspace
```

## License

MIT; see [LICENSE](LICENSE). The vendored Smithay is MIT as well
(`third_party/smithay/LICENSE.txt`).
