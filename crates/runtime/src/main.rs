//! `macwl-run [--root DIR] [--bind HOST[:GUEST]]... [--mac-programs]
//! [--host PATH | --new-host] PROGRAM [ARGS...]`
//! `macwl-run --serve PATH`
//! `macwl-run --ps [--host PATH]`
//!
//! Runs an arm64 Linux program. Without `--root` the program sees the Mac's
//! own filesystem, which is enough for programs from Nix (their libraries are
//! `/nix/store` paths). With `--root`, DIR is the guest's `/` — an extracted
//! Linux distribution, say — and some host directories are bind-mounted into
//! it at their own paths: the home directory, `/nix`, `/tmp`, the Wayland
//! runtime directory and the current directory. `--bind` adds more. PROGRAM
//! is a guest path.
//!
//! Hosts and clients (docs/processes.md, section 1): `--serve PATH` is a
//! host with no program of its own, serving clients at the Unix socket
//! PATH. With `MACWL_HOST=PATH` (or `--host PATH`) `macwl-run` is a client:
//! it runs PROGRAM in that host (starting one if none serves PATH) and
//! stands in for it on the Mac: its exit status, signals sent to it, its
//! terminal. Otherwise (or with `--new-host`) `macwl-run` is the host of
//! PROGRAM and everything it starts. `--ps` lists a host's processes.
//!
//! A Linux program may run Mac programs, as WSL's run Windows ones: an
//! `execve` of a Mach-O file runs it natively in the process's place (see
//! `linux_abi::syscall::exec_host`). Off by default for
//! now (ROADMAP.md, item 13). `--mac-programs` (or
//! `MACWL_MAC_PROGRAMS=1`) turns it on.
//!
//! Needs the hypervisor entitlement: `cargo run` signs automatically (see
//! .cargo/config.toml); otherwise use scripts/sign-hv.sh.

use std::{
    ffi::{OsStr, OsString},
    os::{fd::AsFd, unix::ffi::OsStrExt},
    path::PathBuf,
    process::ExitCode,
};

use macwl_runtime::{launch::Launch, Exit};

const USAGE: &str = "usage: macwl-run [--root DIR] [--bind HOST[:GUEST]]... [--mac-programs] [--host PATH | --new-host] PROGRAM [ARGS...]
       macwl-run --serve PATH
       macwl-run --ps [--host PATH]";

struct Options {
    root: Option<PathBuf>,
    /// (host, guest)
    binds: Vec<(PathBuf, OsString)>,
    /// The host to join (`--host`, else MACWL_HOST), unless `--new-host`.
    host: Option<PathBuf>,
    new_host: bool,
    ps: bool,
    args: Vec<OsString>,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options { root: None, binds: Vec::new(), host: None, new_host: false, ps: false, args: Vec::new() };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_bytes() {
            b"--root" => opts.root = Some(args.next().ok_or("--root needs a directory")?.into()),
            b"--bind" => {
                let spec = args.next().ok_or("--bind needs HOST[:GUEST]")?;
                let spec = spec.as_bytes();
                let (host, guest) = match spec.iter().position(|&b| b == b':') {
                    Some(i) => (&spec[..i], &spec[i + 1..]),
                    None => (spec, spec),
                };
                if !guest.starts_with(b"/") {
                    return Err(format!("--bind: the guest path must be absolute: {}", String::from_utf8_lossy(guest)));
                }
                opts.binds.push((OsStr::from_bytes(host).into(), OsStr::from_bytes(guest).into()));
            }
            // Read where execve looks, in this process and the fork
            // children that inherit its environment.
            b"--mac-programs" => unsafe { std::env::set_var("MACWL_MAC_PROGRAMS", "1") },
            b"--host" => opts.host = Some(args.next().ok_or("--host needs a path")?.into()),
            b"--new-host" => opts.new_host = true,
            b"--ps" => opts.ps = true,
            b"--" => {
                opts.args.extend(args);
                break;
            }
            [b'-', b'-', ..] if opts.args.is_empty() => return Err(format!("unknown option {}", arg.display())),
            _ => {
                opts.args.push(arg);
                opts.args.extend(args);
                break;
            }
        }
    }
    if opts.host.is_none() && !opts.new_host {
        opts.host = std::env::var_os("MACWL_HOST").filter(|p| !p.is_empty()).map(PathBuf::from);
    }
    if opts.ps {
        return Ok(opts);
    }
    if opts.args.is_empty() {
        return Err(USAGE.into());
    }
    if opts.root.is_none() && !opts.binds.is_empty() {
        return Err("--bind needs --root (without it the guest sees all host paths)".into());
    }
    Ok(opts)
}

/// Host directories bind-mounted at their own paths under `--root`, so the
/// guest can reach the user's files and talk to the compositor.
fn default_binds() -> Vec<(PathBuf, OsString)> {
    // The Mac's fonts too, for the default fontconfig setup (fonts.conf).
    let mut paths: Vec<PathBuf> = vec!["/tmp".into(), "/nix".into(), "/System/Library/Fonts".into(), "/Library/Fonts".into()];
    paths.extend(std::env::var_os("HOME").map(PathBuf::from));
    paths.extend(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from));
    paths.extend(std::env::current_dir().ok());
    let mut binds: Vec<(PathBuf, OsString)> = Vec::new();
    for p in paths {
        // Skip what doesn't exist, and what an earlier bind already covers.
        if !p.is_dir() || binds.iter().any(|(_, g)| p.starts_with(g)) {
            continue;
        }
        binds.push((p.clone(), p.into_os_string()));
    }
    binds
}

/// Write the default fontconfig setup where guests can read it.
fn default_fonts_conf() -> Option<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/macwl-{}", unsafe { libc::getuid() }));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("fonts.conf");
    let conf = include_str!("../share/fonts.conf");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(conf) {
        std::fs::write(&path, conf).ok()?;
    }
    Some(path)
}

/// What to run, from the options and this process's environment, working
/// directory and umask.
fn launch(opts: &Options) -> Result<Launch, String> {
    let host_root = opts.root.is_none();
    let root = match &opts.root {
        Some(dir) => std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?,
        None => "/".into(),
    };
    let wanted = if host_root { Vec::new() } else { default_binds().into_iter().chain(opts.binds.iter().cloned()).collect() };
    let mut binds = Vec::new();
    for (host, guest) in &wanted {
        // Binds point at real directories: macOS's /tmp is a symlink.
        let target = std::fs::canonicalize(host).map_err(|e| format!("--bind {}: {e}", host.display()))?;
        binds.push((target, guest.as_bytes().to_vec()));
    }
    // The guest starts where it was run from, if it can see that directory.
    let cwd = std::env::current_dir().ok().and_then(|cwd| {
        let visible = host_root || wanted.iter().any(|(_, g)| cwd.starts_with(g));
        let cwd = if host_root { std::fs::canonicalize(&cwd).unwrap_or(cwd) } else { cwd };
        visible.then(|| cwd.into_os_string().into_encoded_bytes())
    });
    // The guest starts with the umask it was run with. (Read by setting it,
    // and back: there's no other way.)
    let umask = unsafe { libc::umask(0o022) };
    unsafe { libc::umask(umask) };
    let argv = opts.args.iter().map(|a| a.as_bytes().to_vec()).collect();
    Ok(Launch { root, binds, cwd, umask: umask as u32, argv, env: environment(), ignored: 0, blocked: 0 })
}

/// A minimal Linux-flavoured environment rather than the Mac's, plus what a
/// Wayland client needs to find the compositor (and D-Bus clients the
/// session bus).
fn environment() -> Vec<Vec<u8>> {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    // HOME is the user's own: visible to the guest either way (bound under
    // --root), and where programs keep their config and caches.
    let home = std::env::var_os("HOME").map_or_else(|| b"/".to_vec(), |h| h.into_encoded_bytes());
    let mut env = vec![b"PATH=/usr/local/bin:/usr/bin:/bin".to_vec(), [b"HOME=", &home[..]].concat(), format!("TERM={term}").into_bytes()];
    for var in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DEBUG", "RUST_LOG", "RUST_BACKTRACE", "LANG", "FONTCONFIG_FILE"] {
        if let Some(value) = std::env::var_os(var) {
            env.push([var.as_bytes(), b"=", value.as_bytes()].concat());
        }
    }
    // A UTF-8 locale every Linux libc has, unless one was asked for.
    if std::env::var_os("LANG").is_none() {
        env.push(b"LANG=C.UTF-8".to_vec());
    }
    // Fonts: the Mac's, through a fontconfig setup of our own (in /tmp, which
    // the guest sees with or without --root), unless one was asked for.
    if std::env::var_os("FONTCONFIG_FILE").is_none()
        && let Some(conf) = default_fonts_conf()
    {
        env.push([&b"FONTCONFIG_FILE="[..], conf.as_os_str().as_bytes()].concat());
    }
    // MACWL_ENV_<NAME>=value sets <NAME> for the guest (replacing the above):
    // without --root, say, PATH=Linux tools, since the Mac's /bin holds Mac
    // programs.
    for (key, value) in std::env::vars_os() {
        if let Some(name) = key.as_bytes().strip_prefix(b"MACWL_ENV_").filter(|n| !n.is_empty()) {
            env.retain(|e| !e.starts_with(name) || e.get(name.len()) != Some(&b'='));
            env.push([name, b"=", value.as_bytes()].concat());
        }
    }
    env
}

/// Report how the guest ended, and exit with the status a Linux shell would
/// show.
fn finish(result: Result<Exit, macwl_runtime::Error>) -> ExitCode {
    match result {
        Ok(exit) => {
            if let Some(what) = exit.describe() {
                eprintln!("macwl-run: {what}");
            }
            macwl_runtime::profile::mark("process exiting");
            ExitCode::from(exit.status() as u8)
        }
        Err(e) => {
            eprintln!("macwl-run: {e}");
            ExitCode::from(2)
        }
    }
}

/// As many host fds as the Mac allows a process: each guest thread has a
/// kqueue (its waker), and a host blocks thousands of threads (a shell's
/// default soft limit is 256).
fn raise_fd_limit() {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    let mut max: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let per_proc = if unsafe { libc::sysctlbyname(c"kern.maxfilesperproc".as_ptr(), (&raw mut max).cast(), &mut len, std::ptr::null_mut(), 0) } == 0 { max as u64 } else { 10240 };
    let want = lim.rlim_max.min(per_proc);
    if want > lim.rlim_cur {
        lim.rlim_cur = want;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    }
}

/// `--serve PATH [--idle-exit]`.
fn serve(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let Some(path) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    // A host a client started goes 2 s after it has nothing to do, whether
    // or not that client ever came.
    let idle_exit = args.next().is_some_and(|a| a == "--idle-exit");
    match macwl_runtime::serve(path.as_ref(), idle_exit) {
        Ok(()) => ExitCode::SUCCESS,
        Err(macwl_runtime::Error::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("macwl-run: {e}");
            ExitCode::from(macwl_runtime::ALREADY_SERVING)
        }
        Err(e) => {
            eprintln!("macwl-run: {e}");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    macwl_runtime::profile::mark("main");
    raise_fd_limit();
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        // A guest process forked by another (see the runtime's fork module).
        Some(a) if a == "--fork-child" => return finish(macwl_runtime::run_fork_child()),
        Some(a) if a == "--serve" => return serve(args),
        _ => {}
    }
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    if opts.ps {
        return match &opts.host {
            Some(host) => macwl_runtime::client::ps(host),
            None => {
                eprintln!("macwl-run: --ps needs a host: --host PATH, or MACWL_HOST");
                ExitCode::from(2)
            }
        };
    }
    let launch = match launch(&opts) {
        Ok(launch) => launch,
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    // A client: the host runs the program (this process never makes a VM).
    if let Some(host) = &opts.host {
        return macwl_runtime::client::run(host, &launch);
    }
    // The host: fds 0–2 its own.
    let stdio: Vec<_> = (0..3).filter_map(|fd| {
        let held = match fd {
            0 => std::io::stdin().as_fd().try_clone_to_owned(),
            1 => std::io::stdout().as_fd().try_clone_to_owned(),
            _ => std::io::stderr().as_fd().try_clone_to_owned(),
        };
        held.ok().map(|h| (fd, h))
    }).collect();
    if stdio.len() < 3 {
        eprintln!("macwl-run: stdin, stdout and stderr must be open");
        return ExitCode::from(2);
    }
    let process = match launch.process(stdio) {
        Ok((process, elf)) => {
            process.set_umask(launch.umask);
            (process, elf)
        }
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    let (process, elf) = process;
    // With many processes in the VM, the terminal it was started in is
    // relayed to a pty of the host's (in 1:1 the Mac's own driver does
    // job control).
    macwl_runtime::relay_terminal();
    let argv: Vec<&[u8]> = launch.argv.iter().map(Vec::as_slice).collect();
    let envp: Vec<&[u8]> = launch.env.iter().map(Vec::as_slice).collect();
    finish(macwl_runtime::run(&elf, &argv, &envp, process))
}
