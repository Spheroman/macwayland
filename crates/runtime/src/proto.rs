//! The client protocol (docs/processes.md, section 1, "How a `macwl-run`
//! finds its host"): messages over a Unix stream socket, with fds alongside
//! (`SCM_RIGHTS`). Both ends are the same `macwl-run` binary, so there's no
//! versioning; bodies are `linux_abi::wire`, as fork's state transfer is.
//!
//! A message is a header (its body's length, its kind, how many fds came
//! with it) and its body. Fds arrive no later than the first byte of the
//! message they were sent with, so a reader keeps those it has been given
//! in order and hands each message the number its header says.

use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

use linux_abi::wire::{Decoder, Encoder, Truncated};

use crate::Exit;

/// Client to host: run a program (a [`crate::launch::Launch`], which of its
/// fds 0–2 come with it, and its terminal if it has one).
pub const RUN: u8 = 1;
/// Client to host: list the host's processes.
pub const PS: u8 = 2;
/// Client to host: bytes typed at the client's terminal.
pub const INPUT: u8 = 3;
/// Client to host: a signal from the Mac side (a Linux number), for the
/// client's process's group.
pub const SIGNAL: u8 = 4;
/// Client to host: the client's terminal has a new size.
pub const RESIZE: u8 = 5;
/// Client to host: the client's terminal has hung up.
pub const HANGUP: u8 = 6;
/// Host to coordinator, first: a host links (its Mac pid is the socket's
/// peer's).
pub const HELLO: u8 = 7;
/// Client to coordinator: every host's processes (answered with a
/// [`LISTING`]); the coordinator asks each host with a [`PS`].
pub const PS_ALL: u8 = 8;

/// Host to client: the process runs, with this pid (and its terminal's
/// master, if it has one).
pub const STARTED: u8 = 16;
/// Host to client: it couldn't be started (a message).
pub const FAILED: u8 = 17;
/// Host to client: the process has stopped, by this signal.
pub const STOPPED: u8 = 18;
/// Host to client: the process has ended (see [`encode_exit`]).
pub const EXITED: u8 = 19;
/// Host to client: the process listing (text).
pub const LISTING: u8 = 20;

/// The most a message may be: argv and environment fit easily.
const MAX_BODY: usize = 16 << 20;
/// The most fds a message carries (a run's three stdio fds; a Mac
/// program's inherited fds go to its agent in batches of this many).
pub const MAX_FDS: usize = 64;

/// Send a message on `sock` (blocking, or bounded by its SO_SNDTIMEO).
pub fn send(sock: RawFd, kind: u8, body: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    assert!(fds.len() <= MAX_FDS);
    let mut msg = Vec::with_capacity(6 + body.len());
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.push(kind);
    msg.push(fds.len() as u8);
    msg.extend_from_slice(body);
    let mut at = 0;
    let mut cmsg = [0u8; 64 + MAX_FDS * size_of::<RawFd>()];
    while at < msg.len() {
        let mut iov = libc::iovec { iov_base: msg[at..].as_ptr() as *mut _, iov_len: msg.len() - at };
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        // The fds go with the message's first bytes.
        if at == 0 && !fds.is_empty() {
            let len = std::mem::size_of_val(fds);
            let space = unsafe { libc::CMSG_SPACE(len as u32) } as usize;
            hdr.msg_control = cmsg.as_mut_ptr().cast();
            hdr.msg_controllen = space as _;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&hdr);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(len as u32) as _;
                std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(c), len);
            }
        }
        let n = unsafe { libc::sendmsg(sock, &hdr, 0) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        at += n as usize;
    }
    Ok(())
}

/// A message received: its kind, body, and the fds that came with it.
#[derive(Debug)]
pub struct Msg {
    pub kind: u8,
    pub body: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// What a socket has sent so far, as messages.
#[derive(Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

/// How a read went.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// Some bytes came.
    Some,
    /// Nothing yet (a non-blocking read).
    Nothing,
    /// The other end has gone.
    Eof,
}

impl Reader {
    /// Read what `sock` has (`nonblocking`: without waiting for it).
    pub fn fill(&mut self, sock: RawFd, nonblocking: bool) -> Read {
        let mut buf = [0u8; 65536];
        // Room for a few messages' fds at once.
        let mut cmsg = [0u8; 64 + 4 * MAX_FDS * size_of::<RawFd>()];
        loop {
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
            let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
            hdr.msg_iov = &mut iov;
            hdr.msg_iovlen = 1;
            hdr.msg_control = cmsg.as_mut_ptr().cast();
            hdr.msg_controllen = cmsg.len() as _;
            let n = unsafe { libc::recvmsg(sock, &mut hdr, if nonblocking { libc::MSG_DONTWAIT } else { 0 }) };
            if n < 0 {
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => return Read::Nothing,
                    _ => return Read::Eof,
                }
            }
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&hdr);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let data = libc::CMSG_DATA(c);
                        let count = ((*c).cmsg_len as usize - (data as usize - c as usize)) / size_of::<RawFd>();
                        for i in 0..count {
                            let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                            // macOS has no MSG_CMSG_CLOEXEC.
                            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                            self.fds.push_back(OwnedFd::from_raw_fd(fd));
                        }
                    }
                    c = libc::CMSG_NXTHDR(&hdr, c);
                }
            }
            if n == 0 {
                return Read::Eof;
            }
            self.buf.extend_from_slice(&buf[..n as usize]);
            return Read::Some;
        }
    }

    /// The next whole message, if one has come; Err if what came makes no
    /// sense.
    pub fn next(&mut self) -> Result<Option<Msg>, Truncated> {
        if self.buf.len() < 6 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        let (kind, nfds) = (self.buf[4], self.buf[5] as usize);
        if len > MAX_BODY || nfds > MAX_FDS {
            return Err(Truncated);
        }
        if self.buf.len() < 6 + len {
            return Ok(None);
        }
        if self.fds.len() < nfds {
            return Err(Truncated);
        }
        let body = self.buf[6..6 + len].to_vec();
        self.buf.drain(..6 + len);
        let fds = self.fds.drain(..nfds).collect();
        Ok(Some(Msg { kind, body, fds }))
    }

    /// Read `sock` until a whole message has come; None if it ended first.
    pub fn recv(&mut self, sock: RawFd) -> Option<Msg> {
        loop {
            match self.next() {
                Ok(Some(msg)) => return Some(msg),
                Ok(None) => {}
                Err(_) => return None,
            }
            if self.fill(sock, false) == Read::Eof {
                return None;
            }
        }
    }
}

/// How a process ended, for its client: an [`Exit`], or the runtime error
/// that ended it.
pub fn encode_exit(e: &mut Encoder, exit: &Result<Exit, String>) {
    match exit {
        Ok(Exit::Code(c)) => e.u64(0).i64(*c as i64),
        Ok(Exit::Signal(s)) => e.u64(1).i64(*s as i64),
        Ok(Exit::Fault { sig, esr, far, pc }) => e.u64(2).i64(*sig as i64).u64(*esr).u64(*far).u64(*pc),
        Ok(Exit::BusError { far, pc }) => e.u64(3).u64(*far).u64(*pc),
        Ok(Exit::OutOfMemory) => e.u64(4),
        Err(msg) => e.u64(5).bytes(msg.as_bytes()),
    };
}

pub fn decode_exit(d: &mut Decoder) -> Result<Result<Exit, String>, Truncated> {
    Ok(match d.u64()? {
        0 => Ok(Exit::Code(d.i64()? as i32)),
        1 => Ok(Exit::Signal(d.i64()? as i32)),
        2 => Ok(Exit::Fault { sig: d.i64()? as i32, esr: d.u64()?, far: d.u64()?, pc: d.u64()? }),
        3 => Ok(Exit::BusError { far: d.u64()?, pc: d.u64()? }),
        4 => Ok(Exit::OutOfMemory),
        5 => Err(String::from_utf8_lossy(&d.bytes()?).into_owned()),
        _ => return Err(Truncated),
    })
}

/// A Mac terminal's settings and size, as raw bytes (both ends are the same
/// binary on the same Mac).
pub fn encode_terminal(e: &mut Encoder, termios: &libc::termios, size: Option<libc::winsize>) {
    let raw = unsafe { std::slice::from_raw_parts((termios as *const libc::termios).cast::<u8>(), size_of::<libc::termios>()) };
    e.bytes(raw);
    e.bool(size.is_some());
    let ws = size.unwrap_or(libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 });
    for v in [ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel] {
        e.u64(v as u64);
    }
}

pub fn decode_terminal(d: &mut Decoder) -> Result<(libc::termios, Option<libc::winsize>), Truncated> {
    let raw = d.bytes()?;
    if raw.len() != size_of::<libc::termios>() {
        return Err(Truncated);
    }
    let termios = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast::<libc::termios>()) };
    let has_size = d.bool()?;
    let ws = decode_winsize(d)?;
    Ok((termios, has_size.then_some(ws)))
}

pub fn encode_winsize(e: &mut Encoder, ws: libc::winsize) {
    for v in [ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel] {
        e.u64(v as u64);
    }
}

pub fn decode_winsize(d: &mut Decoder) -> Result<libc::winsize, Truncated> {
    Ok(libc::winsize { ws_row: d.u64()? as u16, ws_col: d.u64()? as u16, ws_xpixel: d.u64()? as u16, ws_ypixel: d.u64()? as u16 })
}

/// The uid of the process at the other end of `sock`.
pub fn peer_uid(sock: &impl AsRawFd) -> Option<libc::uid_t> {
    let (mut uid, mut gid) = (0, 0);
    (unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

/// The Mac pid of the process at the other end of `sock`.
pub fn peer_pid(sock: &impl AsRawFd) -> Option<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(sock.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) };
    (r == 0).then_some(pid)
}
