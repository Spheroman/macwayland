//! Forking a guest process.
//!
//! Hypervisor.framework doesn't survive a host `fork()` (the child's
//! framework state is unusable and crashes), so a guest's fork child is a
//! separate `macwl-run --fork-child` process that the parent hands the
//! process to:
//!
//! - guest memory as copy-on-write Mach memory entries ([`crate::mach`]),
//!   one per segment of the frame pool in use —
//!   in RAM, never a file (macOS writes dirty file pages to disk);
//! - over a Unix socket: the CPU state, the address-space layout, the fd
//!   table and the rest of [`Process`], with host fds (guest files, shared
//!   windows) attached as `SCM_RIGHTS`.
//!
//! Launching a process costs ~3 ms on macOS before any of our code runs, so a
//! process that has forked once keeps a *spare*: a child launched ahead of
//! time, idle until the next fork hands it a process. It announces itself
//! (with a port for the memory entry) on the parent's Mach inbox, ignores
//! signals while idle, survives the parent's `execve` (it holds nothing yet)
//! and exits when the parent does. `wait` never reports it
//! ([`linux_abi::Process`] only reaps children it forked).
//!
//! The socket stays open as the child's control channel (see
//! [`linux_abi::syscall::ctl`]): it says when it's ready (and the parent may
//! run on), when it execs (which ends a vfork), and whether a Linux signal
//! killed it.

use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Mutex,
};

use linux_abi::{
    errno,
    syscall::{ctl, Fork, Thread},
    wire::{Decoder, Encoder},
    Process,
};

use crate::{
    hv::UserContext,
    mach::{self, MemoryEntry, ReceivePort, SendPort},
    memory::{AddressSpace, Space},
    profile::mark,
    Error, Exit,
};

/// The child finds its control socket here.
const CHILD_CTL_FD: RawFd = 3;
/// File descriptors per SCM_RIGHTS message.
const FDS_PER_MSG: usize = 64;
/// How long to wait for a spare to announce itself.
const SPARE_TIMEOUT_MS: u32 = 10_000;

/// A child launched ahead of time.
struct Spare {
    pid: i32,
    sock: OwnedFd,
    /// Where to send it the memory entry, once it has said.
    port: Option<SendPort>,
}

/// This process's side of forking: the inbox spares announce themselves on,
/// and the current spare.
struct Broker {
    inbox: ReceivePort,
    spare: Option<Spare>,
}

static BROKER: Mutex<Option<Broker>> = Mutex::new(None);

impl Broker {
    /// Launch a spare.
    fn launch(&mut self) -> io::Result<()> {
        let (ours, theirs) = socketpair()?;
        let to_us = self.inbox.sender()?;
        // Registered ports are what a new task inherits; one at a time.
        to_us.register()?;
        let pid = spawn_child(theirs.as_raw_fd());
        mach::unregister();
        self.spare = Some(Spare { pid: pid?, sock: ours, port: None });
        Ok(())
    }

    /// The spare, once it has announced itself (launching one if needed).
    fn take_spare(&mut self) -> io::Result<Spare> {
        if self.spare.is_none() {
            self.launch()?;
        }
        let mut spare = self.spare.take().unwrap();
        while spare.port.is_none() {
            match self.inbox.receive(SPARE_TIMEOUT_MS)? {
                // Announcements from spares that died since are dropped.
                Some((port, pid)) if pid == spare.pid as u64 => spare.port = Some(port),
                Some(_) => {}
                None => {
                    discard(spare);
                    return Err(io::Error::other("the spare process never started"));
                }
            }
        }
        Ok(spare)
    }
}

/// Get rid of the spare, if there is one: for a process that won't fork
/// again (it's becoming a Mac program).
pub fn discard_spare() {
    let spare = BROKER.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|b| b.spare.take());
    if let Some(spare) = spare {
        discard(spare);
    }
}

/// Get rid of a spare that won't be used.
fn discard(spare: Spare) {
    unsafe {
        libc::kill(spare.pid, libc::SIGKILL);
        libc::waitpid(spare.pid, std::ptr::null_mut(), 0);
    }
}

/// In the parent: make a child that resumes with `ctx` (x0 = 0). Returns its
/// pid, or a Linux errno.
pub fn fork(space: &Space, process: &Process, thread: &Thread, ctx: &UserContext, f: &Fork) -> Result<i64, i64> {
    mark("fork: snapshot");
    let mut e = Encoder::default();
    ctx.encode(&mut e);
    f.encode(&mut e);
    let (entries, window_fds) = space.lock().encode_for_fork(&mut e).map_err(|err| {
        eprintln!("macwl-run: fork: {err}");
        errno::ENOMEM
    })?;
    // The runtime's own stderr first (spares start with /dev/null), then the
    // shared windows', then the guest's.
    let mut fds = vec![libc::STDERR_FILENO];
    fds.extend(window_fds);
    fds.extend(process.encode_for_fork(thread, &mut e));

    let mut broker = BROKER.lock().unwrap_or_else(|e| e.into_inner());
    if broker.is_none() {
        let inbox = ReceivePort::new().map_err(|_| errno::EAGAIN)?;
        *broker = Some(Broker { inbox, spare: None });
    }
    let broker = broker.as_mut().unwrap();
    // A spare that died since it was launched fails the handoff; then try a
    // fresh one.
    let mut attempt = 0;
    let (pid, sock) = loop {
        attempt += 1;
        mark("fork: getting a spare");
        let spare = broker.take_spare().map_err(|err| {
            eprintln!("macwl-run: fork: {err}");
            errno::EAGAIN
        })?;
        mark("fork: handing over");
        match hand_over(&spare, &entries, &fds, &e.0) {
            Ok(()) => break (spare.pid, spare.sock),
            Err(err) => {
                discard(spare);
                if attempt == 2 {
                    eprintln!("macwl-run: fork: {err}");
                    return Err(errno::EAGAIN);
                }
            }
        }
    };
    mark("fork: child ready");
    // Have the next spare start up while this process carries on.
    if let Err(err) = broker.launch() {
        eprintln!("macwl-run: fork: launching a spare: {err}");
    }
    drop(entries);

    // vfork: the parent waits until the child execs or exits.
    let mut seen = Vec::new();
    if f.vfork {
        let mut buf = [0u8; 64];
        loop {
            match read_some(&sock, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if buf[..n].contains(&ctl::EXEC) {
                        break;
                    }
                }
            }
        }
    }
    process.forked_parent(space, f, pid as i64, sock, &seen);
    Ok(pid as i64)
}

/// Give `spare` the process and wait until it has taken it.
fn hand_over(spare: &Spare, entries: &[MemoryEntry], fds: &[RawFd], state: &[u8]) -> io::Result<()> {
    send_state(&spare.sock, fds, state)?;
    // Then the memory, one entry per pool segment in use, each saying how
    // many more follow. (After the state, which the child takes first: a
    // port's queue holds only a few messages.)
    for (i, entry) in entries.iter().enumerate() {
        entry.send_to(spare.port.as_ref().unwrap(), (entries.len() - 1 - i) as u64)?;
    }
    let mut b = [0u8];
    match read_some(&spare.sock, &mut b)? {
        1 if b[0] == ctl::READY => Ok(()),
        _ => Err(io::Error::other("the child didn't start")),
    }
}

/// In a child started by [`fork`]: announce ourselves, wait to be handed a
/// process (or for the parent to go away), then run it.
pub fn run_child() -> Result<Exit, Error> {
    mark("child: main");
    let ctl_sock = unsafe { OwnedFd::from_raw_fd(CHILD_CTL_FD) };
    let bad = |what: &str| Error::Unexpected(format!("fork child: {what}"));
    // Idle, a spare belongs to its parent alone: a Ctrl-C or a signal to the
    // process group mustn't kill it.
    set_signals(libc::SIG_IGN);
    let inbox = ReceivePort::new()?;
    SendPort::inherited()?.send_port(&inbox.sender()?, std::process::id() as u64)?;
    let (fds, state) = match receive_state(&ctl_sock) {
        Ok(got) => got,
        // The parent exited (or exec'd away its need for us): done.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Exit::Code(0)),
        Err(e) => return Err(e.into()),
    };
    mark("child: state received");
    let mut entries = Vec::new();
    loop {
        let (port, more) = inbox.receive(SPARE_TIMEOUT_MS)?.ok_or_else(|| bad("no memory"))?;
        entries.push(port.into_memory_entry());
        if more == 0 {
            break;
        }
    }
    set_signals(libc::SIG_DFL);

    let mut d = Decoder(&state);
    let mut ctx = UserContext::decode(&mut d).map_err(|_| bad("CPU state"))?;
    let f = Fork::decode(&mut d).map_err(|_| bad("fork arguments"))?;
    let mut fds = fds.into_iter();
    let stderr = fds.next().ok_or_else(|| bad("no stderr"))?;
    unsafe { libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) };
    drop(stderr);
    let space = AddressSpace::decode_forked(&mut d, &entries, &mut fds).map_err(|_| bad("address space"))?;
    drop(entries);
    // Requests from the parent (to change our process group) come on the
    // control socket; a thread answers them while the guest runs.
    let requests = ctl_sock.try_clone()?;
    linux_abi::host_threads::spawn("parent requests", move || answer_parent(requests))?;
    let (mut process, mut thread) = Process::decode_forked(&mut d, fds.collect(), ctl_sock).map_err(|_| bad("process state"))?;
    let space = Space::new(space);
    process.forked_child(&mut thread, &space, &f);
    let space = space.into_inner();
    mark("child: rebuilt, ready");
    process.tell_parent(&[ctl::READY]);

    ctx.x[0] = 0;
    if f.stack != 0 {
        ctx.sp = f.stack;
    }
    if let Some(tls) = f.tls {
        ctx.tpidr = tls;
    }
    crate::execute(space, crate::Start::Resume(Box::new(ctx)), process, thread)
}

/// Answer the parent's requests until it goes away: `setpgid` on ourselves
/// (see `linux_abi::syscall::answer_setpgid`).
fn answer_parent(sock: OwnedFd) {
    let mut kind = [0u8];
    while matches!(read_some(&sock, &mut kind), Ok(1)) {
        if kind[0] == ctl::SETPGID {
            let mut arg = [0u8; 4];
            if read_exact(&sock, &mut arg).is_err() {
                return;
            }
            let _ = write_all(&sock, &linux_abi::syscall::answer_setpgid(arg));
        }
    }
}

/// Set every catchable signal to `disposition` (SIGPIPE stays ignored: the
/// runtime needs EPIPE).
fn set_signals(disposition: libc::sighandler_t) {
    for sig in 1..32 {
        if sig != libc::SIGKILL && sig != libc::SIGSTOP && sig != libc::SIGPIPE {
            unsafe { libc::signal(sig, disposition) };
        }
    }
}

fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut sv = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
    for fd in [&a, &b] {
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((a, b))
}

/// `posix_spawn` this executable as a fork child, with `sock` as its fd 3,
/// /dev/null as stdio (so an idle spare holds no pipe open), and nothing else
/// (POSIX_SPAWN_CLOEXEC_DEFAULT).
fn spawn_child(sock: RawFd) -> io::Result<i32> {
    let exe = CString::new(std::env::current_exe()?.into_os_string().into_encoded_bytes()).map_err(io::Error::other)?;
    let args = [c"macwl-run".to_owned(), c"--fork-child".to_owned()];
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv.push(std::ptr::null_mut());
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawnattr_init(&mut attr);
        libc::posix_spawn_file_actions_adddup2(&mut actions, sock, CHILD_CTL_FD);
        for fd in 0..3 {
            let mode = if fd == 0 { libc::O_RDONLY } else { libc::O_WRONLY };
            libc::posix_spawn_file_actions_addopen(&mut actions, fd, c"/dev/null".as_ptr(), mode, 0);
        }
        libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short);
        let mut pid = 0;
        let r = libc::posix_spawn(&mut pid, exe.as_ptr(), &actions, &attr, argv.as_ptr(), *libc::_NSGetEnviron());
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(pid)
    }
}

/// Header (fd count, state length), the fds in batches, then the state.
fn send_state(sock: &OwnedFd, fds: &[RawFd], state: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 16];
    header[..8].copy_from_slice(&(fds.len() as u64).to_le_bytes());
    header[8..].copy_from_slice(&(state.len() as u64).to_le_bytes());
    write_all(sock, &header)?;
    for batch in fds.chunks(FDS_PER_MSG) {
        send_fds(sock, batch)?;
    }
    write_all(sock, state)
}

fn receive_state(sock: &OwnedFd) -> io::Result<(Vec<OwnedFd>, Vec<u8>)> {
    let mut header = [0u8; 16];
    read_exact(sock, &mut header)?;
    let n_fds = u64::from_le_bytes(header[..8].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(header[8..].try_into().unwrap()) as usize;
    let mut fds = Vec::with_capacity(n_fds);
    while fds.len() < n_fds {
        recv_fds(sock, (n_fds - fds.len()).min(FDS_PER_MSG), &mut fds)?;
    }
    let mut state = vec![0u8; len];
    read_exact(sock, &mut state)?;
    Ok((fds, state))
}

fn send_fds(sock: &OwnedFd, fds: &[RawFd]) -> io::Result<()> {
    let bytes = std::mem::size_of_val(fds) as u32;
    let mut cbuf = vec![0u8; unsafe { libc::CMSG_SPACE(bytes) } as usize];
    let mut byte = [0u8];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
    }
    if unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) } != 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fds(sock: &OwnedFd, max: usize, out: &mut Vec<OwnedFd>) -> io::Result<()> {
    let bytes = (max * size_of::<RawFd>()) as u32;
    let mut cbuf = vec![0u8; unsafe { libc::CMSG_SPACE(bytes) } as usize];
    let mut byte = [0u8];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;
    if unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) } != 1 || msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("receiving file descriptors failed"));
    }
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                for i in 0..n {
                    out.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok(())
}

fn write_all(sock: &OwnedFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(sock.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

fn read_some(sock: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn read_exact(sock: &OwnedFd, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match read_some(sock, buf)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => buf = &mut buf[n..],
        }
    }
    Ok(())
}
