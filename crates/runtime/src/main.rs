//! `macwl-run [--root DIR] [--bind HOST[:GUEST]]... [--mac-programs] PROGRAM [ARGS...]`
//!
//! Runs an arm64 Linux program. Without `--root` the program sees the Mac's
//! own filesystem, which is enough for programs from Nix (their libraries are
//! `/nix/store` paths). With `--root`, DIR is the guest's `/` — an extracted
//! Linux distribution, say — and some host directories are bind-mounted into
//! it at their own paths: the home directory, `/nix`, `/tmp`, the Wayland
//! runtime directory and the current directory. `--bind` adds more. PROGRAM
//! is a guest path.
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
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::ExitCode,
};

use linux_abi::{
    fs::{Resolved, Target},
    Process,
};
use macwl_runtime::Exit;

const USAGE: &str = "usage: macwl-run [--root DIR] [--bind HOST[:GUEST]]... [--mac-programs] PROGRAM [ARGS...]";

struct Options {
    root: Option<PathBuf>,
    /// (host, guest)
    binds: Vec<(PathBuf, OsString)>,
    args: Vec<OsString>,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options { root: None, binds: Vec::new(), args: Vec::new() };
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

fn setup(opts: &Options) -> Result<Process, String> {
    let host_root = opts.root.is_none();
    let root = match &opts.root {
        Some(dir) => std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?,
        None => "/".into(),
    };
    let mut process = Process::with_host_stdio(&root).map_err(|e| e.to_string())?;
    let binds = if host_root { Vec::new() } else { default_binds().into_iter().chain(opts.binds.iter().cloned()).collect() };
    for (host, guest) in &binds {
        // Binds point at real directories: macOS's /tmp is a symlink.
        let target = std::fs::canonicalize(host).map_err(|e| format!("--bind {}: {e}", host.display()))?;
        process.fs.bind(guest.as_bytes(), target).map_err(|e| format!("--bind {}: {e}", host.display()))?;
    }
    // The guest starts where it was run from, if it can see that directory.
    if let Ok(cwd) = std::env::current_dir() {
        let visible = host_root || binds.iter().any(|(_, g)| cwd.starts_with(g));
        let cwd = if host_root { std::fs::canonicalize(&cwd).unwrap_or(cwd) } else { cwd };
        if visible {
            process.fs.set_cwd(cwd.into_os_string().into_encoded_bytes());
        }
    }
    Ok(process)
}

/// The program, as a guest path: its host file and contents.
fn read_program(process: &Process, path: &OsStr) -> Result<(Vec<u8>, Vec<u8>), String> {
    const AT_FDCWD: u64 = -100i64 as u64;
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let host = match process.fs.resolve(&process.fds, AT_FDCWD, path.as_bytes(), true) {
        Ok(Resolved { target: Target::Host(host), .. }) => host,
        Ok(_) => return Err(err(std::io::ErrorKind::Unsupported.into())),
        Err(e) => return Err(err(std::io::Error::from_raw_os_error(e as i32))),
    };
    let host = Path::new(OsStr::from_bytes(host.as_bytes()));
    let elf = std::fs::read(host).map_err(err)?;
    // /proc/self/exe: the guest's name for the file, symlinks resolved.
    let canonical = std::fs::canonicalize(host).map_err(err)?;
    let exe = process.fs.guest_path_of_host(canonical.as_os_str().as_bytes());
    Ok((elf, exe))
}

/// Report how the guest ended, and exit with the status a Linux shell would
/// show.
fn finish(result: Result<Exit, macwl_runtime::Error>) -> ExitCode {
    match result {
        Ok(exit) => {
            match exit {
                Exit::Fault { sig, esr, far, pc } => {
                    eprintln!("macwl-run: guest fault (signal {sig}) at pc {pc:#x}: ESR {esr:#x}, FAR {far:#x}")
                }
                Exit::BusError { far, pc } => eprintln!("macwl-run: bus error at pc {pc:#x}: address {far:#x}"),
                Exit::OutOfMemory => eprintln!("macwl-run: out of guest memory"),
                Exit::Signal(_) | Exit::Code(_) => {}
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

fn main() -> ExitCode {
    macwl_runtime::profile::mark("main");
    // A guest process forked by another (see the runtime's fork module).
    if std::env::args_os().nth(1).is_some_and(|a| a == "--fork-child") {
        return finish(macwl_runtime::run_fork_child());
    }
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    let process = match setup(&opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    let elf = match read_program(&process, &opts.args[0]) {
        Ok((elf, exe)) => {
            process.set_exe(exe);
            elf
        }
        Err(e) => {
            eprintln!("macwl-run: {e}");
            return ExitCode::from(2);
        }
    };
    let argv: Vec<&[u8]> = opts.args.iter().map(|a| a.as_bytes()).collect();
    // A minimal Linux-flavoured environment rather than the Mac's, plus what
    // a Wayland client needs to find the compositor (and D-Bus clients the
    // session bus).
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
    let envp: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();

    finish(macwl_runtime::run(&elf, &argv, &envp, &process))
}
