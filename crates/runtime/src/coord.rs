//! The coordinator (docs/processes.md, section 1, "Federated hosts"; step
//! 9): one per user, `macwl-run --coordinator`, serving the Unix socket
//! `$TMPDIR/macwl-$UID/coordinator`. Hosts link to it; it knows them all,
//! and forwards between them. It holds no process state of its own.
//!
//! A host links to a coordinator that is running, and starts one only when
//! it needs one (`macwl-run --ps --all`, a program moving to a host of its
//! own): a session that never federates never has a coordinator process.
//! A host without one watches the socket's directory (a kqueue on it, no
//! polling), and links as soon as one starts; one that loses its
//! coordinator does the same, and re-registers with the next.
//!
//! The coordinator goes 2 s after its last host has gone.
//!
//! Wire: `proto`'s messages. A host says [`proto::HELLO`] first; the
//! coordinator asks hosts for their listings ([`proto::PS`], answered with
//! [`proto::LISTING`]) for a client's [`proto::PS_ALL`].

use std::{
    collections::HashSet,
    ffi::CString,
    io::ErrorKind,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use crate::{endpoint::Endpoint, host::Table, proto};

/// How long a coordinator with no host waits for one before it goes.
const GRACE: Duration = Duration::from_secs(2);
/// How long a listing waits for hosts that should answer (and for those
/// the pid file says are running to link, once it has just started).
const ANSWER_WITHIN: Duration = Duration::from_millis(1000);
const LINK_WITHIN: Duration = Duration::from_millis(500);
/// The most connections the coordinator takes at once.
const MAX_CONNS: usize = 1024;

/// The user's directory for hosts' shared state: the pid ranges file and
/// the coordinator's socket.
pub(crate) fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("macwl-{}", unsafe { libc::getuid() }))
}

pub(crate) fn socket_path() -> PathBuf {
    dir().join("coordinator")
}

/// A connection to the coordinator: a host's link, a client's query, or
/// not yet known.
struct Conn {
    sock: UnixStream,
    reader: proto::Reader,
    /// Its Mac pid.
    mac_pid: Option<libc::pid_t>,
    role: Role,
    gone: bool,
}

#[derive(PartialEq, Eq)]
enum Role {
    Unknown,
    Host,
    /// A `--ps --all`: the hosts it's still waiting for, what they said,
    /// and when it gives up on the rest.
    Listing { waiting: HashSet<libc::pid_t>, parts: Vec<(libc::pid_t, String)>, until: Instant },
}

impl Conn {
    fn send(&mut self, kind: u8, body: &[u8]) {
        if !self.gone && proto::send(self.sock.as_raw_fd(), kind, body, &[]).is_err() {
            self.gone = true;
        }
    }
}

/// `macwl-run --coordinator`.
pub fn run() -> ExitCode {
    let path = socket_path();
    let mut endpoint = match Endpoint::claim(&path) {
        Ok(e) => e,
        // Another one serves already: nothing to do.
        Err(e) if e.kind() == ErrorKind::AddrInUse => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("macwl-run: coordinator: {e}");
            return ExitCode::from(2);
        }
    };
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let started = Instant::now();
    let mut conns: Vec<Conn> = Vec::new();
    let mut idle_since: Option<Instant> = None;
    loop {
        conns.retain(|c| !c.gone);
        finish_listings(&mut conns);
        let busy = !conns.is_empty();
        let timeout = if busy {
            idle_since = None;
            // A listing's deadline, if one waits.
            conns
                .iter()
                .filter_map(|c| match &c.role {
                    Role::Listing { until, .. } => Some(until.saturating_duration_since(Instant::now()).as_millis() as i32 + 1),
                    _ => None,
                })
                .min()
                .unwrap_or(-1)
        } else {
            let since = *idle_since.get_or_insert_with(Instant::now);
            let left = GRACE.saturating_sub(since.elapsed());
            if left.is_zero() {
                break;
            }
            left.as_millis() as i32 + 1
        };
        let listener = endpoint.listener.as_ref().map_or(-1, |l| l.as_raw_fd());
        let mut fds = vec![libc::pollfd { fd: listener, events: libc::POLLIN, revents: 0 }];
        fds.extend(conns.iter().map(|c| libc::pollfd { fd: c.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 }));
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } <= 0 {
            continue;
        }
        for i in 0..conns.len() {
            if fds[1 + i].revents != 0 {
                read(&mut conns, i, started);
            }
        }
        if fds[0].revents != 0
            && let Some(l) = &endpoint.listener
        {
            while let Ok((sock, _)) = l.accept() {
                if proto::peer_uid(&sock) != Some(unsafe { libc::getuid() }) || conns.len() >= MAX_CONNS {
                    continue;
                }
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
                let mac_pid = proto::peer_pid(&sock);
                conns.push(Conn { sock, reader: proto::Reader::default(), mac_pid, role: Role::Unknown, gone: false });
            }
        }
    }
    endpoint.close();
    ExitCode::SUCCESS
}

/// Connection `i` has sent something (or gone).
fn read(conns: &mut [Conn], i: usize, started: Instant) {
    let eof = conns[i].reader.fill(conns[i].sock.as_raw_fd(), true) == proto::Read::Eof;
    loop {
        let msg = match conns[i].reader.next() {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(_) => {
                conns[i].gone = true;
                break;
            }
        };
        message(conns, i, msg, started);
    }
    if eof {
        conns[i].gone = true;
        // A host gone: listings waiting for it don't any more.
        if let Some(pid) = conns[i].mac_pid {
            for c in conns.iter_mut() {
                if let Role::Listing { waiting, .. } = &mut c.role {
                    waiting.remove(&pid);
                }
            }
        }
    }
}

fn message(conns: &mut [Conn], i: usize, msg: proto::Msg, started: Instant) {
    match (msg.kind, &conns[i].role) {
        (proto::HELLO, Role::Unknown) => {
            conns[i].role = Role::Host;
            // A listing waiting for this host (the pid file said it runs)
            // asks it now.
            let me = conns[i].mac_pid;
            if conns.iter().any(|c| matches!(&c.role, Role::Listing { waiting, .. } if me.is_some_and(|p| waiting.contains(&p)))) {
                conns[i].send(proto::PS, &[]);
            }
        }
        (proto::PS_ALL, Role::Unknown) => {
            let mut waiting: HashSet<libc::pid_t> = HashSet::new();
            for c in conns.iter_mut().filter(|c| c.role == Role::Host) {
                if let Some(pid) = c.mac_pid {
                    waiting.insert(pid);
                }
                c.send(proto::PS, &[]);
            }
            // Just started: hosts the pid file says are running may not have
            // linked yet. Wait a little for them too.
            if started.elapsed() < LINK_WITHIN {
                waiting.extend(crate::host::range_owners().into_iter().map(|(_, owner)| owner).filter(|&o| o != std::process::id() as i32));
            }
            let until = Instant::now() + ANSWER_WITHIN;
            conns[i].role = Role::Listing { waiting, parts: Vec::new(), until };
        }
        (proto::LISTING, Role::Host) => {
            let Some(pid) = conns[i].mac_pid else { return };
            let text = String::from_utf8_lossy(&msg.body).into_owned();
            for c in conns.iter_mut() {
                if let Role::Listing { waiting, parts, .. } = &mut c.role
                    && waiting.remove(&pid)
                {
                    parts.push((pid, text.clone()));
                }
            }
        }
        _ => conns[i].gone = true,
    }
}

/// Listings that have every answer (or have waited long enough) go to
/// their clients.
fn finish_listings(conns: &mut [Conn]) {
    let now = Instant::now();
    for c in conns.iter_mut() {
        let Role::Listing { waiting, parts, until } = &mut c.role else { continue };
        if !waiting.is_empty() && now < *until {
            continue;
        }
        parts.sort_by_key(|(pid, _)| *pid);
        let mut out = String::new();
        for (pid, text) in parts.iter() {
            out.push_str(&format!("host {pid}\n{text}"));
        }
        let missing: Vec<String> = waiting.iter().map(|p| p.to_string()).collect();
        if !missing.is_empty() {
            out.push_str(&format!("(no answer from host {})\n", missing.join(", ")));
        }
        c.send(proto::LISTING, out.as_bytes());
        c.gone = true;
    }
}

/// `macwl-run --ps --all`: every host's processes, from the coordinator
/// (started if none runs).
pub fn ps_all() -> ExitCode {
    let sock = match connect(true) {
        Ok(sock) => sock,
        Err(why) => {
            eprintln!("macwl-run: {why}");
            return ExitCode::from(1);
        }
    };
    if proto::send(sock.as_raw_fd(), proto::PS_ALL, &[], &[]).is_ok()
        && let Some(msg) = proto::Reader::default().recv(sock.as_raw_fd())
        && msg.kind == proto::LISTING
    {
        print!("{}", String::from_utf8_lossy(&msg.body));
        return ExitCode::SUCCESS;
    }
    eprintln!("macwl-run: the coordinator didn't answer");
    ExitCode::from(1)
}

/// A connection to the coordinator; `start`: starting one if none runs.
pub(crate) fn connect(start: bool) -> Result<UnixStream, String> {
    let path = socket_path();
    if let Ok(sock) = UnixStream::connect(&path) {
        return Ok(sock);
    }
    if !start {
        return Err("no coordinator".into());
    }
    let pid = spawn().map_err(|e| format!("starting the coordinator: {e}"))?;
    // Reaped as it goes, by a thread that waits for it alone.
    let _ = linux_abi::host_threads::spawn("coordinator reaper", move || {
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(&path) {
            Ok(sock) => return Ok(sock),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Start `macwl-run --coordinator`: a new process in a Mac session of its
/// own, its signals as a new program's, its stderr to the socket's log.
fn spawn() -> std::io::Result<libc::pid_t> {
    let exe = crate::mac::macwl_run_exe()?;
    let cstr = |b: &[u8]| CString::new(b).map_err(|_| std::io::Error::from(ErrorKind::InvalidInput));
    let args = [cstr(exe.as_os_str().as_encoded_bytes())?, cstr(b"--coordinator")?];
    std::fs::create_dir_all(dir())?;
    let log = cstr([socket_path().as_os_str().as_encoded_bytes(), b".log"].concat().as_slice())?;
    let flags = crate::mac::POSIX_SPAWN_SETSID
        | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short
        | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
        | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
    crate::mac::spawn(&args[0], &args, &[], flags, |actions, attr| unsafe {
        let mut all: libc::sigset_t = 0;
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(attr, &all);
        let mut none: libc::sigset_t = 0;
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(attr, &none);
        libc::posix_spawn_file_actions_addopen(actions, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
        libc::posix_spawn_file_actions_addopen(actions, 1, c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
        libc::posix_spawn_file_actions_addopen(actions, 2, log.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o600);
    })
    .map_err(std::io::Error::from_raw_os_error)
}

/// A host's link to the coordinator: a thread that keeps it linked (to the
/// one running, or the next to start) and answers it for the table. It
/// ends when the link does (the table has gone).
pub(crate) struct Link {
    /// Its write end closing (the table going) ends the thread.
    _stop: OwnedFd,
    /// The connection while there is one, for the table to send on.
    sock: Arc<Mutex<Option<UnixStream>>>,
}

impl Link {
    pub fn start(table: &Arc<Table>) -> std::io::Result<Link> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (stop_r, stop_w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let sock = Arc::new(Mutex::new(None));
        let (weak, shared) = (Arc::downgrade(table), sock.clone());
        linux_abi::host_threads::spawn("coordinator link", move || link(weak, shared, stop_r)).map_err(|_| std::io::Error::from(ErrorKind::WouldBlock))?;
        Ok(Link { _stop: stop_w, sock })
    }

    /// Whether the host is linked now.
    #[allow(dead_code)]
    pub fn linked(&self) -> bool {
        self.sock.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// The link thread: linked while a coordinator runs, waiting for one (the
/// socket's directory changing) while none does, until `stop` ends.
fn link(table: Weak<Table>, shared: Arc<Mutex<Option<UnixStream>>>, stop: OwnedFd) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let _ = std::fs::create_dir_all(dir());
    let Ok(dir_c) = CString::new(dir().as_os_str().as_encoded_bytes()) else { return };
    let dir_fd = unsafe { libc::open(dir_c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if dir_fd < 0 {
        return;
    }
    let dir_fd = unsafe { OwnedFd::from_raw_fd(dir_fd) };
    let watch = |fd: RawFd, filter: i16, fflags: u32| {
        let ev = libc::kevent { ident: fd as usize, filter, flags: libc::EV_ADD | libc::EV_CLEAR, fflags, data: 0, udata: std::ptr::null_mut() };
        unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    };
    watch(stop.as_raw_fd(), libc::EVFILT_READ, 0);
    watch(dir_fd.as_raw_fd(), libc::EVFILT_VNODE, libc::NOTE_WRITE);
    loop {
        // Linked: answer it until it goes.
        if let Ok(sock) = UnixStream::connect(socket_path()) {
            if serve_link(&table, &shared, sock, &kq, &stop) {
                return;
            }
            continue;
        }
        // Not: until the directory changes (a coordinator's socket may
        // have come), or the host goes.
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, out.as_mut_ptr(), 4, std::ptr::null()) };
        if out[..n.max(0) as usize].iter().any(|ev| ev.ident == stop.as_raw_fd() as usize && ev.filter == libc::EVFILT_READ) {
            return;
        }
        // The socket appears a moment before it listens.
        for _ in 0..20 {
            if std::fs::symlink_metadata(socket_path()).is_err() || UnixStream::connect(socket_path()).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Linked to the coordinator on `sock`: say who we are, then answer it.
/// True if the host has gone (the thread ends), false if the coordinator
/// has.
fn serve_link(table: &Weak<Table>, shared: &Arc<Mutex<Option<UnixStream>>>, sock: UnixStream, kq: &OwnedFd, stop: &OwnedFd) -> bool {
    let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
    if proto::send(sock.as_raw_fd(), proto::HELLO, &[], &[]).is_err() {
        return false;
    }
    let Ok(send) = sock.try_clone() else { return false };
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = Some(send);
    let ev = libc::kevent { ident: sock.as_raw_fd() as usize, filter: libc::EVFILT_READ, flags: libc::EV_ADD, fflags: 0, data: 0, udata: std::ptr::null_mut() };
    unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    let mut reader = proto::Reader::default();
    let gone = 'linked: loop {
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, out.as_mut_ptr(), 4, std::ptr::null()) };
        let events = &out[..n.max(0) as usize];
        if events.iter().any(|ev| ev.ident == stop.as_raw_fd() as usize && ev.filter == libc::EVFILT_READ) {
            break true;
        }
        if !events.iter().any(|ev| ev.ident == sock.as_raw_fd() as usize) {
            continue;
        }
        let eof = reader.fill(sock.as_raw_fd(), true) == proto::Read::Eof;
        loop {
            match reader.next() {
                Ok(Some(msg)) => {
                    let Some(table) = table.upgrade() else { break 'linked true };
                    if msg.kind == proto::PS {
                        let text = table.listing();
                        if proto::send(sock.as_raw_fd(), proto::LISTING, text.as_bytes(), &[]).is_err() {
                            break 'linked false;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break 'linked false,
            }
        }
        if eof {
            break false;
        }
    };
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let ev = libc::kevent { ident: sock.as_raw_fd() as usize, filter: libc::EVFILT_READ, flags: libc::EV_DELETE, fflags: 0, data: 0, udata: std::ptr::null_mut() };
    unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    gone
}
