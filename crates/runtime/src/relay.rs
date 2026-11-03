//! A host started in a Mac terminal relays it (docs/processes.md, section 1,
//! "Its terminal", and section 6): its first process gets a pty of the
//! host's as its controlling terminal in place of the Mac terminal, which is
//! put in raw mode and joined to the pty's master both ways, as `ssh -t`
//! does. So the host does job control for that terminal as for any other of
//! its ptys (foot's): Ctrl-C and Ctrl-Z are bytes, which the host turns
//! into signals for the pty's foreground group; and the Mac shell still
//! sees its job stop when the first process stops, and continue when it
//! says `fg` or `bg`.
//!
//! Two host threads: one carries the Mac terminal's input to the master
//! (through the line discipline's signal characters), one the master's
//! output to the Mac terminal. Neither ever starts a process.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use linux_abi::{fs::FdEntry, Process};

use crate::host::Table;

/// Whether a host may relay the terminal it was started in: `macwl-run`
/// says so; a test harness running hosts in its own process doesn't.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// `macwl-run` hosts relay the Mac terminal they're started in.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(crate) struct Relay {
    /// The pty's number.
    pub n: u32,
    master: OwnedFd,
    /// The Mac terminal, and its own settings, which it gets back whenever
    /// the host stops and when it ends.
    term: OwnedFd,
    saved: libc::termios,
    /// Wake the input thread: b's' the first process stopped, b'q' the host
    /// is done.
    input_ctl: (OwnedFd, OwnedFd),
    /// Wake the output thread: b'q' to copy what's left and finish.
    output_ctl: (OwnedFd, OwnedFd),
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

pub(crate) fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&pair.0, &pair.1] {
        cloexec(fd.as_raw_fd());
    }
    Ok(pair)
}

pub(crate) fn cloexec(fd: RawFd) {
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

pub(crate) fn nonblocking(fd: RawFd) {
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) };
}

/// `open` with `flags`, close-on-exec.
pub(crate) fn open(path: &std::ffi::CStr, flags: i32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn same_terminal(a: RawFd, b: RawFd) -> bool {
    let rdev = |fd: RawFd| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFCHR).then_some(st.st_rdev)
    };
    rdev(a).is_some_and(|d| Some(d) == rdev(b))
}

impl Relay {
    /// If `macwl-run` was started with a terminal as its stdin: a pty in
    /// its place for `process` (its fds 0–2 that were that terminal are
    /// the pty's slave now), with the Mac terminal's settings and size.
    pub fn new(process: &Process) -> Option<Relay> {
        if !ENABLED.load(Ordering::Relaxed) || unsafe { libc::isatty(0) } == 0 {
            return None;
        }
        match Relay::open(process) {
            Ok(relay) => Some(relay),
            Err(e) => {
                eprintln!("macwl-run: no terminal relay ({e}): the program gets the terminal as it is");
                None
            }
        }
    }

    fn open(process: &Process) -> std::io::Result<Relay> {
        let mut name = [0 as libc::c_char; 128];
        if unsafe { libc::ttyname_r(0, name.as_mut_ptr(), name.len()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let term = open(unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }, libc::O_RDWR)?;
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(term.as_raw_fd(), &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pty = Pty::open(&saved, linux_abi::syscall::tty::winsize(term.as_raw_fd()))?;
        let which: Vec<u64> = (0..3u64).filter(|&fd| process.fds.host(fd).is_ok_and(|host| same_terminal(host, term.as_raw_fd()))).collect();
        pty.give(process, &which)?;
        Ok(Relay { n: pty.n, master: pty.master, term, saved, input_ctl: pipe()?, output_ctl: pipe()?, threads: Mutex::new(Vec::new()) })
    }

    /// Start relaying, for the processes of `table`, with the Mac terminal
    /// raw from now on.
    pub fn start(self: &Arc<Relay>, table: &Arc<Table>) {
        self.raw();
        let mut threads = self.threads.lock().unwrap_or_else(|e| e.into_inner());
        let (relay, t) = (self.clone(), table.clone());
        match linux_abi::host_threads::spawn("terminal input", move || relay.input(&t)) {
            Ok(h) => threads.push(h),
            Err(e) => eprintln!("macwl-run: terminal relay: {e}"),
        }
        let relay = self.clone();
        match linux_abi::host_threads::spawn("terminal output", move || relay.output()) {
            Ok(h) => threads.push(h),
            Err(e) => eprintln!("macwl-run: terminal relay: {e}"),
        }
    }

    /// The Mac terminal in raw mode: every byte to the pty as typed.
    fn raw(&self) {
        let mut raw = self.saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(self.term.as_raw_fd(), libc::TCSANOW, &raw) };
    }

    /// The Mac terminal as it was.
    fn restore(&self) {
        unsafe { libc::tcsetattr(self.term.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }

    /// The first process has stopped (the table says, holding none of its
    /// locks).
    pub fn first_stopped(&self) {
        unsafe { libc::write(self.input_ctl.1.as_raw_fd(), b"s".as_ptr().cast(), 1) };
    }

    /// The Mac terminal's size changed (SIGWINCH): the pty's too, and its
    /// foreground group hears of it.
    pub fn resize(&self, table: &Table) {
        if let Some(ws) = linux_abi::syscall::tty::winsize(self.term.as_raw_fd()) {
            table.resize_tty(self.n, self.master.as_raw_fd(), ws);
        }
    }

    /// The host is done: the rest of the output goes to the Mac terminal,
    /// which gets its settings back.
    pub fn finish(&self) {
        unsafe { libc::write(self.output_ctl.1.as_raw_fd(), b"q".as_ptr().cast(), 1) };
        unsafe { libc::write(self.input_ctl.1.as_raw_fd(), b"q".as_ptr().cast(), 1) };
        let threads = std::mem::take(&mut *self.threads.lock().unwrap_or_else(|e| e.into_inner()));
        for t in threads {
            let _ = t.join();
        }
        self.restore();
    }

    /// Mac terminal → master, until the host is done.
    fn input(&self, table: &Table) {
        let (term, master, ctl) = (self.term.as_raw_fd(), self.master.as_raw_fd(), self.input_ctl.0.as_raw_fd());
        let mut buf = [0u8; 4096];
        // Typed bytes the pty hasn't taken yet (it's full: no one reads it).
        let mut pending: Vec<u8> = Vec::new();
        let mut term_open = true;
        loop {
            let mut fds = [
                libc::pollfd { fd: ctl, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: if pending.is_empty() && term_open { term } else { -1 }, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: if pending.is_empty() { -1 } else { master }, events: libc::POLLOUT, revents: 0 },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) } < 0 {
                continue;
            }
            if fds[0].revents != 0 {
                let mut what = [0u8; 1];
                if unsafe { libc::read(ctl, what.as_mut_ptr().cast(), 1) } == 1 {
                    match what[0] {
                        b'q' => return,
                        b's' => self.stop_with_first(table),
                        _ => {}
                    }
                }
                continue;
            }
            if fds[1].revents != 0 {
                let n = unsafe { libc::read(term, buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    pending.extend_from_slice(&buf[..n as usize]);
                } else if n == 0 || !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                    // The Mac terminal has gone: the pty hangs up.
                    term_open = false;
                    table.hangup(self.n);
                    continue;
                }
            }
            if !pending.is_empty() {
                let r = table.type_keys(self.n, master, &pending).unwrap_or_else(|| {
                    let n = unsafe { libc::write(master, pending.as_ptr().cast(), pending.len()) };
                    if n < 0 { Err(linux_abi::errno::from_darwin(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))) } else { Ok(n as i64) }
                });
                match r {
                    Ok(n) => {
                        pending.drain(..n as usize);
                    }
                    Err(e) if e == linux_abi::errno::EAGAIN || e == linux_abi::errno::EINTR => {}
                    // Nothing has the slave open: what's typed goes nowhere.
                    Err(_) => pending.clear(),
                }
            }
        }
    }

    /// The first process stopped: the Mac terminal gets its settings back
    /// and the host stops, so the Mac shell sees its job stop. Continued
    /// (`fg`, `bg`), the terminal is raw again and the first process's
    /// group continues.
    fn stop_with_first(&self, table: &Table) {
        // What the program wrote before it stopped (a full-screen one
        // putting the screen back) goes out first.
        drained(self.master.as_raw_fd());
        self.restore();
        // A stop of the whole Mac process, as any job's; the call returns
        // once it's continued.
        unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
        self.raw();
        self.resize(table);
        table.continue_first();
    }

    /// Master → Mac terminal, until the host is done.
    fn output(&self) {
        pump_output(self.master.as_raw_fd(), self.term.as_raw_fd(), self.output_ctl.0.as_raw_fd());
    }
}

/// A pty of the host's for a process in place of a Mac terminal: its
/// master, kept by the host (and passed to a client to read), and its slave.
pub(crate) struct Pty {
    /// Its number.
    pub n: u32,
    pub master: OwnedFd,
    slave: OwnedFd,
    slave_path: Vec<u8>,
}

impl Pty {
    /// A new pty, set up as the Mac terminal it stands for is: its line
    /// discipline settings (`termios`: a cooked terminal, as the shell left
    /// it) and its size. The master is non-blocking.
    pub fn open(termios: &libc::termios, size: Option<libc::winsize>) -> std::io::Result<Pty> {
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        cloexec(master.as_raw_fd());
        if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut slave_name = [0 as libc::c_char; 128];
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCPTYGNAME as u64, slave_name.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave_path = unsafe { std::ffi::CStr::from_ptr(slave_name.as_ptr()) };
        let slave = open(slave_path, libc::O_RDWR)?;
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, termios) };
        if let Some(ws) = size {
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        }
        let n = linux_abi::syscall::tty_of(master.as_raw_fd()).ok_or_else(|| std::io::Error::other("not a pty"))?.n;
        nonblocking(master.as_raw_fd());
        Ok(Pty { n, master, slave, slave_path: slave_path.to_bytes().to_vec() })
    }

    /// `process`'s fds `which` (those that were the Mac terminal) are the
    /// slave from now on, one open file, as a login sets a terminal up.
    pub fn give(&self, process: &Process, which: &[u64]) -> std::io::Result<()> {
        let mut shared = None;
        for &fd in which {
            match shared {
                None => {
                    process.fds.insert_at(fd as usize, FdEntry::new(self.slave.try_clone()?).path(self.slave_path.clone()));
                    shared = Some(fd);
                }
                Some(first) => {
                    let _ = process.fds.dup_to(first, fd as usize, false);
                }
            }
        }
        Ok(())
    }
}

/// Wait (a little) for what's in `master` to have been read: what a program
/// wrote before it stopped (a full-screen one putting the screen back)
/// goes out before the Mac terminal gets its settings back.
pub(crate) fn drained(master: RawFd) {
    for _ in 0..50 {
        let mut n: libc::c_int = 0;
        if unsafe { libc::ioctl(master, libc::FIONREAD, &mut n) } != 0 || n == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Master → Mac terminal, until a byte comes on `ctl`: then what's left,
/// and done.
pub(crate) fn pump_output(master: RawFd, term: RawFd, ctl: RawFd) {
    let mut buf = [0u8; 16384];
    let mut hung_up = false;
    loop {
        let mut fds = [
            libc::pollfd { fd: ctl, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if hung_up { -1 } else { master }, events: libc::POLLIN, revents: 0 },
        ];
        // With no slave open, the master reads as hung up: look again
        // now and then, for a process that opens the terminal again.
        let timeout = if hung_up { 50 } else { -1 };
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) } < 0 {
            continue;
        }
        let done = fds[0].revents != 0;
        hung_up = !copy(master, term, &mut buf);
        if done {
            return;
        }
    }
}

/// Copy what the master has to the Mac terminal; false if it reads as hung
/// up (no slave open).
fn copy(master: RawFd, term: RawFd, buf: &mut [u8]) -> bool {
    loop {
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let mut at = 0;
            while at < n as usize {
                let w = unsafe { libc::write(term, buf[at..n as usize].as_ptr().cast(), n as usize - at) };
                if w <= 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return true;
                }
                at += w as usize;
            }
            continue;
        }
        let e = std::io::Error::last_os_error().raw_os_error();
        return n < 0 && matches!(e, Some(libc::EAGAIN | libc::EINTR));
    }
}
