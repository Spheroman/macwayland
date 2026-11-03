//! A host serving clients (docs/processes.md, section 1, "How a `macwl-run`
//! finds its host", and step 5): `macwl-run --serve PATH` is a host with
//! no program of its own, listening on the Unix socket at PATH; a
//! `macwl-run` with `MACWL_HOST=PATH` is a client, which hands the host a
//! program to run and stands in for its Linux process on the Mac.
//!
//! One host thread (the one `serve` is called on) serves every client: it
//! accepts them, starts their processes, types what their terminals send
//! into their ptys, routes the signals they forward, and tells them when
//! their processes stop, continue and end. A client costs the host no
//! thread of its own, and the number of clients is bounded.
//!
//! The socket is the user's alone: its directory is made mode 700 if the
//! host makes it, the socket is mode 600, and a client whose uid isn't the
//! host's is turned away (`getpeereid`). A lock file beside it (PATH.lock)
//! makes sure one host serves a path at a time.
//!
//! A host lives while it has processes or clients, and goes 2 s after the
//! last of them (once its first client has come, or at once for a host a
//! client started): a daemon (wineserver) keeps it, and a quick run of
//! `macwl-run`s, or a restarted `session.sh`, finds it still there.

use std::{
    io::ErrorKind,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{fs::DirBuilderExt, net::{UnixListener, UnixStream}},
    },
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use linux_abi::{
    errno,
    syscall::{SigInfo, Thread},
    wire::{Decoder, Encoder, Truncated},
};

use super::{route_host_signals, Kind, RangePids, Table, TableKernel, Target, SIGCHLD, SIGCONT, SIGHUP, SI_KERNEL, SI_USER};
use crate::{
    hv,
    launch::Launch,
    load,
    memory::{Machine, Space},
    place, proto, relay,
    sched::{Sched, Start, Task},
    signal, Error, Exit, Host, Proc, Scope,
};

/// How long a host with no processes and no clients waits for another
/// before it goes.
const GRACE: Duration = Duration::from_secs(2);

/// The most clients a host serves at once (each costs it a socket, no
/// thread).
const MAX_CLIENTS: usize = 256;

/// What the process table tells the host's service of its clients'
/// processes.
#[derive(Debug)]
pub(crate) enum Event {
    /// Stopped by this signal.
    Stopped(i64, u64),
    Continued(i64),
    Exited(i64, Result<Exit, String>),
    /// The host's last process has gone.
    NoProcesses,
}

/// Events for the service, and a pipe that wakes it for them.
pub(crate) struct Notify {
    events: Mutex<Vec<Event>>,
    wake: (OwnedFd, OwnedFd),
}

impl Notify {
    fn new() -> std::io::Result<Notify> {
        let wake = relay::pipe()?;
        relay::nonblocking(wake.0.as_raw_fd());
        relay::nonblocking(wake.1.as_raw_fd());
        Ok(Notify { events: Mutex::default(), wake })
    }

    /// Tell the service of `event` (from any thread, holding no lock of
    /// the table's).
    pub fn post(&self, event: Event) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
        unsafe { libc::write(self.wake.1.as_raw_fd(), b"!".as_ptr().cast(), 1) };
    }

    fn take(&self) -> Vec<Event> {
        let mut buf = [0u8; 256];
        while unsafe { libc::read(self.wake.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The socket a host serves, and the lock that makes it this host's.
struct Endpoint {
    path: PathBuf,
    listener: Option<UnixListener>,
    lock: Option<OwnedFd>,
}

/// Another host already serves the path (`macwl-run --serve` exits with
/// this status then).
pub const ALREADY_SERVING: u8 = 3;

impl Endpoint {
    fn claim(path: &Path) -> std::io::Result<Endpoint> {
        let me = unsafe { libc::getuid() };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            // Directories the host makes are the user's alone.
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(dir)?.uid() != me {
                return Err(std::io::Error::new(ErrorKind::PermissionDenied, format!("{} isn't yours", dir.display())));
            }
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path)?;
        // A host that's just going still holds it for a moment.
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if Instant::now() > deadline {
                return Err(std::io::Error::new(ErrorKind::AddrInUse, format!("a host already serves {}", path.display())));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Holding the lock, whatever is at the path is a host's that has
        // gone.
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Endpoint { path: path.to_owned(), listener: Some(listener), lock: Some(lock.into()) })
    }

    /// No more clients: the path goes, then the lock, so that a new host
    /// can serve the path at once.
    fn close(&mut self) {
        if self.listener.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
        self.lock = None;
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.close();
    }
}

/// A client's terminal: a pty of the host's whose slave its process has.
/// The client reads the master's output itself (it has the master too);
/// what's typed comes through the host, which raises the keyboard's
/// signals.
struct ClientTty {
    n: u32,
    master: OwnedFd,
    /// Typed bytes the pty hasn't taken yet.
    typed: Vec<u8>,
}

struct Client {
    sock: UnixStream,
    reader: proto::Reader,
    /// Its Mac pid, to continue it when its process is continued.
    mac_pid: Option<libc::pid_t>,
    /// Its process, from when it starts until it has ended.
    pid: Option<i64>,
    /// It has asked for a process (a listing doesn't count).
    ran: bool,
    /// A `--ps` query, which goes once answered.
    listing: bool,
    tty: Option<ClientTty>,
    /// It was told its process stopped (and so stopped itself).
    told_stopped: bool,
    gone: bool,
}

impl Client {
    /// Whether it keeps the host alive.
    fn counts(&self) -> bool {
        !self.listing
    }

    fn send(&mut self, kind: u8, body: &[u8], fds: &[RawFd]) {
        if !self.gone && proto::send(self.sock.as_raw_fd(), kind, body, fds).is_err() {
            self.gone = true;
        }
    }
}

struct Service {
    table: Arc<Table>,
    notify: Arc<Notify>,
    endpoint: Endpoint,
    clients: Vec<Client>,
    /// Its lifetime has begun: its first client came (or a client started
    /// it, `--idle-exit`).
    started: bool,
    idle_since: Option<Instant>,
}

/// Serve clients at `path` until 2 s after the last process and client
/// have gone (once the first client has come; `idle_exit`: from the start).
pub(crate) fn serve(path: &Path, idle_exit: bool) -> Result<(), Error> {
    let endpoint = Endpoint::claim(path)?;
    // A guest writing to a closed pipe or socket gets EPIPE, and so does a
    // write to a client that has gone; the host must not be killed by
    // SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    signal::install_interrupt();
    let pids = RangePids::new()?;
    let sched = Arc::new(Sched::new());
    let parking = crate::park::Parking::new(sched.clone())?;
    let table = Arc::new(Table::new(Box::new(pids), 0, parking.clone()));
    let notify = Arc::new(Notify::new()?);
    let _ = table.service.set(notify.clone());
    // Each process's umask is applied as it creates files; the host's own
    // would apply on top.
    let old_umask = unsafe { libc::umask(0) };
    route_host_signals(&table)?;
    // The VM, with no process yet: the pool and windows mapped into it,
    // as a host's first process would have them.
    let machine = Machine::new()?;
    let vm = hv::Vm::new()?;
    machine.map_into(&vm)?;
    let host = Host { vm: &vm, machine: &machine, sched, parking, table: Some(table.clone()), exec_host: Mutex::new(None) };
    let mut service = Service { table, notify, endpoint, clients: Vec::new(), started: idle_exit, idle_since: None };
    std::thread::scope(|scope| {
        linux_abi::host_threads::spawn_scoped(scope, "ticker", || host.sched.ticker()).expect("the host's own threads fit its budget");
        crate::park::run_waiters(&host, scope);
        service.run(&host, scope);
    });
    drop(host.parking.take_all());
    unsafe { libc::umask(old_umask) };
    Ok(())
}

impl Service {
    fn run<'scope, 'env>(&mut self, sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>) {
        loop {
            for event in self.notify.take() {
                self.event(event);
            }
            self.drop_gone();
            let busy = self.table.live.load(Ordering::Acquire) > 0 || self.clients.iter().any(Client::counts);
            let timeout = if busy || !self.started {
                self.idle_since = None;
                -1
            } else {
                let since = *self.idle_since.get_or_insert_with(Instant::now);
                let left = GRACE.saturating_sub(since.elapsed());
                if left.is_zero() {
                    // One last look for a client that has just come.
                    self.accept();
                    if self.clients.is_empty() {
                        break;
                    }
                    continue;
                }
                left.as_millis() as i32 + 1
            };
            let mut fds = vec![
                libc::pollfd { fd: self.notify.wake.0.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.endpoint.listener.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 },
            ];
            for c in &self.clients {
                let typing = c.tty.as_ref().is_some_and(|t| !t.typed.is_empty());
                // While its typing waits for the pty, the client waits too.
                fds.push(libc::pollfd { fd: if typing { -1 } else { c.sock.as_raw_fd() }, events: libc::POLLIN, revents: 0 });
                fds.push(libc::pollfd {
                    fd: match &c.tty {
                        Some(t) if typing => t.master.as_raw_fd(),
                        _ => -1,
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } <= 0 {
                continue;
            }
            for i in 0..self.clients.len() {
                if fds[2 + 2 * i].revents != 0 {
                    self.read(sh, scope, i);
                }
                if fds[3 + 2 * i].revents != 0 {
                    self.type_in(i);
                }
            }
            if fds[1].revents != 0 {
                self.accept();
            }
        }
        // No process, no client, and no one came: the path is free for a
        // new host from now on, and this one ends.
        self.endpoint.close();
        sh.sched.stop();
    }

    fn accept(&mut self) {
        let Some(listener) = &self.endpoint.listener else { return };
        loop {
            let sock = match listener.accept() {
                Ok((sock, _)) => sock,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            // Only the user's own processes.
            if proto::peer_uid(&sock) != Some(unsafe { libc::getuid() }) {
                continue;
            }
            // Reads don't wait (the service polls); writes do, a little: a
            // client that doesn't read for that long has stopped or gone.
            let _ = sock.set_nonblocking(false);
            let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
            let mac_pid = proto::peer_pid(&sock);
            let mut client = Client {
                sock,
                reader: proto::Reader::default(),
                mac_pid,
                pid: None,
                ran: false,
                listing: false,
                tty: None,
                told_stopped: false,
                gone: false,
            };
            if self.clients.len() >= MAX_CLIENTS {
                client.send(proto::FAILED, b"the host has too many clients", &[]);
                continue;
            }
            self.clients.push(client);
        }
    }

    /// Client `i` has sent something (or gone).
    fn read<'scope, 'env>(&mut self, sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, i: usize) {
        let c = &mut self.clients[i];
        let eof = c.reader.fill(c.sock.as_raw_fd(), true) == proto::Read::Eof;
        loop {
            let msg = match self.clients[i].reader.next() {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(_) => {
                    self.clients[i].gone = true;
                    break;
                }
            };
            if let Err(Truncated) = self.message(sh, scope, i, msg) {
                self.clients[i].gone = true;
                break;
            }
        }
        if eof {
            self.clients[i].gone = true;
        }
    }

    fn message<'scope, 'env>(&mut self, sh: &'scope Host<'env>, scope: &'scope Scope<'scope, 'env>, i: usize, msg: proto::Msg) -> Result<(), Truncated> {
        let mut d = Decoder(&msg.body);
        match msg.kind {
            proto::RUN if !self.clients[i].ran => {
                self.clients[i].ran = true;
                self.started = true;
                let launch = Launch::decode(&mut d)?;
                // Its fds 0–2 that came (a bit each), in order.
                let sent = d.u64()?;
                let stdio: Vec<(usize, OwnedFd)> = (0..3).filter(|fd| sent & (1 << fd) != 0).zip(msg.fds).collect();
                let terminal = if d.bool()? {
                    let (termios, size) = proto::decode_terminal(&mut d)?;
                    let which = d.u64()?;
                    Some((termios, size, (0..3u64).filter(|fd| which & (1 << fd) != 0).collect::<Vec<_>>()))
                } else {
                    None
                };
                match self.start(sh, scope, launch, stdio, terminal) {
                    Ok((pid, pty)) => {
                        let c = &mut self.clients[i];
                        c.pid = Some(pid);
                        let mut e = Encoder::default();
                        e.i64(pid);
                        match pty {
                            Some(pty) => {
                                c.send(proto::STARTED, &e.0, &[pty.master.as_raw_fd()]);
                                c.tty = Some(ClientTty { n: pty.n, master: pty.master, typed: Vec::new() });
                            }
                            None => c.send(proto::STARTED, &e.0, &[]),
                        }
                    }
                    Err(why) => self.clients[i].send(proto::FAILED, why.as_bytes(), &[]),
                }
            }
            proto::PS => {
                let text = self.listing();
                let c = &mut self.clients[i];
                c.listing = true;
                c.send(proto::LISTING, text.as_bytes(), &[]);
                c.gone = true;
            }
            proto::INPUT => {
                if let Some(t) = &mut self.clients[i].tty {
                    t.typed.extend_from_slice(&msg.body);
                    self.type_in(i);
                }
            }
            proto::SIGNAL => {
                let sig = d.u64()?;
                if let Some(pid) = self.clients[i].pid {
                    self.signal_group(pid, sig, SI_USER);
                }
            }
            proto::RESIZE => {
                let ws = proto::decode_winsize(&mut d)?;
                if let Some(t) = &self.clients[i].tty {
                    self.table.resize_tty(t.n, t.master.as_raw_fd(), ws);
                }
            }
            proto::HANGUP => {
                if let Some(t) = &self.clients[i].tty {
                    self.table.hangup(t.n);
                }
            }
            _ => return Err(Truncated),
        }
        Ok(())
    }

    /// Send `sig` from the Mac side to the group of `pid`, as a terminal
    /// sends its foreground job's (the client is that job in the Mac
    /// shell; its process leads a group of its own).
    fn signal_group(&self, pid: i64, sig: u64, code: i32) {
        let pgid = {
            let s = self.table.state();
            match s.procs.get(&pid) {
                Some(e) if matches!(e.kind, Kind::Local(_)) => e.pgid,
                _ => return,
            }
        };
        let _ = self.table.deliver(Target::Group(pgid), SigInfo::user(sig, code, 0));
    }

    /// What client `i` typed goes into its pty, as far as the pty takes it.
    fn type_in(&mut self, i: usize) {
        let table = &self.table;
        let Some(t) = &mut self.clients[i].tty else { return };
        if t.typed.is_empty() {
            return;
        }
        let master = t.master.as_raw_fd();
        let r = table.type_keys(t.n, master, &t.typed).unwrap_or_else(|| {
            let n = unsafe { libc::write(master, t.typed.as_ptr().cast(), t.typed.len()) };
            if n < 0 { Err(errno::from_darwin(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))) } else { Ok(n as i64) }
        });
        match r {
            Ok(n) => {
                t.typed.drain(..n as usize);
            }
            Err(e) if e == errno::EAGAIN || e == errno::EINTR => {}
            // Nothing has the slave open: what's typed goes nowhere.
            Err(_) => t.typed.clear(),
        }
    }

    /// Start `launch` for a client: a process of the host's, init's child,
    /// leading a group and session of its own (as a process `macwl-run`
    /// starts), with a pty for its terminal if it has one. Its pid, and
    /// the pty.
    fn start<'scope, 'env>(
        &self,
        sh: &'scope Host<'env>,
        scope: &'scope Scope<'scope, 'env>,
        launch: Launch,
        stdio: Vec<(usize, OwnedFd)>,
        terminal: Option<(libc::termios, Option<libc::winsize>, Vec<u64>)>,
    ) -> Result<(i64, Option<relay::Pty>), String> {
        let table = &self.table;
        let (mut process, elf) = launch.process(stdio)?;
        let pid = table.pids.alloc().ok_or("no pid left for a process")?;
        process.pid = pid;
        process.set_kernel(Arc::new(TableKernel(Arc::downgrade(table))));
        process.set_umask(launch.umask);
        process.ignore_signals(launch.ignored);
        let made = (|| {
            let pty = match &terminal {
                Some((termios, size, which)) => {
                    let pty = relay::Pty::open(termios, *size).map_err(|e| format!("no pty for the terminal: {e}"))?;
                    pty.give(&process, which).map_err(|e| format!("no pty for the terminal: {e}"))?;
                    Some(pty)
                }
                None => None,
            };
            let argv: Vec<&[u8]> = launch.argv.iter().map(Vec::as_slice).collect();
            let envp: Vec<&[u8]> = launch.env.iter().map(Vec::as_slice).collect();
            let program = load(sh.machine, &elf, &argv, &envp, &process).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(&launch.argv[0])))?;
            process.set_arg_area(program.args, program.env);
            Ok::<_, String>((program, pty))
        })();
        let (program, pty) = match made {
            Ok(made) => made,
            Err(e) => {
                table.pids.free(pid);
                return Err(e);
            }
        };
        let space = Arc::new(Space::new(program.space));
        space.lock().take_tlb_stale();
        let mut thread = Thread::main(&process);
        thread.set_blocked_signals(launch.blocked);
        let proc = Proc::new(process, space);
        table.insert(pid, 1, pid, pid, None, proc.clone(), SIGCHLD);
        {
            let mut s = table.state();
            if let Some(e) = s.procs.get_mut(&pid) {
                e.client = true;
                // The client is the Mac shell's job: that shell continues
                // it when it stops, so its group is never orphaned.
                e.mac_parent = true;
            }
            if let Some(pty) = &pty {
                Table::set_ctty(&mut s, pid, pty.n);
            }
        }
        let task = Task::new(thread, proc);
        sh.register(&task);
        place(sh, scope, task, Start::Fresh { entry: program.entry, sp: program.sp });
        Ok((pid, pty))
    }

    fn event(&mut self, event: Event) {
        let pid = match &event {
            Event::Stopped(pid, _) | Event::Continued(pid) | Event::Exited(pid, _) => *pid,
            Event::NoProcesses => return,
        };
        let Some(c) = self.clients.iter_mut().find(|c| c.pid == Some(pid)) else { return };
        match event {
            Event::Stopped(_, sig) => {
                let mut e = Encoder::default();
                e.u64(sig);
                c.send(proto::STOPPED, &e.0, &[]);
                c.told_stopped = true;
            }
            // Continued from inside (a `kill -CONT`, a shell's `fg`): so is
            // the client, which stopped with it.
            Event::Continued(_) => {
                if std::mem::take(&mut c.told_stopped)
                    && let Some(mac_pid) = c.mac_pid
                {
                    unsafe { libc::kill(mac_pid, libc::SIGCONT) };
                }
            }
            Event::Exited(_, exit) => {
                let mut e = Encoder::default();
                proto::encode_exit(&mut e, &exit);
                c.send(proto::EXITED, &e.0, &[]);
                c.pid = None;
            }
            Event::NoProcesses => {}
        }
    }

    /// Clients that have gone: a process still running hangs up, as a
    /// terminal closing hangs up its session (SIGHUP and SIGCONT to the
    /// session's leader, the process, which as a shell passes it on to its
    /// jobs); without a terminal, the process's group gets them.
    fn drop_gone(&mut self) {
        let mut i = 0;
        while i < self.clients.len() {
            if !self.clients[i].gone {
                i += 1;
                continue;
            }
            let c = self.clients.swap_remove(i);
            match (&c.tty, c.pid) {
                (Some(t), _) => self.table.hangup(t.n),
                (None, Some(pid)) => {
                    for sig in [SIGHUP, SIGCONT] {
                        self.signal_group(pid, sig, SI_KERNEL);
                    }
                }
                (None, None) => {}
            }
        }
    }

    /// `macwl-run --ps`: the host's processes, as `ps -ef` would show them
    /// inside, and the Mac pid of each one's client.
    fn listing(&self) -> String {
        let clients: std::collections::HashMap<i64, libc::pid_t> =
            self.clients.iter().filter_map(|c| Some((c.pid?, c.mac_pid?))).collect();
        let rows: Vec<_> = {
            let s = self.table.state();
            s.procs
                .iter()
                .map(|(&pid, e)| {
                    let (state, cpu, proc, comm) = match &e.kind {
                        Kind::Local(p) => {
                            let state = if p.stopped.load(Ordering::Acquire) {
                                'T'
                            } else if p.cpu.in_guest() {
                                'R'
                            } else {
                                'S'
                            };
                            (state, p.cpu.times(), Some(p.clone()), p.abi.comm())
                        }
                        Kind::Zombie { cpu, comm, .. } => ('Z', *cpu, None, comm.clone()),
                        // A stand-in: what macOS says of its Mac program.
                        Kind::Mac { cpu } => {
                            let m = e.mac.as_ref().expect("a stand-in's Mac program");
                            let (user, sys, _) = crate::mac::usage(m.pid).unwrap_or_default();
                            (if m.stopped { 'T' } else { 'S' }, *cpu + crate::cputime::Times { user, sys }, None, m.comm.clone())
                        }
                    };
                    let mac = e.mac.as_ref().filter(|_| matches!(e.kind, Kind::Mac { .. })).map(|m| (m.pid, m.cmdline.clone()));
                    (pid, e.ppid, e.pgid, e.sid, e.ctty, state, cpu, proc, comm, mac)
                })
                .collect()
        };
        let mut out = format!("{:>7} {:>7} {:>7} {:>7} {:<7} {:<4} {:>8} {:>8} {:>6}  CMD\n", "PID", "PPID", "PGID", "SID", "TTY", "STAT", "TIME", "RSS", "CLIENT");
        for (pid, ppid, pgid, sid, ctty, state, cpu, proc, comm, mac) in rows {
            // The command line from the process's memory, as /proc's
            // cmdline; a zombie's name in brackets.
            let (cmd, rss_kb) = match proc.and_then(|p| Some((p.abi.arg_area(), p.space.lock().unwrap_or_else(|e| e.into_inner()).clone()?))) {
                Some(([a0, a1, ..], space)) => {
                    let mut args = vec![0u8; a1.saturating_sub(a0).min(4096) as usize];
                    let args = if linux_abi::GuestMemory::read(&*space, a0, &mut args).is_ok() { args } else { Vec::new() };
                    let cmd: Vec<u8> = args.strip_suffix(b"\0").unwrap_or(&args).iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
                    let cmd = if cmd.is_empty() { format!("[{}]", String::from_utf8_lossy(&comm)) } else { String::from_utf8_lossy(&cmd).into_owned() };
                    (cmd, linux_abi::GuestMemory::resident_pages(&*space) * 4)
                }
                None => match &mac {
                    Some((mac_pid, cmdline)) => {
                        let cmd: Vec<u8> = cmdline.strip_suffix(b"\0").unwrap_or(cmdline).iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
                        (String::from_utf8_lossy(&cmd).into_owned(), crate::mac::usage(*mac_pid).map_or(0, |u| u.2 / 1024))
                    }
                    None => (format!("[{}]", String::from_utf8_lossy(&comm)), 0),
                },
            };
            let secs = cpu.total() / 1_000_000_000;
            let time = format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
            let tty = ctty.map_or_else(|| "?".to_string(), |n| format!("pts/{n}"));
            // The Mac process standing for it: its client's, or the Mac
            // program it stands in for (marked so).
            let client = match (clients.get(&pid), &mac) {
                (Some(p), _) => p.to_string(),
                (None, Some((mac_pid, _))) => format!("{mac_pid}*"),
                (None, None) => "-".to_string(),
            };
            out.push_str(&format!("{pid:>7} {ppid:>7} {pgid:>7} {sid:>7} {tty:<7} {state:<4} {time:>8} {rss_kb:>8} {client:>6}  {cmd}\n"));
        }
        out
    }
}
