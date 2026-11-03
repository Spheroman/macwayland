//! A client: a `macwl-run` that joins a host (`MACWL_HOST=PATH`, or
//! `--host PATH`) rather than being one, as `docker exec` joins a
//! container (docs/processes.md, section 1, "How a `macwl-run` finds its
//! host"). It never makes a VM: it sends the host what to run, and stands
//! in on the Mac for the Linux process the host starts.
//!
//! - **Its fds** 0–2 go to the host (`SCM_RIGHTS`) and are the process's
//!   own: no copying, and no latency, between the process and a pipe or
//!   file the client was given.
//! - **Its terminal**, if stdin is one: the host gives the process a pty of
//!   its own instead (its controlling terminal, whose job control the host
//!   does) and passes the client the master. The client reads the master
//!   straight to the Mac terminal, and sends what's typed through the host,
//!   which raises the keyboard's signals (Ctrl-C, Ctrl-Z) for the pty's
//!   foreground group; the Mac terminal is raw meanwhile, as with `ssh -t`.
//! - **Signals from the Mac side** (`kill`, or Ctrl-C without a terminal
//!   relay) go to the process's group; SIGWINCH is the pty's resize and
//!   SIGHUP its hangup. SIGKILL ends the client, which the host takes as a
//!   hangup.
//! - **Stops**: when the process stops, the client gives the Mac terminal
//!   its settings back and stops itself, so the Mac shell sees its job
//!   stop; continued, it continues the process.
//! - **Its exit** is the process's: the code, or the signal, which it
//!   raises on itself (as a shell expects of a job killed by one), except
//!   those that would dump core, for which it exits with 128 + the signal.
//!
//! When no host serves the path, the client starts one (`macwl-run --serve
//! PATH --idle-exit`, a plain `posix_spawn` of a new process in a session
//! of its own) and joins it.

use std::{
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{fs::DirBuilderExt, net::UnixStream},
    },
    path::Path,
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use linux_abi::{
    syscall::{signal_from_darwin, signal_to_darwin},
    wire::{Decoder, Encoder},
};

use crate::{launch::Launch, proto, relay, Exit};

/// The Mac side's signals the client passes on: every catchable one but
/// those of its own faults, and SIGTTIN and SIGTTOU, which stop the client
/// itself (as they would any program) when it touches its terminal from
/// the background.
const FORWARDED: [libc::c_int; 16] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGTERM,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGWINCH,
    libc::SIGVTALRM,
    libc::SIGPROF,
    libc::SIGXCPU,
    libc::SIGXFSZ,
    libc::SIGURG,
    libc::SIGIO,
];

/// How long a client waits for a host to answer at the path (one it
/// started included).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The Mac terminal the client was started in.
struct Terminal {
    fd: OwnedFd,
    saved: libc::termios,
    /// Which of fds 0–2 are it (a bit each).
    which: u64,
}

impl Terminal {
    fn open() -> Option<Terminal> {
        if unsafe { libc::isatty(0) } == 0 {
            return None;
        }
        let mut name = [0 as libc::c_char; 128];
        if unsafe { libc::ttyname_r(0, name.as_mut_ptr(), name.len()) } != 0 {
            return None;
        }
        let fd = relay::open(unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }, libc::O_RDWR).ok()?;
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut saved) } != 0 {
            return None;
        }
        let which = (0..3).filter(|&n| relay::same_terminal(n, fd.as_raw_fd())).fold(0, |w, n| w | 1 << n);
        Some(Terminal { fd, saved, which })
    }

    fn raw(&self) {
        let mut raw = self.saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &raw) };
    }

    fn restore(&self) {
        unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }

    /// Whether the client is the Mac terminal's foreground job.
    fn foreground(&self) -> bool {
        unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) == libc::getpgrp() }
    }
}

/// What the client's threads share.
struct Shared {
    sock: UnixStream,
    /// One message at a time on the socket.
    sending: Mutex<()>,
    term: Option<Terminal>,
    /// The pty's master, read to the Mac terminal.
    master: Option<OwnedFd>,
    /// The client is in the Mac terminal's foreground, and reads it.
    reading: AtomicBool,
    /// Wake the input thread: b'q' to finish, else to look again.
    input_ctl: (OwnedFd, OwnedFd),
    /// Wake the output thread: b'q' to copy what's left and finish.
    output_ctl: (OwnedFd, OwnedFd),
}

impl Shared {
    fn send(&self, kind: u8, body: &[u8]) {
        let _one = self.sending.lock().unwrap_or_else(|e| e.into_inner());
        let _ = proto::send(self.sock.as_raw_fd(), kind, body, &[]);
    }

    fn wake_input(&self, what: u8) {
        unsafe { libc::write(self.input_ctl.1.as_raw_fd(), [what].as_ptr().cast(), 1) };
    }

    /// The terminal is raw and read again, if the client is in its
    /// foreground (after a start or a continue).
    fn take_terminal(&self) {
        if let Some(term) = &self.term
            && term.foreground()
        {
            term.raw();
            self.reading.store(true, Ordering::Release);
            self.wake_input(b'r');
        }
    }

    /// The terminal as the Mac shell left it, and not read (before a stop
    /// or the end).
    fn give_terminal_back(&self) {
        if let Some(term) = &self.term {
            self.reading.store(false, Ordering::Release);
            self.wake_input(b'r');
            if let Some(master) = &self.master {
                relay::drained(master.as_raw_fd());
            }
            term.restore();
        }
    }

    /// Mac terminal → host, while the client is in its foreground.
    fn input(&self) {
        let (Some(term), ctl) = (&self.term, self.input_ctl.0.as_raw_fd()) else { return };
        let mut buf = [0u8; 4096];
        let mut open = true;
        loop {
            let reading = open && self.reading.load(Ordering::Acquire);
            let mut fds = [
                libc::pollfd { fd: ctl, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: if reading { term.fd.as_raw_fd() } else { -1 }, events: libc::POLLIN, revents: 0 },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                continue;
            }
            if fds[0].revents != 0 {
                let mut what = [0u8; 1];
                if unsafe { libc::read(ctl, what.as_mut_ptr().cast(), 1) } == 1 && what[0] == b'q' {
                    return;
                }
                continue;
            }
            if fds[1].revents != 0 && self.reading.load(Ordering::Acquire) {
                let n = unsafe { libc::read(term.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    self.send(proto::INPUT, &buf[..n as usize]);
                } else if n == 0 || !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                    // The Mac terminal has gone: the pty hangs up.
                    open = false;
                    self.send(proto::HANGUP, &[]);
                }
            }
        }
    }

    /// The Mac side's signals, passed on.
    fn signals(&self) {
        let set = signal_set(&FORWARDED);
        loop {
            let mut sig = 0;
            if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                continue;
            }
            match sig {
                libc::SIGWINCH if self.term.is_some() => {
                    if let Some(ws) = linux_abi::syscall::tty::winsize(self.term.as_ref().unwrap().fd.as_raw_fd()) {
                        let mut e = Encoder::default();
                        proto::encode_winsize(&mut e, ws);
                        self.send(proto::RESIZE, &e.0);
                    }
                }
                libc::SIGHUP if self.term.is_some() => self.send(proto::HANGUP, &[]),
                _ => {
                    // Continued (`fg`, `bg`): the terminal is the process's
                    // again if the client is in the foreground, and the
                    // process continues either way.
                    if sig == libc::SIGCONT {
                        self.take_terminal();
                    }
                    let mut e = Encoder::default();
                    e.u64(signal_from_darwin(sig));
                    self.send(proto::SIGNAL, &e.0);
                }
            }
        }
    }
}

/// The signals this process ignores and blocks, as Linux sets.
fn inherited_signals() -> (u64, u64) {
    let mut blocked: libc::sigset_t = 0;
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut blocked) };
    let (mut ignored_set, mut blocked_set) = (0u64, 0u64);
    for darwin in 1..32 {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let linux = signal_from_darwin(darwin);
        // (Darwin's own, SIGEMT and SIGINFO, have no Linux number.)
        if signal_to_darwin(linux) != Some(darwin) {
            continue;
        }
        if unsafe { libc::sigaction(darwin, std::ptr::null(), &mut old) } == 0 && old.sa_sigaction == libc::SIG_IGN {
            ignored_set |= 1 << (linux - 1);
        }
        if unsafe { libc::sigismember(&blocked, darwin) } == 1 {
            blocked_set |= 1 << (linux - 1);
        }
    }
    (ignored_set, blocked_set)
}

fn signal_set(sigs: &[libc::c_int]) -> libc::sigset_t {
    let mut set: libc::sigset_t = 0;
    unsafe { libc::sigemptyset(&mut set) };
    for &s in sigs {
        unsafe { libc::sigaddset(&mut set, s) };
    }
    set
}

/// Join the host at `path` (starting one if none serves it) and run
/// `launch` there: the process's exit status, as a Mac shell expects it.
pub fn run(path: &Path, launch: &Launch) -> ExitCode {
    // The process starts with the signals this one was started with
    // ignored and blocked, as if it had been exec'd in its place.
    let mut launch = launch.clone();
    (launch.ignored, launch.blocked) = inherited_signals();
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let term = Terminal::open();
    let which = term.as_ref().map_or(0, |t| t.which);
    // Its fds 0–2, those that aren't the terminal and are open.
    let stdio: Vec<RawFd> = (0..3).filter(|&fd| which & (1 << fd) == 0 && unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0).collect();
    let mut e = Encoder::default();
    launch.encode(&mut e);
    e.u64(stdio.iter().fold(0, |w, &fd| w | 1 << fd));
    e.bool(term.is_some());
    if let Some(t) = &term {
        proto::encode_terminal(&mut e, &t.saved, linux_abi::syscall::tty::winsize(t.fd.as_raw_fd()));
        e.u64(t.which);
    }
    // The signals the client passes on wait for its signal thread, in
    // every thread from the start.
    let set = signal_set(&FORWARDED);
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    // Those whose default is to ignore them (SIGWINCH, SIGURG, SIGIO) are
    // dropped as they're sent, blocked or not, unless something catches
    // them: a handler that's never run (they stay blocked) keeps them for
    // the signal thread. Those this process was started ignoring stay so.
    extern "C" fn never(_: libc::c_int) {}
    for sig in FORWARDED {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } == 0 && old.sa_sigaction == libc::SIG_DFL {
            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            sa.sa_sigaction = never as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) };
        }
    }

    let (sock, mut reader, started) = match start(path, &e.0, &stdio) {
        Ok(s) => s,
        Err(why) => {
            eprintln!("macwl-run: {why}");
            return ExitCode::from(2);
        }
    };
    let master = started.fds.into_iter().next();
    let shared = Arc::new(Shared {
        sock,
        sending: Mutex::new(()),
        term,
        master,
        reading: AtomicBool::new(false),
        input_ctl: relay::pipe().expect("a pipe"),
        output_ctl: relay::pipe().expect("a pipe"),
    });
    let mut output = None;
    if let (Some(term), Some(master)) = (&shared.term, &shared.master) {
        let (master, term, ctl) = (master.as_raw_fd(), term.fd.as_raw_fd(), shared.output_ctl.0.as_raw_fd());
        output = linux_abi::host_threads::spawn("terminal output", move || relay::pump_output(master, term, ctl)).ok();
        let s = shared.clone();
        let _ = linux_abi::host_threads::spawn("terminal input", move || s.input());
        shared.take_terminal();
    }
    let s = shared.clone();
    let _ = linux_abi::host_threads::spawn("signals", move || s.signals());

    loop {
        let Some(msg) = reader.recv(shared.sock.as_raw_fd()) else {
            finish_terminal(&shared, output.take());
            eprintln!("macwl-run: the host has gone");
            return ExitCode::from(128 + 9);
        };
        let mut d = Decoder(&msg.body);
        match msg.kind {
            proto::STOPPED => {
                shared.give_terminal_back();
                // A stop of the client, as of any job; it's continued by
                // the Mac shell (`fg`, `bg`), or by the host when the
                // process is continued from inside.
                unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
            }
            proto::EXITED => {
                let Ok(exit) = proto::decode_exit(&mut d) else { continue };
                finish_terminal(&shared, output.take());
                return exit_as(exit);
            }
            _ => {}
        }
    }
}

/// The process has ended (or the host has): the rest of the pty's output
/// to the Mac terminal, which gets its settings back.
fn finish_terminal(shared: &Shared, output: Option<std::thread::JoinHandle<()>>) {
    if let Some(output) = output {
        unsafe { libc::write(shared.output_ctl.1.as_raw_fd(), b"q".as_ptr().cast(), 1) };
        let _ = output.join();
    }
    shared.wake_input(b'q');
    if let Some(term) = &shared.term {
        term.restore();
    }
}

/// Exit as the process did: its code, or its signal raised on the client.
fn exit_as(exit: Result<Exit, String>) -> ExitCode {
    let exit = match exit {
        Ok(exit) => exit,
        Err(why) => {
            eprintln!("macwl-run: {why}");
            Exit::Signal(9)
        }
    };
    if let Some(what) = exit.describe() {
        eprintln!("macwl-run: {what}");
    }
    let Some(sig) = exit.signal() else { return ExitCode::from(exit.status() as u8) };
    // Signals whose default would dump core (a crash report for the
    // client, not the program) or that can't end a process are a status.
    let dumps_core = matches!(sig, 3 | 4 | 5 | 6 | 7 | 8 | 11 | 31);
    let ends = !matches!(sig, 17..=23 | 28 | 29);
    if let Some(darwin) = signal_to_darwin(sig as u64).filter(|_| !dumps_core && ends) {
        unsafe {
            libc::signal(darwin, libc::SIG_DFL);
            let set = signal_set(&[darwin]);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            // This thread's: the signal thread mustn't take it.
            libc::pthread_kill(libc::pthread_self(), darwin);
        }
    }
    ExitCode::from(exit.status() as u8)
}

/// Connect to the host at `path` and hand it the run: the socket, what it
/// has sent since, and its STARTED message. A host that goes before it
/// answers (it was just ending) is tried again, or another started.
fn start(path: &Path, body: &[u8], stdio: &[RawFd]) -> Result<(UnixStream, proto::Reader, proto::Msg), String> {
    for _ in 0..5 {
        let sock = connect(path, true)?;
        if proto::send(sock.as_raw_fd(), proto::RUN, body, stdio).is_err() {
            continue;
        }
        let mut reader = proto::Reader::default();
        match reader.recv(sock.as_raw_fd()) {
            Some(msg) if msg.kind == proto::STARTED => return Ok((sock, reader, msg)),
            Some(msg) if msg.kind == proto::FAILED => return Err(String::from_utf8_lossy(&msg.body).into_owned()),
            _ => continue,
        }
    }
    Err(format!("the host at {} keeps going away", path.display()))
}

/// A connection to the host at `path`, which is the user's own; if none
/// serves it, one started (`start_host`) or an error.
fn connect(path: &Path, start_host: bool) -> Result<UnixStream, String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut started: Option<libc::pid_t> = None;
    let mut wait_until = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(sock) => {
                if proto::peer_uid(&sock) != Some(unsafe { libc::getuid() }) {
                    return Err(format!("{} is another user's", path.display()));
                }
                return Ok(sock);
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        if !start_host {
            return Err(format!("no host at {}", path.display()));
        }
        if let Some(pid) = started {
            let mut st = 0;
            if unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid {
                started = None;
                // Another client's host won the path: wait for it.
                if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == crate::host::ALREADY_SERVING as i32 {
                    wait_until = Instant::now() + Duration::from_millis(500);
                } else {
                    return Err(format!("the host for {} didn't start (see {}.log)", path.display(), path.display()));
                }
            }
        }
        if started.is_none() && Instant::now() >= wait_until {
            started = Some(spawn_host(path).map_err(|e| format!("starting a host for {}: {e}", path.display()))?);
        }
        if Instant::now() > deadline {
            return Err(format!("no host answered at {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `posix_spawn` flags macOS has that the libc crate doesn't name.
const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;

/// Start `macwl-run --serve PATH --idle-exit`: a new process (never this
/// one replaced), in a session of its own, with no terminal, its output
/// to PATH.log.
fn spawn_host(path: &Path) -> std::io::Result<libc::pid_t> {
    // The host makes the VM; a client never has one.
    assert!(!crate::hv::vm_tried(), "a process that made a VM starting a host");
    use std::os::unix::ffi::OsStrExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let exe = std::env::current_exe()?;
    let cstr = |b: &[u8]| std::ffi::CString::new(b).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let exe = cstr(exe.as_os_str().as_bytes())?;
    let path_c = cstr(path.as_os_str().as_bytes())?;
    let log = cstr(&[path.as_os_str().as_bytes(), b".log"].concat())?;
    let args = [exe.clone(), cstr(b"--serve")?, path_c, cstr(b"--idle-exit")?];
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|a| a.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());
    unsafe extern "C" {
        static environ: *const *mut libc::c_char;
    }
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        libc::posix_spawnattr_init(&mut attr);
        libc::posix_spawn_file_actions_init(&mut actions);
        // A plain spawn: no POSIX_SPAWN_SETEXEC. Only the fds below go to
        // it, and its signals start as a new program's.
        let flags = POSIX_SPAWN_SETSID
            | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short
            | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
            | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short;
        libc::posix_spawnattr_setflags(&mut attr, flags);
        let empty = signal_set(&[]);
        libc::posix_spawnattr_setsigmask(&mut attr, &empty);
        let mut all: libc::sigset_t = 0;
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(&mut attr, &all);
        libc::posix_spawn_file_actions_addopen(&mut actions, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
        libc::posix_spawn_file_actions_addopen(&mut actions, 1, c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
        libc::posix_spawn_file_actions_addopen(&mut actions, 2, log.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o600);
        let mut pid = 0;
        let r = libc::posix_spawn(&mut pid, exe.as_ptr(), &actions, &attr, argv.as_ptr(), environ);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r));
        }
        Ok(pid)
    }
}

/// `macwl-run --ps`: the processes of the host at `path`.
pub fn ps(path: &Path) -> ExitCode {
    let sock = match connect(path, false) {
        Ok(sock) => sock,
        Err(why) => {
            eprintln!("macwl-run: {why}");
            return ExitCode::from(1);
        }
    };
    if proto::send(sock.as_raw_fd(), proto::PS, &[], &[]).is_ok()
        && let Some(msg) = proto::Reader::default().recv(sock.as_raw_fd())
        && msg.kind == proto::LISTING
    {
        print!("{}", String::from_utf8_lossy(&msg.body));
        return ExitCode::SUCCESS;
    }
    eprintln!("macwl-run: the host at {} didn't answer", path.display());
    ExitCode::from(1)
}
