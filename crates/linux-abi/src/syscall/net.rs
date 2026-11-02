//! Sockets. Unix-domain sockets are the priority (Wayland runs over one, with
//! buffers passed as `SCM_RIGHTS` fds); IPv4 and IPv6 addresses are
//! translated too. Linux and Darwin differ in `sockaddr` (Darwin has a length
//! byte), `cmsghdr` (16 vs 12 bytes, 8- vs 4-byte padding), message flags and
//! socket option numbers, so everything is translated at this boundary.

#![allow(clippy::too_many_arguments)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{gather, iov_len, iovecs, io::set_nonblocking, ok, scatter, Process, MAX_IO};
use crate::{
    abi::net::*,
    errno,
    fs::{FdEntry, Target},
    memory::{GuestMemory, GuestMemoryExt},
};

/// Room for any host socket address.
const SOCKADDR_MAX: usize = std::mem::size_of::<libc::sockaddr_storage>();
/// Most control data we'll copy in from the guest.
const CONTROL_MAX: u64 = 64 << 10;

fn ignore_sigpipe(host: i32) {
    // The runtime ignores SIGPIPE too; this makes it hold for each socket even
    // when this crate is used elsewhere (its tests, for one).
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(host, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&on as *const i32).cast(), 4);
    }
}

/// Install a new host socket as a guest fd, applying SOCK_* flags.
fn install(p: &Process, host: OwnedFd, flags: u64) -> Result<i64, i64> {
    ignore_sigpipe(host.as_raw_fd());
    // Set or clear: Darwin's accept inherits O_NONBLOCK from the listener.
    set_nonblocking(host.as_raw_fd(), flags & SOCK_NONBLOCK != 0)?;
    Ok(p.fds.insert(FdEntry::new(host).cloexec(flags & SOCK_CLOEXEC != 0)))
}

fn host_domain(domain: u64) -> Result<i32, i64> {
    match domain as u16 {
        AF_UNIX => Ok(libc::AF_UNIX),
        AF_INET => Ok(libc::AF_INET),
        AF_INET6 => Ok(libc::AF_INET6),
        _ => Err(errno::EAFNOSUPPORT),
    }
}

pub(super) fn socket(p: &Process, domain: u64, ty: u64, proto: u64) -> Result<i64, i64> {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    // SOCK_STREAM, SOCK_DGRAM, SOCK_RAW and SOCK_SEQPACKET match Darwin's.
    let fd = ok(unsafe { libc::socket(host_domain(domain)?, (ty & SOCK_TYPE_MASK) as i32, proto as i32) } as i64)?;
    install(p, unsafe { OwnedFd::from_raw_fd(fd as i32) }, ty)
}

/// SOCK_SEQPACKET (macOS has no Unix seqpacket sockets).
const SOCK_SEQPACKET: u64 = 5;

/// The SOCK_SEQPACKET sockets this process made (as host datagram sockets),
/// by inode, so as to say what they are. Passed to another process, they're
/// datagram sockets there, which is the same but for `SO_TYPE` and the
/// error a send to a closed peer gives.
static SEQPACKET: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

fn inode(host: i32) -> Option<u64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(host, &mut st) } == 0).then_some(st.st_ino)
}

/// Whether guest fd `fd` is a Unix datagram socket on the host (as
/// SOCK_SEQPACKET sockets are made).
pub(super) fn unix_dgram(p: &Process, fd: u64) -> bool {
    p.fds.host(fd).is_ok_and(host_unix_dgram)
}

fn host_unix_dgram(host: i32) -> bool {
    let (mut ty, mut len) = (0i32, 4u32);
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut alen = SOCKADDR_MAX as u32;
    unsafe {
        libc::getsockopt(host, libc::SOL_SOCKET, libc::SO_TYPE, (&mut ty as *mut i32).cast(), &mut len) == 0
            && ty == libc::SOCK_DGRAM
            && libc::getsockname(host, (&mut addr as *mut libc::sockaddr_storage).cast(), &mut alen) == 0
            && addr.ss_family as i32 == libc::AF_UNIX
    }
}

/// Whether guest fd `fd` is a SOCK_SEQPACKET socket this process made.
pub(super) fn is_seqpacket(p: &Process, fd: u64) -> bool {
    let Some(ino) = p.fds.host(fd).ok().and_then(inode) else { return false };
    SEQPACKET.lock().unwrap_or_else(|e| e.into_inner()).contains(&ino)
}

/// Send on `host` with `send`. A Unix datagram socket (a SOCK_SEQPACKET
/// one) whose peer's queue is full fails on macOS (ENOBUFS), where Linux's
/// seqpacket blocks until there's room: so wait, a little at a time (its
/// POLLOUT says nothing of the peer's queue), or EAGAIN if it mustn't.
pub(super) fn send_waiting(host: i32, dontwait: bool, mut send: impl FnMut() -> isize) -> Result<i64, i64> {
    let mut pause = std::time::Duration::from_micros(50);
    loop {
        match ok(send() as i64) {
            Err(errno::ENOBUFS) if host_unix_dgram(host) => {
                let nonblocking = unsafe { libc::fcntl(host, libc::F_GETFL) } & libc::O_NONBLOCK != 0;
                if dontwait || nonblocking {
                    return Err(errno::EAGAIN);
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(std::time::Duration::from_millis(5));
            }
            r => return r,
        }
    }
}

/// A pair of connected Unix sockets. SOCK_SEQPACKET (Firefox's sandbox
/// broker) is a host datagram pair: messages keep their boundaries, and fds
/// pass with them, as with seqpacket; a closed peer is told apart in
/// `dispatch` (reads see the end of the file).
pub(super) fn socketpair<M: GuestMemory + ?Sized>(p: &Process, mem: &M, domain: u64, ty: u64, proto: u64, sv: u64) -> Result<i64, i64> {
    if domain as u16 != AF_UNIX {
        return Err(errno::EAFNOSUPPORT);
    }
    let seqpacket = ty & SOCK_TYPE_MASK == SOCK_SEQPACKET;
    let host_ty = if seqpacket { libc::SOCK_DGRAM } else { (ty & SOCK_TYPE_MASK) as i32 };
    let mut host = [0; 2];
    ok(unsafe { libc::socketpair(libc::AF_UNIX, host_ty, proto as i32, host.as_mut_ptr()) } as i64)?;
    if seqpacket {
        let mut known = SEQPACKET.lock().unwrap_or_else(|e| e.into_inner());
        known.extend(host.iter().filter_map(|&h| inode(h)));
        // Linux's default buffers. macOS's (2 KiB) also cap a datagram's
        // size, and Firefox's fork server sends far bigger messages.
        for h in host {
            let (snd, rcv): (libc::c_int, libc::c_int) = (212_992, 2 * 212_992);
            unsafe {
                libc::setsockopt(h, libc::SOL_SOCKET, libc::SO_SNDBUF, (&snd as *const i32).cast(), 4);
                libc::setsockopt(h, libc::SOL_SOCKET, libc::SO_RCVBUF, (&rcv as *const i32).cast(), 4);
            }
        }
    }
    let owned = host.map(|h| unsafe { OwnedFd::from_raw_fd(h) });
    let mut nums = [0i32; 2];
    for (i, h) in owned.into_iter().enumerate() {
        nums[i] = install(p, h, ty)? as i32;
    }
    let bytes: Vec<u8> = nums.iter().flat_map(|n| n.to_le_bytes()).collect();
    if let Err(e) = mem.write(sv, &bytes) {
        for n in nums {
            p.fds.remove(n as u64).ok();
        }
        return Err(e.into());
    }
    Ok(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &[u8]) -> Option<Vec<u8>> {
    s.chunks(2)
        .map(|c| std::str::from_utf8(c).ok().and_then(|c| u8::from_str_radix(c, 16).ok()))
        .collect()
}

/// A guest `sockaddr` as a host one. Unix paths go through the guest
/// filesystem; abstract names (leading NUL, Linux-only) become files in the
/// runtime's private directory.
fn addr_to_host<M: GuestMemory + ?Sized>(p: &Process, mem: &M, addr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if !(2..=SOCKADDR_MAX as u64).contains(&len) {
        return Err(errno::EINVAL);
    }
    let mut raw = vec![0u8; len as usize];
    mem.read(addr, &mut raw)?;
    match u16::from_le_bytes([raw[0], raw[1]]) {
        AF_UNIX => {
            let path = &raw[2..];
            let host_path = if path.first() == Some(&0) {
                let dir = p.fs.private_dir("abstract")?;
                dir.join(hex(&path[1..])).into_os_string().into_encoded_bytes()
            } else {
                let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                if end == 0 {
                    return Err(errno::EINVAL);
                }
                // bind creates the last component, connect follows it.
                match p.fs.resolve(&p.fds, crate::abi::at::FDCWD as u64, &path[..end], true)?.target {
                    Target::Host(h) => h.into_bytes(),
                    Target::Proc(_) | Target::Shm(_) => return Err(errno::ECONNREFUSED),
                }
            };
            // Darwin's sun_path holds 104 bytes, NUL included.
            if host_path.len() >= 104 {
                return Err(errno::ENAMETOOLONG);
            }
            let mut out = vec![0u8, libc::AF_UNIX as u8];
            out.extend_from_slice(&host_path);
            out.push(0);
            out[0] = out.len() as u8;
            Ok(out)
        }
        AF_INET if len >= 16 => Ok([&[16, libc::AF_INET as u8], &raw[2..16]].concat()),
        AF_INET6 if len >= 28 => Ok([&[28, libc::AF_INET6 as u8], &raw[2..28]].concat()),
        AF_INET | AF_INET6 => Err(errno::EINVAL),
        _ => Err(errno::EAFNOSUPPORT),
    }
}

/// A host `sockaddr` (of `len` bytes) as the guest's.
fn addr_to_guest(p: &Process, host: &[u8]) -> Vec<u8> {
    if host.len() < 2 {
        return AF_UNIX.to_le_bytes().to_vec(); // unnamed
    }
    match host[1] as i32 {
        libc::AF_UNIX => {
            let path = &host[2..host.len().min(host[0].max(2) as usize)];
            let path = &path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())];
            let mut out = AF_UNIX.to_le_bytes().to_vec();
            if path.is_empty() {
                return out;
            }
            if let Ok(dir) = p.fs.private_dir("abstract") {
                let dir = dir.into_os_string().into_encoded_bytes();
                if let Some(name) = path.strip_prefix(dir.as_slice()).and_then(|n| n.strip_prefix(b"/")).and_then(unhex) {
                    out.push(0);
                    out.extend(name);
                    return out;
                }
            }
            out.extend(p.fs.guest_path_of_host(path));
            out.push(0);
            out
        }
        libc::AF_INET => [&AF_INET.to_le_bytes()[..], &host[2..16.min(host.len())]].concat(),
        libc::AF_INET6 => [&AF_INET6.to_le_bytes()[..], &host[2..28.min(host.len())]].concat(),
        _ => AF_UNIX.to_le_bytes().to_vec(),
    }
}

/// Store an address for the guest with Linux's in/out length convention:
/// copy at most `*len_ptr` bytes, then set `*len_ptr` to the full length.
fn put_addr<M: GuestMemory + ?Sized>(mem: &M, addr: u64, len_ptr: u64, bytes: &[u8]) -> Result<(), i64> {
    if addr == 0 || len_ptr == 0 {
        return Ok(());
    }
    let cap: u32 = mem.read_pod(len_ptr)?;
    mem.write(addr, &bytes[..bytes.len().min(cap as usize)])?;
    mem.write(len_ptr, &(bytes.len() as u32).to_le_bytes())?;
    Ok(())
}

pub(super) fn bind<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, addr: u64, len: u64) -> Result<i64, i64> {
    let host = addr_to_host(p, mem, addr, len)?;
    ok(unsafe { libc::bind(p.fds.host(fd)?, host.as_ptr().cast(), host.len() as u32) } as i64).map(|_| 0)
}

pub(super) fn connect<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, addr: u64, len: u64) -> Result<i64, i64> {
    let host = addr_to_host(p, mem, addr, len)?;
    ok(unsafe { libc::connect(p.fds.host(fd)?, host.as_ptr().cast(), host.len() as u32) } as i64).map(|_| 0)
}

pub(super) fn accept4<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, addr: u64, len_ptr: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let mut buf = [0u8; SOCKADDR_MAX];
    let mut len = buf.len() as u32;
    let host = ok(unsafe { libc::accept(p.fds.host(fd)?, buf.as_mut_ptr().cast(), &mut len) } as i64)? as i32;
    let n = install(p, unsafe { OwnedFd::from_raw_fd(host) }, flags)?;
    let guest = addr_to_guest(p, &buf[..len as usize]);
    if let Err(e) = put_addr(mem, addr, len_ptr, &guest) {
        p.fds.remove(n as u64).ok();
        return Err(e);
    }
    Ok(n)
}

pub(super) fn getname<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, addr: u64, len_ptr: u64, peer: bool) -> Result<i64, i64> {
    let mut buf = [0u8; SOCKADDR_MAX];
    let mut len = buf.len() as u32;
    let host = p.fds.host(fd)?;
    let r = if peer {
        unsafe { libc::getpeername(host, buf.as_mut_ptr().cast(), &mut len) }
    } else {
        unsafe { libc::getsockname(host, buf.as_mut_ptr().cast(), &mut len) }
    };
    ok(r as i64)?;
    put_addr(mem, addr, len_ptr, &addr_to_guest(p, &buf[..len as usize]))?;
    Ok(0)
}

/// Linux `MSG_*` flags for send/recv calls as Darwin's. MSG_NOSIGNAL needs
/// nothing (SIGPIPE is ignored); MSG_CMSG_CLOEXEC is applied when fds are
/// installed; MSG_TRUNC and MSG_MORE have no Darwin equivalent and are dropped.
fn msg_flags_to_host(flags: u64) -> i32 {
    let mut out = 0;
    for (linux, darwin) in [
        (MSG_OOB, libc::MSG_OOB),
        (MSG_PEEK, libc::MSG_PEEK),
        (MSG_DONTROUTE, libc::MSG_DONTROUTE),
        (MSG_DONTWAIT, libc::MSG_DONTWAIT),
        (MSG_EOR, libc::MSG_EOR),
        (MSG_WAITALL, libc::MSG_WAITALL),
    ] {
        if flags & linux != 0 {
            out |= darwin;
        }
    }
    out
}

/// Darwin `msg_flags` from `recvmsg` as Linux's.
fn msg_flags_to_guest(flags: i32) -> i32 {
    let mut out = 0;
    for (linux, darwin) in [(MSG_OOB, libc::MSG_OOB), (MSG_TRUNC, libc::MSG_TRUNC), (MSG_CTRUNC, libc::MSG_CTRUNC), (MSG_EOR, libc::MSG_EOR)] {
        if flags & darwin != 0 {
            out |= linux;
        }
    }
    out as i32
}

pub(super) fn sendto<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> Result<i64, i64> {
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    mem.read(buf, &mut data)?;
    let host_addr = if addr != 0 { Some(addr_to_host(p, mem, addr, addrlen)?) } else { None };
    let (ap, al) = host_addr.as_ref().map_or((std::ptr::null(), 0), |a| (a.as_ptr(), a.len() as u32));
    let host = p.fds.host(fd)?;
    send_waiting(host, flags & MSG_DONTWAIT != 0, || unsafe { libc::sendto(host, data.as_ptr().cast(), data.len(), msg_flags_to_host(flags), ap.cast(), al) })
}

pub(super) fn recvfrom<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, buf: u64, len: u64, flags: u64, addr: u64, len_ptr: u64) -> Result<i64, i64> {
    let mut data = vec![0u8; len.min(MAX_IO) as usize];
    let mut abuf = [0u8; SOCKADDR_MAX];
    let mut alen = abuf.len() as u32;
    let n = ok(unsafe {
        libc::recvfrom(p.fds.host(fd)?, data.as_mut_ptr().cast(), data.len(), msg_flags_to_host(flags), abuf.as_mut_ptr().cast(), &mut alen)
    } as i64)?;
    mem.write(buf, &data[..n as usize])?;
    put_addr(mem, addr, len_ptr, &addr_to_guest(p, &abuf[..alen as usize]))?;
    Ok(n)
}

/// The guest path of a directory fd received from another process.
fn received_dir_path(p: &Process, host: i32) -> Option<Vec<u8>> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(host, &mut st) } < 0 || st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return None;
    }
    Some(p.fs.guest_path_of_host(&super::path::host_fd_path(host)?))
}

/// Parse Linux control messages; only SCM_RIGHTS is supported. Returns the
/// host fds to pass.
fn guest_rights(p: &Process, control: &[u8]) -> Result<Vec<i32>, i64> {
    let mut fds = Vec::new();
    let mut off = 0;
    while off + CMSG_HDR as usize <= control.len() {
        let len = u64::from_le_bytes(control[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(control[off + 8..off + 12].try_into().unwrap());
        let kind = i32::from_le_bytes(control[off + 12..off + 16].try_into().unwrap());
        if len < CMSG_HDR as usize || off + len > control.len() {
            return Err(errno::EINVAL);
        }
        if (level, kind) != (SOL_SOCKET, SCM_RIGHTS) {
            p.unimplemented(format!("control message level {level} type {kind}"));
            return Err(errno::EINVAL);
        }
        for fd in control[off + CMSG_HDR as usize..off + len].chunks_exact(4) {
            fds.push(p.fds.host(i32::from_le_bytes(fd.try_into().unwrap()) as u64)?);
        }
        off += len.next_multiple_of(8);
    }
    Ok(fds)
}

/// Sockets in flight, kept reachable until they arrive.
///
/// XNU's garbage collector for fds in flight (`unp_gc`) gets nesting wrong:
/// a socket sent over a socket that is itself in flight (Firefox sends its
/// fork server one end of a pair, then a child's IPC socket through the
/// other) can be judged unreachable and flushed, and its holder then reads
/// the end of the file at once (the IPC socket), or gets no fd at all. So
/// the sender keeps its own reference to each socket it sends until the
/// message is received, and the message says when: it carries one more fd,
/// a receipt socket marked with [`RECEIPT_LOWAT`] (an option Linux programs
/// can't set), which the receiving runtime closes. The sender watches the
/// receipt's other end and lets go of its references at the end of file.
/// Host programs (the compositor) never see receipts: only sockets get one,
/// and no Wayland request passes a socket.
mod in_flight {
    use std::{
        collections::HashMap,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        sync::{Mutex, OnceLock},
    };

    /// `SO_SNDLOWAT` of a receipt.
    const RECEIPT_LOWAT: i32 = 1847;

    /// References kept until a message arrives, by the fd of the receipt's
    /// other end.
    static HELD: Mutex<Option<HashMap<i32, Held>>> = Mutex::new(None);

    struct Held {
        _watch: OwnedFd,
        _sockets: Vec<OwnedFd>,
    }

    fn is_socket(fd: i32) -> bool {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        unsafe { libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK }
    }

    /// Whether `fd`, just received, is a receipt (to close, not deliver).
    pub(super) fn is_receipt(fd: i32) -> bool {
        let mut v: libc::c_int = 0;
        let mut len = 4 as libc::socklen_t;
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDLOWAT, (&mut v as *mut i32).cast(), &mut len) == 0 && v == RECEIPT_LOWAT }
    }

    /// A message's receipt, made when it carries sockets.
    pub(super) struct Receipt {
        /// Goes with the message.
        pub(super) sent: OwnedFd,
        watch: OwnedFd,
        sockets: Vec<OwnedFd>,
    }

    impl Receipt {
        pub(super) fn new(fds: &[i32]) -> Option<Receipt> {
            let sockets: Vec<OwnedFd> = fds
                .iter()
                .filter(|&&fd| is_socket(fd))
                .filter_map(|&fd| {
                    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                    (dup >= 0).then(|| unsafe { OwnedFd::from_raw_fd(dup) })
                })
                .collect();
            if sockets.is_empty() {
                return None;
            }
            let mut pair = [0; 2];
            if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
                return None;
            }
            let (sent, watch) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
            for fd in pair {
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            let lowat = RECEIPT_LOWAT;
            unsafe { libc::setsockopt(sent.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDLOWAT, (&lowat as *const i32).cast(), 4) };
            Some(Receipt { sent, watch, sockets })
        }

        /// The message went: hold its sockets until the receipt closes.
        pub(super) fn sent(self) {
            let Some(kq) = watcher() else { return };
            let watch = self.watch.as_raw_fd();
            HELD.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(
                watch,
                Held { _watch: self.watch, _sockets: self.sockets },
            );
            let ev = libc::kevent { ident: watch as usize, filter: libc::EVFILT_READ, flags: libc::EV_ADD | libc::EV_ONESHOT, fflags: 0, data: 0, udata: std::ptr::null_mut() };
            if unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
                release(watch);
            }
        }
    }

    fn release(watch: i32) {
        if let Some(held) = HELD.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            held.remove(&watch);
        }
    }

    /// The kqueue a thread waits on for receipts closing.
    fn watcher() -> Option<i32> {
        static KQ: OnceLock<Option<i32>> = OnceLock::new();
        *KQ.get_or_init(|| {
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return None;
            }
            std::thread::Builder::new()
                .name("receipts".into())
                .spawn(move || loop {
                    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                    if unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) } == 1 {
                        // Nothing is written to a receipt: readable is closed.
                        release(ev.ident as i32);
                    }
                })
                .ok()?;
            Some(kq)
        })
    }
}

pub(super) fn sendmsg<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, msg: u64, flags: u64) -> Result<i64, i64> {
    let m: MsgHdr = mem.read_pod(msg)?;
    let data = gather(mem, &iovecs(mem, m.iov, m.iovlen)?)?;
    let name = if m.name != 0 && m.namelen > 0 { Some(addr_to_host(p, mem, m.name, m.namelen as u64)?) } else { None };
    let fds = if m.control != 0 && m.controllen > 0 {
        if m.controllen > CONTROL_MAX {
            return Err(errno::ENOBUFS);
        }
        let mut control = vec![0u8; m.controllen as usize];
        mem.read(m.control, &mut control)?;
        guest_rights(p, &control)?
    } else {
        Vec::new()
    };
    let receipt = in_flight::Receipt::new(&fds);
    let fds = match &receipt {
        Some(r) => fds.into_iter().chain([r.sent.as_raw_fd()]).collect(),
        None => fds,
    };

    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if let Some(name) = &name {
        hdr.msg_name = name.as_ptr() as *mut _;
        hdr.msg_namelen = name.len() as u32;
    }
    // u64s keep the buffer aligned for cmsghdr.
    let fd_bytes = (fds.len() * 4) as u32;
    let mut control = vec![0u64; (unsafe { libc::CMSG_SPACE(fd_bytes) } as usize).div_ceil(8)];
    if !fds.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = unsafe { libc::CMSG_SPACE(fd_bytes) };
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_len = libc::CMSG_LEN(fd_bytes);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<i32>(), fds.len());
        }
    }
    let host = p.fds.host(fd)?;
    let n = send_waiting(host, flags & MSG_DONTWAIT != 0, || unsafe { libc::sendmsg(host, &hdr, msg_flags_to_host(flags)) })?;
    if let Some(r) = receipt {
        r.sent();
    }
    Ok(n)
}

/// `struct mmsghdr` on arm64: a `msghdr`, then the bytes sent or received.
const MMSGHDR: u64 = 64;
const MSG_LEN: u64 = 56;
const MSG_WAITFORONE: u64 = 0x10000;

/// `sendmmsg`: `sendmsg` for each, as far as they go. An error after the
/// first is left for the next call, as Linux does.
pub(super) fn sendmmsg<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, msgs: u64, n: u64, flags: u64) -> Result<i64, i64> {
    let n = n.min(1024);
    for i in 0..n {
        let at = msgs + i * MMSGHDR;
        match sendmsg(p, mem, fd, at, flags) {
            Ok(len) => mem.write_pod(at + MSG_LEN, &(len as u32))?,
            Err(e) if i == 0 => return Err(e),
            Err(_) => return Ok(i as i64),
        }
    }
    Ok(n as i64)
}

/// `recvmmsg`: `recvmsg` for each, as far as there's something to take;
/// with MSG_WAITFORONE, only the first waits. (Its timeout isn't kept.)
pub(super) fn recvmmsg<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, msgs: u64, n: u64, flags: u64) -> Result<i64, i64> {
    let n = n.min(1024);
    for i in 0..n {
        let at = msgs + i * MMSGHDR;
        let f = if i > 0 && flags & MSG_WAITFORONE != 0 { flags | MSG_DONTWAIT } else { flags } & !MSG_WAITFORONE;
        match recvmsg(p, mem, fd, at, f) {
            Ok(len) => mem.write_pod(at + MSG_LEN, &(len as u32))?,
            Err(e) if i == 0 => return Err(e),
            Err(_) => return Ok(i as i64),
        }
    }
    Ok(n as i64)
}

pub(super) fn recvmsg<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, msg: u64, flags: u64) -> Result<i64, i64> {
    let mut m: MsgHdr = mem.read_pod(msg)?;
    let iov = iovecs(mem, m.iov, m.iovlen)?;
    let mut data = vec![0u8; iov_len(&iov) as usize];
    let mut name = [0u8; SOCKADDR_MAX];
    // Always take up to Linux's per-message maximum from the host, then keep
    // what fits in the guest's buffer: Linux closes the rest and sets
    // MSG_CTRUNC, but Darwin, given no control buffer, drops fds silently.
    const SCM_MAX_FD: u32 = 253;
    let guest_cap = if m.control != 0 { m.controllen.min(CONTROL_MAX) } else { 0 };
    let max_fds = (guest_cap.saturating_sub(CMSG_HDR) / 4) as usize;
    let host_space = unsafe { libc::CMSG_SPACE(SCM_MAX_FD * 4) };
    let mut control = vec![0u64; (host_space as usize).div_ceil(8)];

    let mut hiov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut hiov;
    hdr.msg_iovlen = 1;
    if m.name != 0 {
        hdr.msg_name = name.as_mut_ptr().cast();
        hdr.msg_namelen = name.len() as u32;
    }
    hdr.msg_control = control.as_mut_ptr().cast();
    hdr.msg_controllen = host_space;
    let n = ok(unsafe { libc::recvmsg(p.fds.host(fd)?, &mut hdr, msg_flags_to_host(flags)) } as i64)?;

    // Received fds become guest fds, as many as fit.
    let mut received = Vec::new();
    let mut truncated = false;
    if hdr.msg_controllen > 0 {
        let mut c = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
        while !c.is_null() {
            let (level, kind, len) = unsafe { ((*c).cmsg_level, (*c).cmsg_type, (*c).cmsg_len) };
            if (level, kind) == (libc::SOL_SOCKET, libc::SCM_RIGHTS) {
                let count = (len as usize - unsafe { libc::CMSG_LEN(0) } as usize) / 4;
                let first = unsafe { libc::CMSG_DATA(c).cast::<i32>() };
                for i in 0..count {
                    let host = unsafe { OwnedFd::from_raw_fd(first.add(i).read_unaligned()) };
                    if in_flight::is_receipt(host.as_raw_fd()) {
                        continue; // dropping it tells the sender
                    }
                    if received.len() == max_fds {
                        truncated = true; // dropping `host` closes it
                        continue;
                    }
                    let mut entry = FdEntry::new(host).cloexec(flags & MSG_CMSG_CLOEXEC != 0);
                    // A directory needs its path to be a dirfd or the target
                    // of fchdir: wineserver opens directories for its
                    // clients, which list them after an fchdir.
                    if let Some(guest) = received_dir_path(p, entry.host.as_raw_fd()) {
                        entry = entry.path(guest);
                    }
                    received.push(p.fds.insert(entry) as i32);
                }
            }
            c = unsafe { libc::CMSG_NXTHDR(&hdr, c) };
        }
    }
    let mut controllen = 0;
    if !received.is_empty() {
        let len = CMSG_HDR + received.len() as u64 * 4;
        let mut out = Vec::with_capacity(len.next_multiple_of(8) as usize);
        out.extend(len.to_le_bytes());
        out.extend(SOL_SOCKET.to_le_bytes());
        out.extend(SCM_RIGHTS.to_le_bytes());
        out.extend(received.iter().flat_map(|f| f.to_le_bytes()));
        out.resize((len.next_multiple_of(8)).min(guest_cap) as usize, 0);
        mem.write(m.control, &out)?;
        controllen = out.len() as u64;
    }

    scatter(mem, &iov, &data[..n as usize])?;
    if m.name != 0 {
        let guest = addr_to_guest(p, &name[..hdr.msg_namelen as usize]);
        let cap = m.namelen as usize;
        mem.write(m.name, &guest[..guest.len().min(cap)])?;
        m.namelen = guest.len() as u32;
    }
    m.controllen = controllen;
    m.flags = msg_flags_to_guest(hdr.msg_flags) | if truncated { MSG_CTRUNC as i32 } else { 0 };
    mem.write_pod(msg, &m)?;
    Ok(n)
}

/// A socket option the guest names, as the host's (level, name).
fn host_option(level: u64, name: u64) -> Option<(i32, i32)> {
    Some(match (level as i32, name) {
        (SOL_SOCKET, SO_REUSEADDR) => (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        (SOL_SOCKET, SO_TYPE) => (libc::SOL_SOCKET, libc::SO_TYPE),
        (SOL_SOCKET, SO_ERROR) => (libc::SOL_SOCKET, libc::SO_ERROR),
        (SOL_SOCKET, SO_BROADCAST) => (libc::SOL_SOCKET, libc::SO_BROADCAST),
        (SOL_SOCKET, SO_SNDBUF) => (libc::SOL_SOCKET, libc::SO_SNDBUF),
        (SOL_SOCKET, SO_RCVBUF) => (libc::SOL_SOCKET, libc::SO_RCVBUF),
        (SOL_SOCKET, SO_KEEPALIVE) => (libc::SOL_SOCKET, libc::SO_KEEPALIVE),
        // struct timeval: Linux {i64, i64}, Darwin {i64, i32, pad}; the same
        // bytes on little-endian for any valid usec.
        (SOL_SOCKET, SO_RCVTIMEO) => (libc::SOL_SOCKET, libc::SO_RCVTIMEO),
        (SOL_SOCKET, SO_SNDTIMEO) => (libc::SOL_SOCKET, libc::SO_SNDTIMEO),
        (l, TCP_NODELAY) if l as u64 == IPPROTO_TCP => (libc::IPPROTO_TCP, libc::TCP_NODELAY),
        (l, IPV6_V6ONLY) if l as u64 == IPPROTO_IPV6 => (libc::IPPROTO_IPV6, libc::IPV6_V6ONLY),
        _ => return None,
    })
}

pub(super) fn setsockopt<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, level: u64, name: u64, val: u64, len: u64) -> Result<i64, i64> {
    let host = p.fds.host(fd)?;
    if (level as i32, name) == (SOL_SOCKET, SO_PASSCRED) {
        // Only affects receiving SCM_CREDENTIALS, which we never deliver.
        return Ok(0);
    }
    // ICMP errors on an error queue (MSG_ERRQUEUE), which Darwin hasn't:
    // taken as asked, and never queued. glibc's resolver asks, and gives up
    // on DNS if it can't; a refused port still shows, as ECONNREFUSED from
    // the next recv on a connected socket, which is what it looks for.
    if matches!((level, name), (IPPROTO_IP, IP_RECVERR) | (IPPROTO_IPV6, IPV6_RECVERR)) {
        return Ok(0);
    }
    let Some((hl, hn)) = host_option(level, name) else {
        p.unimplemented(format!("setsockopt level {level} option {name}"));
        return Err(errno::ENOPROTOOPT);
    };
    if len > 64 {
        return Err(errno::EINVAL);
    }
    let mut buf = vec![0u8; len as usize];
    mem.read(val, &mut buf)?;
    ok(unsafe { libc::setsockopt(host, hl, hn, buf.as_ptr().cast(), len as u32) } as i64).map(|_| 0)
}

pub(super) fn getsockopt<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, level: u64, name: u64, val: u64, len_ptr: u64) -> Result<i64, i64> {
    let host = p.fds.host(fd)?;
    let cap: u32 = mem.read_pod(len_ptr)?;
    let value: Vec<u8> = match (level as i32, name) {
        (SOL_SOCKET, SO_PEERCRED) => {
            let (mut uid, mut gid) = (0, 0);
            ok(unsafe { libc::getpeereid(host, &mut uid, &mut gid) } as i64)?;
            let mut pid: libc::pid_t = 0;
            let mut plen = 4u32;
            ok(unsafe { libc::getsockopt(host, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&mut pid as *mut i32).cast(), &mut plen) } as i64)?;
            let cred = Ucred { pid, uid, gid };
            unsafe { crate::abi::as_bytes(&cred) }.to_vec()
        }
        (SOL_SOCKET, SO_PASSCRED) => 0i32.to_le_bytes().to_vec(),
        _ if matches!((level, name), (IPPROTO_IP, IP_RECVERR) | (IPPROTO_IPV6, IPV6_RECVERR)) => 0i32.to_le_bytes().to_vec(),
        _ => {
            let Some((hl, hn)) = host_option(level, name) else {
                p.unimplemented(format!("getsockopt level {level} option {name}"));
                return Err(errno::ENOPROTOOPT);
            };
            let mut buf = [0u8; 64];
            let mut blen = buf.len() as u32;
            ok(unsafe { libc::getsockopt(host, hl, hn, buf.as_mut_ptr().cast(), &mut blen) } as i64)?;
            let mut v = buf[..blen as usize].to_vec();
            if (level as i32, name) == (SOL_SOCKET, SO_TYPE) && v.len() >= 4 && is_seqpacket(p, fd) {
                v[..4].copy_from_slice(&(SOCK_SEQPACKET as i32).to_le_bytes());
            }
            if (level as i32, name) == (SOL_SOCKET, SO_ERROR) && v.len() >= 4 {
                let e = i32::from_le_bytes(v[..4].try_into().unwrap());
                let linux = if e == 0 { 0 } else { errno::from_darwin(e) as i32 };
                v[..4].copy_from_slice(&linux.to_le_bytes());
            }
            v
        }
    };
    let n = value.len().min(cap as usize);
    mem.write(val, &value[..n])?;
    mem.write(len_ptr, &(n as u32).to_le_bytes())?;
    Ok(0)
}
