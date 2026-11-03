//! Dispatcher tests against a flat fake guest memory and a temporary root.

use std::{
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::Path,
};

use super::*;
use crate::{
    abi::{self, at, fcntl},
    memory::Fault,
    nr,
};

const BASE: u64 = 0x10000;
const FDCWD: u64 = at::FDCWD as u64;
const O_RDONLY: u64 = 0;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_DIRECTORY: u64 = 0o40000; // arm64 values differ from x86-64
const O_NOFOLLOW: u64 = 0o100000;
const O_CLOEXEC: u64 = 0o2000000;

/// Guest memory: one flat buffer at `BASE`.
struct FlatMem(std::cell::RefCell<Vec<u8>>);

impl GuestMemory for FlatMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        let m = self.0.borrow();
        let off = addr.checked_sub(BASE).filter(|o| o + buf.len() as u64 <= m.len() as u64).ok_or(Fault { addr })?;
        buf.copy_from_slice(&m[off as usize..off as usize + buf.len()]);
        Ok(())
    }
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        let mut m = self.0.borrow_mut();
        let off = addr.checked_sub(BASE).filter(|o| o + data.len() as u64 <= m.len() as u64).ok_or(Fault { addr })?;
        m[off as usize..off as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn brk(&self, _: u64) -> u64 {
        0
    }
    fn mmap_anonymous(&self, _: u64, _: u64, _: u64, _: u64) -> i64 {
        -errno::ENOSYS
    }
    fn mmap_shared(&self, _: u64, _: u64, _: u64, _: u64, _: Option<i32>, _: u64) -> i64 {
        -errno::ENOSYS
    }
    fn munmap(&self, _: u64, _: u64) -> i64 {
        0
    }
    fn mprotect(&self, _: u64, _: u64, _: u64) -> i64 {
        0
    }
}

/// A process with a temporary root, plus a bump allocator in guest memory.
struct T {
    p: Process,
    th: Thread,
    m: FlatMem,
    top: u64,
    root: tempfile::TempDir,
}

impl T {
    fn new() -> T {
        let root = tempfile::tempdir().unwrap();
        let p = Process::new(root.path());
        let th = Thread::main(&p);
        T { p, th, m: FlatMem(std::cell::RefCell::new(vec![0; 1 << 20])), top: BASE, root }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn buf(&mut self, len: u64) -> u64 {
        let a = self.top;
        self.top += len.next_multiple_of(16);
        a
    }

    fn put(&mut self, bytes: &[u8]) -> u64 {
        let a = self.buf(bytes.len() as u64);
        self.m.write(a, bytes).unwrap();
        a
    }

    fn s(&mut self, s: &str) -> u64 {
        self.put(&[s.as_bytes(), b"\0"].concat())
    }

    fn call(&mut self, nr: u64, args: &[u64]) -> i64 {
        let mut a = [0; 6];
        a[..args.len()].copy_from_slice(args);
        match dispatch(&self.p, &mut self.th, &self.m, nr, a) {
            Action::Return(v) => v,
            Action::Exit(c) | Action::ExitThread(c) => panic!("exited with {c}"),
            Action::SigReturn => panic!("rt_sigreturn wants the core"),
            a @ (Action::Fork(_) | Action::Exec(_) | Action::ExecHost(_) | Action::MacStandIn(_) | Action::Spawn(_)) => panic!("{} wants the core: {a:?}", nr::name(nr)),
        }
    }

    /// Like `call`, but the call must succeed.
    fn ok(&mut self, nr: u64, args: &[u64]) -> u64 {
        let r = self.call(nr, args);
        assert!(r >= 0, "{} failed: errno {}", nr::name(nr), -r);
        r as u64
    }

    fn get(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0; len];
        self.m.read(addr, &mut b).unwrap();
        b
    }

    fn u32_at(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.get(addr, 4).try_into().unwrap())
    }

    fn open(&mut self, path: &str, flags: u64) -> u64 {
        let p = self.s(path);
        self.ok(nr::OPENAT, &[FDCWD, p, flags, 0o644])
    }

    fn write_str(&mut self, fd: u64, s: &str) {
        let b = self.put(s.as_bytes());
        assert_eq!(self.ok(nr::WRITE, &[fd, b, s.len() as u64]), s.len() as u64);
    }

    fn read_n(&mut self, fd: u64, n: u64) -> Vec<u8> {
        let b = self.buf(n);
        let got = self.ok(nr::READ, &[fd, b, n]);
        self.get(b, got as usize)
    }

    fn readlink(&mut self, path: &str) -> Vec<u8> {
        let p = self.s(path);
        let b = self.buf(256);
        let n = self.ok(nr::READLINKAT, &[FDCWD, p, b, 256]);
        self.get(b, n as usize)
    }

    fn stat_mode(&mut self, path: &str, flags: u64) -> u32 {
        let p = self.s(path);
        let b = self.buf(128);
        self.ok(nr::NEWFSTATAT, &[FDCWD, p, b, flags]);
        self.u32_at(b + 16)
    }
}

#[test]
fn file_roundtrip() {
    let mut t = T::new();
    let fd = t.open("/f", O_CREAT | O_RDWR | O_CLOEXEC);
    t.write_str(fd, "hello world");
    assert_eq!(t.ok(nr::LSEEK, &[fd, 0, 0]), 0);
    assert_eq!(t.read_n(fd, 5), b"hello");
    let b = t.buf(4);
    assert_eq!(t.ok(nr::PREAD64, &[fd, b, 4, 6]), 4);
    assert_eq!(t.get(b, 4), b"worl");
    let st = t.buf(128);
    t.ok(nr::FSTAT, &[fd, st]);
    assert_eq!(u64::from_le_bytes(t.get(st + 48, 8).try_into().unwrap()), 11, "st_size");
    assert_eq!(t.ok(nr::FCNTL, &[fd, fcntl::GETFD]), 1, "O_CLOEXEC");
    t.ok(nr::CLOSE, &[fd]);
    assert_eq!(t.call(nr::CLOSE, &[fd]), -errno::EBADF);
    assert_eq!(std::fs::read(t.root().join("f")).unwrap(), b"hello world");
    let beyond = t.m.0.borrow().len() as u64 * 4;
    assert_eq!(t.call(nr::OPENAT, &[FDCWD, beyond, 0]), -errno::EFAULT);
}

#[test]
fn directories_and_getdents() {
    let mut t = T::new();
    let d = t.s("/d");
    t.ok(nr::MKDIRAT, &[FDCWD, d, 0o755]);
    for name in ["/d/a", "/d/b"] {
        let fd = t.open(name, O_CREAT | O_RDWR);
        t.ok(nr::CLOSE, &[fd]);
    }
    let target = t.s("/d/a");
    let link = t.s("/d/l");
    t.ok(nr::SYMLINKAT, &[target, FDCWD, link]);

    let dir = t.open("/d", O_RDONLY | O_DIRECTORY);
    let read_all = |t: &mut T| {
        let mut names = Vec::new();
        let buf = t.buf(48); // small: forces several calls
        loop {
            let n = t.ok(nr::GETDENTS64, &[dir, buf, 48]);
            if n == 0 {
                break;
            }
            let data = t.get(buf, n as usize);
            let mut off = 0;
            while off < data.len() {
                let reclen = u16::from_le_bytes([data[off + 16], data[off + 17]]) as usize;
                let kind = data[off + 18];
                let name = &data[off + 19..off + reclen];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap()];
                names.push((String::from_utf8(name.to_vec()).unwrap(), kind));
                off += reclen;
            }
        }
        names.sort();
        names
    };
    let names = read_all(&mut t);
    let expect = [(".", 4), ("..", 4), ("a", 8), ("b", 8), ("l", 10)].map(|(n, k)| (n.to_string(), k));
    assert_eq!(names, expect);
    assert_eq!(t.ok(nr::LSEEK, &[dir, 0, 0]), 0, "rewind");
    assert_eq!(read_all(&mut t), expect);
    let tiny = t.buf(8);
    t.ok(nr::LSEEK, &[dir, 0, 0]);
    assert_eq!(t.call(nr::GETDENTS64, &[dir, tiny, 8]), -errno::EINVAL);

    let d = t.s("/d");
    assert_eq!(t.call(nr::UNLINKAT, &[FDCWD, d, at::REMOVEDIR]), -errno::ENOTEMPTY);
}

#[test]
fn symlinks_stay_inside_the_root() {
    let mut t = T::new();
    std::fs::create_dir(t.root().join("etc")).unwrap();
    std::fs::write(t.root().join("etc/hosts"), b"guest hosts").unwrap();
    let target = t.s("/etc/hosts");
    let link = t.s("/hosts");
    t.ok(nr::SYMLINKAT, &[target, FDCWD, link]);
    assert_eq!(t.readlink("/hosts"), b"/etc/hosts");
    // The host's /etc/hosts would say something else entirely.
    let fd = t.open("/hosts", O_RDONLY);
    assert_eq!(t.read_n(fd, 64), b"guest hosts");
    assert_eq!(t.stat_mode("/hosts", at::SYMLINK_NOFOLLOW) & 0o170000, 0o120000);
    assert_eq!(t.stat_mode("/hosts", 0) & 0o170000, 0o100000);
    // O_NOFOLLOW on a symlink fails with ELOOP, as on Linux.
    let p = t.s("/hosts");
    assert_eq!(t.call(nr::OPENAT, &[FDCWD, p, O_RDONLY | O_NOFOLLOW]), -errno::ELOOP);
}

#[test]
fn rename_unlink_access() {
    let mut t = T::new();
    std::fs::write(t.root().join("x"), b"x").unwrap();
    std::fs::write(t.root().join("y"), b"y").unwrap();
    let (x, y, z) = (t.s("/x"), t.s("/y"), t.s("/z"));
    assert_eq!(t.call(nr::RENAMEAT2, &[FDCWD, x, FDCWD, y, fcntl::RENAME_NOREPLACE]), -errno::EEXIST);
    t.ok(nr::RENAMEAT2, &[FDCWD, x, FDCWD, y, fcntl::RENAME_EXCHANGE]);
    assert_eq!(std::fs::read(t.root().join("x")).unwrap(), b"y");
    t.ok(nr::RENAMEAT, &[FDCWD, x, FDCWD, z]);
    assert_eq!(t.call(nr::FACCESSAT, &[FDCWD, x, 0]), -errno::ENOENT);
    assert_eq!(t.ok(nr::FACCESSAT, &[FDCWD, z, 4]), 0);
    t.ok(nr::UNLINKAT, &[FDCWD, z, 0]);
    assert!(!t.root().join("z").exists());
}

#[test]
fn working_directory() {
    let mut t = T::new();
    std::fs::create_dir_all(t.root().join("home/user")).unwrap();
    std::fs::write(t.root().join("home/user/notes"), b"n").unwrap();
    let home = t.s("/home/user");
    t.ok(nr::CHDIR, &[home]);
    let b = t.buf(64);
    assert_eq!(t.ok(nr::GETCWD, &[b, 64]), 11);
    assert_eq!(t.get(b, 11), b"/home/user\0");
    assert_eq!(t.call(nr::GETCWD, &[b, 5]), -errno::ERANGE);
    let fd = t.open("notes", O_RDONLY);
    assert_eq!(t.read_n(fd, 8), b"n");
    // *at() relative to a directory fd.
    let root = t.open("/", O_RDONLY | O_DIRECTORY);
    let rel = t.s("home/user/notes");
    let fd = t.ok(nr::OPENAT, &[root, rel, O_RDONLY]);
    assert_eq!(t.read_n(fd, 8), b"n");
    t.ok(nr::FCHDIR, &[root]);
    t.ok(nr::GETCWD, &[b, 64]);
    assert_eq!(t.get(b, 2), b"/\0");
    let notes = t.s("/home/user/notes");
    assert_eq!(t.call(nr::CHDIR, &[notes]), -errno::ENOTDIR);
}

#[test]
fn fd_juggling_pipes_and_poll() {
    let mut t = T::new();
    let fds = t.buf(8);
    t.ok(nr::PIPE2, &[fds, O_CLOEXEC]);
    let (r, w) = (t.u32_at(fds) as u64, t.u32_at(fds + 4) as u64);
    assert_eq!(t.ok(nr::FCNTL, &[r, fcntl::GETFD]), 1);

    let d = t.ok(nr::DUP, &[w]);
    assert_eq!(t.ok(nr::FCNTL, &[d, fcntl::GETFD]), 0, "dup clears cloexec");
    assert_eq!(t.ok(nr::DUP3, &[w, 10, O_CLOEXEC]), 10);
    assert_eq!(t.call(nr::DUP3, &[w, w, 0]), -errno::EINVAL);
    assert_eq!(t.ok(nr::FCNTL, &[w, fcntl::DUPFD, 20]), 20);

    t.ok(nr::FCNTL, &[r, fcntl::SETFL, 0o4000]);
    assert_ne!(t.ok(nr::FCNTL, &[r, fcntl::GETFL]) & 0o4000, 0, "O_NONBLOCK");
    let b = t.buf(4);
    assert_eq!(t.call(nr::READ, &[r, b, 4]), -errno::EAGAIN);

    // ppoll: nothing to read, then something.
    let zero = t.put(&[0; 16]);
    let pfd = t.put(&[&(r as i32).to_le_bytes()[..], &1i16.to_le_bytes(), &0i16.to_le_bytes(), &(99i32).to_le_bytes(), &1i16.to_le_bytes(), &0i16.to_le_bytes()].concat());
    assert_eq!(t.ok(nr::PPOLL, &[pfd, 1, zero]), 0);
    t.write_str(10, "ping");
    assert_eq!(t.ok(nr::PPOLL, &[pfd, 2, zero]), 2, "readable + POLLNVAL for fd 99");
    assert_eq!(t.get(pfd + 6, 2), 1i16.to_le_bytes());
    assert_eq!(t.get(pfd + 14, 2), (libc::POLLNVAL).to_le_bytes());
    // Asked for POLLPRI too, a readable pipe is only readable (as on Linux;
    // Darwin would say POLLPRI as well).
    let pri = t.put(&[&(r as i32).to_le_bytes()[..], &3i16.to_le_bytes(), &0i16.to_le_bytes()].concat());
    assert_eq!(t.ok(nr::PPOLL, &[pri, 1, zero]), 1);
    assert_eq!(t.get(pri + 6, 2), 1i16.to_le_bytes());
    assert_eq!(t.read_n(r, 16), b"ping");
}

#[test]
fn flock_and_scheduling_queries() {
    let mut t = T::new();
    let (a, b) = (t.open("/lock", O_CREAT | O_RDWR), t.open("/lock", O_RDWR));
    let (sh, ex, nb, un) = (1, 2, 4, 8);
    assert_eq!(t.ok(nr::FLOCK, &[a, ex]), 0);
    assert_eq!(t.call(nr::FLOCK, &[b, sh | nb]), -errno::EAGAIN, "held by the other open file");
    assert_eq!(t.ok(nr::FLOCK, &[a, un]), 0);
    assert_eq!(t.ok(nr::FLOCK, &[b, sh | nb]), 0);

    assert_eq!(t.ok(nr::GETPRIORITY, &[0, 0]), 20, "nice 0");
    assert_eq!(t.ok(nr::SCHED_GETSCHEDULER, &[0]), 0, "SCHED_OTHER");
    assert_eq!(t.ok(nr::SCHED_GET_PRIORITY_MAX, &[1]), 99);
    assert_eq!(t.ok(nr::SCHED_GET_PRIORITY_MIN, &[0]), 0);
}

#[test]
fn memfd_and_proc() {
    let mut t = T::new();
    for _ in 0..3 {
        t.open("/dev/null", O_RDWR); // stdio, as in a real process
    }
    t.p.set_exe(b"/bin/app".to_vec());
    let name = t.s("wl_shm");
    let fd = t.ok(nr::MEMFD_CREATE, &[name, fcntl::MFD_CLOEXEC]);
    t.ok(nr::FTRUNCATE, &[fd, 8192]);
    t.ok(nr::FTRUNCATE, &[fd, 16384]); // growing again works (unlike macOS shm)
    let st = t.buf(128);
    t.ok(nr::FSTAT, &[fd, st]);
    assert_eq!(u64::from_le_bytes(t.get(st + 48, 8).try_into().unwrap()), 16384);
    assert_eq!(t.readlink(&format!("/proc/self/fd/{fd}")), b"/memfd:wl_shm (deleted)");
    assert_eq!(t.readlink(&format!("/proc/{}/exe", t.p.pid)), b"/bin/app");
    assert_eq!(t.readlink("/proc/self/cwd"), b"/");
    let missing = t.s("/proc/self/fd/77");
    let b = t.buf(16);
    assert_eq!(t.call(nr::READLINKAT, &[FDCWD, missing, b, 16]), -errno::ENOENT);

    // /dev/stdout is /proc/self/fd/1.
    let fds = t.buf(8);
    t.ok(nr::PIPE2, &[fds, 0]);
    let (r, w) = (t.u32_at(fds) as u64, t.u32_at(fds + 4) as u64);
    t.ok(nr::DUP3, &[w, 1, 0]);
    let out = t.open("/dev/stdout", 1);
    t.write_str(out, "to stdout");
    assert_eq!(t.read_n(r, 32), b"to stdout");
}

#[test]
fn dev_null_and_shm() {
    let mut t = T::new();
    let null = t.open("/dev/null", O_RDWR);
    t.write_str(null, "gone");
    assert_eq!(t.read_n(null, 8), b"");
    // shm_open's /dev/shm holds shared memory objects, by name.
    let name = format!("/dev/shm/wayland-shm-{}", std::process::id());
    let fd = t.open(&name, O_CREAT | O_RDWR | 0o200 /* O_EXCL */);
    t.write_str(fd, "pool");
    let again = t.open(&name, O_RDONLY);
    assert_eq!(t.read_n(again, 16), b"pool", "the same object");
    assert_eq!(t.stat_mode(&name, 0) & 0o170000, 0o100000, "a regular file");
    let p = t.s(&name);
    t.ok(nr::UNLINKAT, &[FDCWD, p, 0]);
    assert_eq!(t.call(nr::OPENAT, &[FDCWD, p, O_RDONLY, 0]), -errno::ENOENT);
    assert_eq!(t.read_n(fd, 16), b"", "open fds keep it (at their position)");
}

#[test]
fn memfd_is_a_file_in_memory() {
    let mut t = T::new();
    let name = t.s("pool");
    let fd = t.ok(nr::MEMFD_CREATE, &[name, 1 /* MFD_CLOEXEC */]);
    let st = t.buf(128);
    t.ok(nr::FSTAT, &[fd, st]);
    assert_eq!(t.u32_at(st + 16) & 0o170000, 0o100000, "a regular file");
    assert_eq!(u64_at(&t, st + 48), 0, "empty");
    t.write_str(fd, "hello");
    // ftruncate / fallocate grow it, as Wayland clients grow pools.
    t.ok(nr::FTRUNCATE, &[fd, 4096]);
    t.ok(nr::FALLOCATE, &[fd, 0, 0, 8192]);
    t.ok(nr::FSTAT, &[fd, st]);
    assert_eq!(u64_at(&t, st + 48), 8192);
    // The position is the fd's, shared with its dup.
    let dup = t.ok(nr::DUP, &[fd]);
    assert_eq!(t.ok(nr::LSEEK, &[dup, 0, 1 /* SEEK_CUR */]), 5);
    assert_eq!(t.ok(nr::LSEEK, &[fd, 0, 0]), 0);
    assert_eq!(t.read_n(dup, 5), b"hello");
    assert_eq!(t.ok(nr::LSEEK, &[fd, 0, 2 /* SEEK_END */]), 8192);
    let b = t.buf(4);
    assert_eq!(t.ok(nr::PREAD64, &[fd, b, 4, 1]), 4);
    assert_eq!(t.get(b, 4), b"ello");
    // Made without MFD_ALLOW_SEALING: sealed against seals, as on Linux.
    const ADD_SEALS: u64 = 1033;
    const GET_SEALS: u64 = 1034;
    assert_eq!(t.ok(nr::FCNTL, &[fd, GET_SEALS]), 1 /* F_SEAL_SEAL */);
    assert_eq!(t.call(nr::FCNTL, &[fd, ADD_SEALS, 2]), -errno::EPERM);
    // It's a POSIX shm object on the host, which a compositor maps.
    use std::os::fd::AsRawFd;
    let host = t.p.fds.get(fd).unwrap().host.as_raw_fd();
    let p = unsafe { libc::mmap(std::ptr::null_mut(), 16384, libc::PROT_READ, libc::MAP_SHARED, host, 0) };
    assert_ne!(p, libc::MAP_FAILED);
    assert_eq!(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 5) }, b"hello");
}

// --- sockets ---

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;

fn unix_addr(t: &mut T, path: &[u8]) -> (u64, u64) {
    let bytes = [&1u16.to_le_bytes()[..], path, &[0]].concat();
    let len = if path.first() == Some(&0) { bytes.len() - 1 } else { bytes.len() };
    (t.put(&bytes), len as u64)
}

/// A msghdr with one iovec and optional control data.
fn msghdr(t: &mut T, data: u64, len: u64, control: u64, controllen: u64) -> u64 {
    let iov = t.put(&[data.to_le_bytes(), len.to_le_bytes()].concat());
    let hdr = abi::net::MsgHdr { iov, iovlen: 1, control, controllen, ..Default::default() };
    t.put(unsafe { abi::as_bytes(&hdr) })
}

fn rights(fds: &[i32]) -> Vec<u8> {
    let len = 16 + 4 * fds.len() as u64;
    let mut c = [len.to_le_bytes().to_vec(), 1i32.to_le_bytes().to_vec(), 1i32.to_le_bytes().to_vec()].concat();
    c.extend(fds.iter().flat_map(|f| f.to_le_bytes()));
    c.resize(len.next_multiple_of(8) as usize, 0);
    c
}

#[test]
fn socketpair_passes_fds() {
    let mut t = T::new();
    let sv = t.buf(8);
    t.ok(nr::SOCKETPAIR, &[AF_UNIX, SOCK_STREAM | O_CLOEXEC, 0, sv]);
    let (a, b) = (t.u32_at(sv) as u64, t.u32_at(sv + 4) as u64);

    let name = t.s("pool");
    let memfd = t.ok(nr::MEMFD_CREATE, &[name, 0]);
    t.write_str(memfd, "pixels");

    let data = t.put(b"hi");
    let control = t.put(&rights(&[memfd as i32]));
    let msg = msghdr(&mut t, data, 2, control, 24);
    assert_eq!(t.ok(nr::SENDMSG, &[a, msg, 0x4000 /* MSG_NOSIGNAL */]), 2);

    let rdata = t.buf(16);
    let rcontrol = t.buf(64);
    let rmsg = msghdr(&mut t, rdata, 16, rcontrol, 64);
    assert_eq!(t.ok(nr::RECVMSG, &[b, rmsg, abi::net::MSG_CMSG_CLOEXEC]), 2);
    assert_eq!(t.get(rdata, 2), b"hi");
    let hdr: abi::net::MsgHdr = t.m.read_pod(rmsg).unwrap();
    assert_eq!(hdr.controllen, 24);
    assert_eq!(hdr.flags, 0);
    let c = t.get(rcontrol, 24);
    assert_eq!(u64::from_le_bytes(c[..8].try_into().unwrap()), 20, "cmsg_len");
    assert_eq!(&c[8..16], &[1, 0, 0, 0, 1, 0, 0, 0], "SOL_SOCKET, SCM_RIGHTS");
    let got = i32::from_le_bytes(c[16..20].try_into().unwrap()) as u64;
    assert_ne!(got, memfd, "a new guest fd");
    assert_eq!(t.ok(nr::FCNTL, &[got, fcntl::GETFD]), 1, "MSG_CMSG_CLOEXEC");
    let buf = t.buf(8);
    assert_eq!(t.ok(nr::PREAD64, &[got, buf, 6, 0]), 6);
    assert_eq!(t.get(buf, 6), b"pixels", "same file as the sender's memfd");

    // Too little control space: the fd is dropped and MSG_CTRUNC set.
    t.ok(nr::SENDMSG, &[a, msg, 0]);
    let small = msghdr(&mut t, rdata, 16, rcontrol, 16);
    t.ok(nr::RECVMSG, &[b, small, 0]);
    let hdr: abi::net::MsgHdr = t.m.read_pod(small).unwrap();
    assert_eq!(hdr.flags as u64 & abi::net::MSG_CTRUNC, abi::net::MSG_CTRUNC);
    assert_eq!(hdr.controllen, 0);
}

#[test]
fn many_messages_and_dns_options() {
    const SOCK_DGRAM: u64 = 2;
    let mut t = T::new();
    let sv = t.buf(8);
    t.ok(nr::SOCKETPAIR, &[AF_UNIX, SOCK_DGRAM, 0, sv]);
    let (a, b) = (t.u32_at(sv) as u64, t.u32_at(sv + 4) as u64);
    // Two mmsghdrs (64 bytes each: a msghdr, then the length) with one
    // iovec each.
    let mmsgs = |t: &mut T, bufs: [(u64, u64); 2]| {
        let mut v = Vec::new();
        for (buf, len) in bufs {
            let iov = t.put(&[buf.to_le_bytes(), len.to_le_bytes()].concat());
            let mut m = vec![0u8; 64];
            m[16..24].copy_from_slice(&iov.to_le_bytes());
            m[24..32].copy_from_slice(&1u64.to_le_bytes());
            v.extend(m);
        }
        t.put(&v)
    };
    let (one, two) = (t.put(b"first"), t.put(b"the second"));
    let out = mmsgs(&mut t, [(one, 5), (two, 10)]);
    assert_eq!(t.ok(nr::SENDMMSG, &[a, out, 2, 0]), 2);
    assert_eq!((t.u32_at(out + 56), t.u32_at(out + 64 + 56)), (5, 10));
    let (r1, r2) = (t.buf(64), t.buf(64));
    let inp = mmsgs(&mut t, [(r1, 64), (r2, 64)]);
    // MSG_WAITFORONE: takes what's there, then stops without waiting.
    assert_eq!(t.ok(nr::RECVMMSG, &[b, inp, 2, 0x10000, 0]), 2);
    assert_eq!((t.get(r1, 5), t.get(r2, 10)), (b"first".to_vec(), b"the second".to_vec()));
    assert_eq!(t.u32_at(inp + 64 + 56), 10);
    t.ok(nr::SENDMMSG, &[a, out, 1, 0]);
    assert_eq!(t.ok(nr::RECVMMSG, &[b, inp, 2, 0x10000, 0]), 1, "one there, the rest not waited for");

    // glibc's resolver asks for IP_RECVERR, and gives up on DNS without it.
    let udp = t.ok(nr::SOCKET, &[2 /* AF_INET */, SOCK_DGRAM, 0]);
    let on = t.put(&1u32.to_le_bytes());
    assert_eq!(t.ok(nr::SETSOCKOPT, &[udp, 0 /* IPPROTO_IP */, 11 /* IP_RECVERR */, on, 4]), 0);
}

#[test]
fn seqpacket_pairs() {
    const SOCK_SEQPACKET: u64 = 5;
    let mut t = T::new();
    let sv = t.buf(8);
    t.ok(nr::SOCKETPAIR, &[AF_UNIX, SOCK_SEQPACKET, 0, sv]);
    let (a, b) = (t.u32_at(sv) as u64, t.u32_at(sv + 4) as u64);
    // It says what it is.
    let (val, len) = (t.buf(4), t.put(&4u32.to_le_bytes()));
    t.ok(nr::GETSOCKOPT, &[a, 1 /* SOL_SOCKET */, 3 /* SO_TYPE */, val, len]);
    assert_eq!(t.u32_at(val), 5);
    // Messages keep their boundaries.
    for m in ["one", "second"] {
        t.write_str(a, m);
    }
    assert_eq!(t.read_n(b, 64), b"one");
    assert_eq!(t.read_n(b, 64), b"second");
    // Big ones too (macOS's default would stop at 2 KiB).
    let big = vec![7u8; 100_000];
    let bp = t.put(&big);
    assert_eq!(t.ok(nr::WRITE, &[a, bp, big.len() as u64]), big.len() as u64);
    let rbuf = t.buf(120_000);
    assert_eq!(t.ok(nr::READ, &[b, rbuf, 120_000]), big.len() as u64);
    assert_eq!(t.get(rbuf, big.len()), big);
    // A full queue: EAGAIN without blocking (MSG_DONTWAIT), room again once
    // the reader takes one.
    let kb = t.put(&[1u8; 1000]);
    let mut sent = 0;
    while t.call(nr::SENDTO, &[a, kb, 1000, 0x40 /* MSG_DONTWAIT */, 0, 0]) == 1000 {
        sent += 1;
        assert!(sent < 100_000, "never full");
    }
    assert!(sent > 100, "room for {sent} only");
    assert_eq!(t.call(nr::SENDTO, &[a, kb, 1000, 0x40, 0, 0]), -errno::EAGAIN);
    assert_eq!(t.ok(nr::READ, &[b, rbuf, 2000]), 1000);
    assert_eq!(t.ok(nr::SENDTO, &[a, kb, 1000, 0x40, 0, 0]), 1000);
    for _ in 0..sent {
        t.ok(nr::READ, &[b, rbuf, 2000]);
    }
    // With an fd, as Firefox's broker sends one to answer on.
    let name = t.s("x");
    let memfd = t.ok(nr::MEMFD_CREATE, &[name, 0]);
    let data = t.put(b"req");
    let control = t.put(&rights(&[memfd as i32]));
    let msg = msghdr(&mut t, data, 3, control, 24);
    assert_eq!(t.ok(nr::SENDMSG, &[a, msg, 0]), 3);
    let (rdata, rcontrol) = (t.buf(16), t.buf(64));
    let rmsg = msghdr(&mut t, rdata, 16, rcontrol, 64);
    assert_eq!(t.ok(nr::RECVMSG, &[b, rmsg, 0]), 3);
    let hdr: abi::net::MsgHdr = t.m.read_pod(rmsg).unwrap();
    assert_eq!(hdr.controllen, 24, "the fd came along");
    // The peer closes: what was sent still comes, then the end.
    t.write_str(b, "last");
    t.ok(nr::CLOSE, &[b]);
    assert_eq!(t.read_n(a, 64), b"last");
    let buf = t.buf(8);
    assert_eq!(t.ok(nr::READ, &[a, buf, 8]), 0, "end of file");
    // And sending to it is EPIPE (MSG_NOSIGNAL: no SIGPIPE here).
    let d = t.put(b"x");
    assert_eq!(t.call(nr::SENDTO, &[a, d, 1, 0x4000, 0, 0]), -errno::EPIPE);
}

#[test]
fn unix_listener_with_guest_paths() {
    let mut t = T::new();
    let srv = t.ok(nr::SOCKET, &[AF_UNIX, SOCK_STREAM, 0]);
    let (addr, len) = unix_addr(&mut t, b"/wayland-0");
    t.ok(nr::BIND, &[srv, addr, len]);
    assert!(t.root().join("wayland-0").exists(), "socket file lands inside the root");
    t.ok(nr::LISTEN, &[srv, 4]);

    let cli = t.ok(nr::SOCKET, &[AF_UNIX, SOCK_STREAM | abi::net::SOCK_NONBLOCK, 0]);
    t.ok(nr::CONNECT, &[cli, addr, len]);
    let peer = t.buf(128);
    let peer_len = t.put(&128u32.to_le_bytes());
    let conn = t.ok(nr::ACCEPT4, &[srv, peer, peer_len, 0]);
    assert_eq!(t.u32_at(peer_len), 2, "client is unnamed");
    assert_eq!(t.ok(nr::FCNTL, &[conn, fcntl::GETFL]) & 0o4000, 0, "accept4 without SOCK_NONBLOCK");

    let name = t.buf(128);
    let name_len = t.put(&128u32.to_le_bytes());
    t.ok(nr::GETSOCKNAME, &[srv, name, name_len]);
    assert_eq!(t.u32_at(name_len), 2 + 10 + 1);
    assert_eq!(t.get(name, 13), [&1u16.to_le_bytes()[..], b"/wayland-0\0"].concat(), "guest path, not host");
    // A short buffer gets truncated, with the full length reported.
    let short_len = t.put(&4u32.to_le_bytes());
    t.ok(nr::GETPEERNAME, &[cli, name, short_len]);
    assert_eq!(t.u32_at(short_len), 13);

    t.write_str(cli, "hello");
    assert_eq!(t.read_n(conn, 16), b"hello");

    let opt = t.buf(4);
    let opt_len = t.put(&4u32.to_le_bytes());
    t.ok(nr::GETSOCKOPT, &[conn, 1, abi::net::SO_TYPE, opt, opt_len]);
    assert_eq!(t.u32_at(opt), 1, "SOCK_STREAM");
    let cred = t.buf(12);
    let cred_len = t.put(&12u32.to_le_bytes());
    t.ok(nr::GETSOCKOPT, &[conn, 1, abi::net::SO_PEERCRED, cred, cred_len]);
    let c: abi::net::Ucred = t.m.read_pod(cred).unwrap();
    assert_eq!((c.pid as i64, c.uid), (t.p.pid, unsafe { libc::getuid() }));
}

#[test]
fn abstract_sockets() {
    let mut t = T::new();
    let srv = t.ok(nr::SOCKET, &[AF_UNIX, SOCK_STREAM, 0]);
    let unique = format!("\0macwl-test-{}", std::process::id());
    let (addr, len) = unix_addr(&mut t, unique.as_bytes());
    t.ok(nr::BIND, &[srv, addr, len]);
    t.ok(nr::LISTEN, &[srv, 1]);
    let cli = t.ok(nr::SOCKET, &[AF_UNIX, SOCK_STREAM, 0]);
    t.ok(nr::CONNECT, &[cli, addr, len]);
    let name = t.buf(128);
    let name_len = t.put(&128u32.to_le_bytes());
    t.ok(nr::GETSOCKNAME, &[srv, name, name_len]);
    assert_eq!(t.u32_at(name_len) as u64, len, "no trailing NUL for abstract names");
    assert_eq!(t.get(name, len as usize), t.get(addr, len as usize));
    // Clean up the host file standing in for the abstract name.
    let dir = t.p.fs.private_dir("abstract").unwrap();
    let hex: String = unique.as_bytes()[1..].iter().map(|b| format!("{b:02x}")).collect();
    std::fs::remove_file(dir.join(hex)).unwrap();
}

/// The Wayland path end to end, minus the protocol: a host "compositor"
/// listens on a host socket, the guest connects by guest path and sends a
/// memfd; the host maps the fd it receives and sees the guest's pixels.
#[test]
fn host_compositor_receives_guest_buffer() {
    let mut t = T::new();
    let listener = std::os::unix::net::UnixListener::bind(t.root().join("wayland-1")).unwrap();

    let sock = t.ok(nr::SOCKET, &[AF_UNIX, SOCK_STREAM | O_CLOEXEC, 0]);
    let (addr, len) = unix_addr(&mut t, b"/wayland-1");
    t.ok(nr::CONNECT, &[sock, addr, len]);
    let (server, _) = listener.accept().unwrap();

    let name = t.s("wl_shm");
    let memfd = t.ok(nr::MEMFD_CREATE, &[name, fcntl::MFD_CLOEXEC]);
    t.ok(nr::FTRUNCATE, &[memfd, 4096]);
    t.write_str(memfd, "ARGB pixels");
    let data = t.put(b"create_pool");
    let control = t.put(&rights(&[memfd as i32]));
    let msg = msghdr(&mut t, data, 11, control, 24);
    t.ok(nr::SENDMSG, &[sock, msg, 0]);

    // The host side, with plain Darwin calls.
    let mut buf = [0u8; 32];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr().cast();
    hdr.msg_controllen = 64;
    let n = unsafe { libc::recvmsg(server.as_raw_fd(), &mut hdr, 0) };
    assert_eq!(&buf[..n as usize], b"create_pool");
    let c = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
    assert!(!c.is_null());
    let host_fd = unsafe { *libc::CMSG_DATA(c).cast::<i32>() };
    let map = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, host_fd, 0) };
    assert_ne!(map, libc::MAP_FAILED);
    assert_eq!(unsafe { std::slice::from_raw_parts(map.cast::<u8>(), 11) }, b"ARGB pixels");
    unsafe {
        libc::munmap(map, 4096);
        libc::close(host_fd);
    }
    let _ = std::fs::metadata(t.root().join("wayland-1")).map(|m| m.file_type()).unwrap();
    assert!(t.root().join("wayland-1").as_os_str().as_bytes().len() < 104);
}

#[test]
fn writing_to_a_closed_peer_is_epipe() {
    let mut t = T::new();
    let sv = t.buf(8);
    t.ok(nr::SOCKETPAIR, &[AF_UNIX, SOCK_STREAM, 0, sv]);
    let (a, b) = (t.u32_at(sv) as u64, t.u32_at(sv + 4) as u64);
    t.ok(nr::CLOSE, &[b]);
    let data = t.put(b"x");
    assert_eq!(t.call(nr::WRITE, &[a, data, 1]), -errno::EPIPE);
}

#[test]
fn signals_and_single_threaded_futex() {
    let mut t = T::new();
    // rt_sigaction records and reports dispositions.
    let ign = t.put(&[1u64.to_le_bytes(), 0u64.to_le_bytes(), 0u64.to_le_bytes(), 0u64.to_le_bytes()].concat());
    let old = t.buf(32);
    t.ok(nr::RT_SIGACTION, &[13, ign, 0, 8]); // SIGPIPE -> SIG_IGN, as Rust's std does
    t.ok(nr::RT_SIGACTION, &[13, 0, old, 8]);
    assert_eq!(t.get(old, 8), 1u64.to_le_bytes());
    assert_eq!(t.call(nr::RT_SIGACTION, &[9, ign, 0, 8]), -errno::EINVAL, "SIGKILL can't be caught");
    // Raising an ignored signal does nothing; SIGABRT's default kills.
    assert_eq!(t.call(nr::TKILL, &[t.p.pid as u64, 13]), 0);
    assert_eq!(t.p.next_delivery(&mut t.th), None);
    assert_eq!(t.call(nr::TKILL, &[t.p.pid as u64, 6]), 0);
    assert_eq!(t.p.next_delivery(&mut t.th), Some(Delivery::Kill(6)));
    // Blocking masks out SIGKILL.
    let set = t.put(&u64::MAX.to_le_bytes());
    let got = t.buf(8);
    t.ok(nr::RT_SIGPROCMASK, &[2, set, 0, 8]);
    t.ok(nr::RT_SIGPROCMASK, &[0, 0, got, 8]);
    assert_eq!(u64::from_le_bytes(t.get(got, 8).try_into().unwrap()) & (1 << 8), 0);
    let none = t.put(&0u64.to_le_bytes());
    t.ok(nr::RT_SIGPROCMASK, &[2, none, 0, 8]);

    let word = t.put(&7u32.to_le_bytes());
    assert_eq!(t.ok(nr::FUTEX, &[word, 0x81, i32::MAX as u64]), 0, "wake: no waiters");
    assert_eq!(t.call(nr::FUTEX, &[word, 0x80, 8, 0]), -errno::EAGAIN, "value changed");
    let ms = t.put(&[0i64.to_le_bytes(), 1_000_000i64.to_le_bytes()].concat());
    assert_eq!(t.call(nr::FUTEX, &[word, 0x80, 7, ms]), -errno::ETIMEDOUT);
}

#[test]
fn handlers_masks_and_pending() {
    use super::signal::HandlerCall;
    let mut t = T::new();
    const SA_RESTART: u64 = 0x1000_0000;
    const SA_RESETHAND: u64 = 0x8000_0000;
    const SA_RESTORER: u64 = 0x0400_0000;
    // SIGUSR1 (10): handler 0x1234, also blocking SIGUSR2 (12) while it runs.
    let act = t.put(&[0x1234u64.to_le_bytes(), (SA_RESTART | SA_RESTORER).to_le_bytes(), 0x5678u64.to_le_bytes(), (1u64 << 11).to_le_bytes()].concat());
    t.ok(nr::RT_SIGACTION, &[10, act, 0, 8]);
    // Blocked: pending until unblocked.
    let usr1 = t.put(&(1u64 << 9).to_le_bytes());
    t.ok(nr::RT_SIGPROCMASK, &[0, usr1, 0, 8]);
    t.ok(nr::KILL, &[t.p.pid as u64, 10]);
    assert_eq!(t.p.next_delivery(&mut t.th), None);
    let pending = t.buf(8);
    t.ok(nr::RT_SIGPENDING, &[pending, 8]);
    assert_eq!(u64_at(&t, pending), 1 << 9);
    t.ok(nr::RT_SIGPROCMASK, &[1, usr1, 0, 8]);
    let Some(Delivery::Handler(HandlerCall { info, handler, restorer, saved_mask, restart, .. })) = t.p.next_delivery(&mut t.th) else {
        panic!("expected the handler");
    };
    assert_eq!((info.signo, info.code, info.pid as i64), (10, 0, t.p.pid));
    assert_eq!((handler, restorer, saved_mask, restart), (0x1234, Some(0x5678), 0, true));
    // While it runs: SIGUSR1 itself and SIGUSR2 are blocked.
    let cur = t.buf(8);
    t.ok(nr::RT_SIGPROCMASK, &[0, 0, cur, 8]);
    assert_eq!(u64_at(&t, cur), (1 << 9) | (1 << 11));
    t.th.sigreturn(saved_mask);
    t.ok(nr::RT_SIGPROCMASK, &[0, 0, cur, 8]);
    assert_eq!(u64_at(&t, cur), 0);

    // SA_RESETHAND: one shot, then the default (which kills).
    let once = t.put(&[0x1234u64.to_le_bytes(), SA_RESETHAND.to_le_bytes(), [0; 8], [0; 8]].concat());
    t.ok(nr::RT_SIGACTION, &[15, once, 0, 8]);
    t.ok(nr::TKILL, &[t.p.pid as u64, 15]);
    assert!(matches!(t.p.next_delivery(&mut t.th), Some(Delivery::Handler(_))));
    t.th.sigreturn(0);
    t.ok(nr::TKILL, &[t.p.pid as u64, 15]);
    assert_eq!(t.p.next_delivery(&mut t.th), Some(Delivery::Kill(15)));

    // A fault with no handler kills; with one blocked, too (Linux forces it).
    assert_eq!(t.p.fault(&mut t.th, SigInfo::fault(11, 1, 0x10)), Delivery::Kill(11));
    let segv = t.put(&[0x9999u64.to_le_bytes(), [0; 8], [0; 8], [0; 8]].concat());
    t.ok(nr::RT_SIGACTION, &[11, segv, 0, 8]);
    assert!(matches!(t.p.fault(&mut t.th, SigInfo::fault(11, 1, 0x10)), Delivery::Handler(c) if c.info.addr == 0x10));
    assert_eq!(t.p.fault(&mut t.th, SigInfo::fault(11, 1, 0x10)), Delivery::Kill(11), "SIGSEGV is blocked inside its handler");

    // EPIPE raises SIGPIPE, whose default kills.
    t.th.sigreturn(0);
    let mut fds = [0i32; 2];
    unsafe { libc::pipe(fds.as_mut_ptr()) };
    unsafe { libc::close(fds[0]) };
    let w = t.p.fds.insert(FdEntry::new(unsafe { std::os::fd::FromRawFd::from_raw_fd(fds[1]) })) as u64;
    let b = t.put(b"x");
    assert_eq!(t.call(nr::WRITE, &[w, b, 1]), -errno::EPIPE);
    assert_eq!(t.p.next_delivery(&mut t.th), Some(Delivery::Kill(13)));
}

/// Guest memory several host threads can share.
struct SyncMem(std::sync::Mutex<Vec<u8>>);

impl GuestMemory for SyncMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        let m = self.0.lock().unwrap();
        let off = (addr - BASE) as usize;
        buf.copy_from_slice(m.get(off..off + buf.len()).ok_or(Fault { addr })?);
        Ok(())
    }
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        let mut m = self.0.lock().unwrap();
        let off = (addr - BASE) as usize;
        m.get_mut(off..off + data.len()).ok_or(Fault { addr })?.copy_from_slice(data);
        Ok(())
    }
    fn brk(&self, _: u64) -> u64 {
        0
    }
    fn mmap_anonymous(&self, _: u64, _: u64, _: u64, _: u64) -> i64 {
        -errno::ENOSYS
    }
    fn mmap_shared(&self, _: u64, _: u64, _: u64, _: u64, _: Option<i32>, _: u64) -> i64 {
        -errno::ENOSYS
    }
    fn munmap(&self, _: u64, _: u64) -> i64 {
        0
    }
    fn mprotect(&self, _: u64, _: u64, _: u64) -> i64 {
        0
    }
}

#[test]
fn futex_waits_and_wakes_across_threads() {
    use std::time::{Duration, Instant};
    let p = Process::new("/");
    let mem = SyncMem(std::sync::Mutex::new(vec![0; 4096]));
    let word = BASE;
    let call = |t: &mut Thread, nr: u64, a: &[u64]| -> i64 {
        let mut args = [0; 6];
        args[..a.len()].copy_from_slice(a);
        match dispatch(&p, t, &mem, nr, args) {
            Action::Return(v) => v,
            other => panic!("{other:?}"),
        }
    };
    const WAIT_PRIVATE: u64 = 128;
    const WAKE_PRIVATE: u64 = 129;
    std::thread::scope(|s| {
        let waiter = s.spawn(|| {
            let mut t = Thread::main(&p).spawn(2);
            let start = Instant::now();
            let r = call(&mut t, nr::FUTEX, &[word, WAIT_PRIVATE, 0, 0]);
            (r, start.elapsed())
        });
        // Wake it once it's asleep (a wake with nobody waiting does nothing).
        let mut t = Thread::main(&p);
        std::thread::sleep(Duration::from_millis(50));
        mem.write(word, &1u32.to_le_bytes()).unwrap();
        assert_eq!(call(&mut t, nr::FUTEX, &[word, WAKE_PRIVATE, 1]), 1);
        let (r, waited) = waiter.join().unwrap();
        assert_eq!(r, 0);
        assert!(waited >= Duration::from_millis(40));
    });
    // The value already changed: EAGAIN at once.
    let mut t = Thread::main(&p);
    assert_eq!(call(&mut t, nr::FUTEX, &[word, WAIT_PRIVATE, 0, 0]), -errno::EAGAIN);

    // CMP_REQUEUE: wake one of two waiters, move the other to a second word.
    let (w1, w2) = (BASE, BASE + 8);
    mem.write(w1, &0u32.to_le_bytes()).unwrap();
    std::thread::scope(|s| {
        let waiters: Vec<_> = (0..2)
            .map(|i| {
                let (p, mem) = (&p, &mem);
                s.spawn(move || {
                    let mut t = Thread::main(p).spawn(10 + i);
                    let mut args = [0; 6];
                    args[..4].copy_from_slice(&[w1, WAIT_PRIVATE, 0, 0]);
                    dispatch(p, &mut t, mem, nr::FUTEX, args)
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        const CMP_REQUEUE_PRIVATE: u64 = 4 | 128;
        assert_eq!(call(&mut t, nr::FUTEX, &[w1, CMP_REQUEUE_PRIVATE, 1, 1, w2, 0]), 2);
        assert_eq!(call(&mut t, nr::FUTEX, &[w2, WAKE_PRIVATE, 1]), 1, "the requeued one waits on w2");
        for w in waiters {
            assert_eq!(w.join().unwrap(), Action::Return(0));
        }
    });
}

/// PI locks (LOCK_PI handing over at UNLOCK_PI, TRYLOCK_PI, EDEADLK,
/// EPERM), a robust futex whose owner thread ends with a waiter on it,
/// WAKE_OP's arithmetic, and futex_waitv woken by its second futex.
#[test]
fn pi_robust_wake_op_and_waitv() {
    use std::time::Duration;
    let p = Process::new("/");
    let mem = SyncMem(std::sync::Mutex::new(vec![0; 4096]));
    let call = |t: &mut Thread, nr: u64, a: &[u64]| -> i64 {
        let mut args = [0; 6];
        args[..a.len()].copy_from_slice(a);
        match dispatch(&p, t, &mem, nr, args) {
            Action::Return(v) => v,
            other => panic!("{other:?}"),
        }
    };
    let u32_at = |a: u64| {
        let mut b = [0; 4];
        mem.read(a, &mut b).unwrap();
        u32::from_le_bytes(b)
    };
    const WAITERS: u32 = 0x8000_0000;
    const OWNER_DIED: u32 = 0x4000_0000;
    const LOCK_PI: u64 = 6;
    const UNLOCK_PI: u64 = 7;
    const TRYLOCK_PI: u64 = 8;
    let word = BASE;
    let mut a = Thread::main(&p).spawn(2);
    assert_eq!(call(&mut a, nr::FUTEX, &[word, LOCK_PI, 0, 0]), 0, "free: taken");
    assert_eq!(u32_at(word), 2);
    assert_eq!(call(&mut a, nr::FUTEX, &[word, LOCK_PI, 0, 0]), -errno::EDEADLK);
    std::thread::scope(|s| {
        let b = s.spawn(|| {
            let mut b = Thread::main(&p).spawn(3);
            assert_eq!(call(&mut b, nr::FUTEX, &[word, TRYLOCK_PI, 0, 0]), -errno::EAGAIN);
            assert_eq!(call(&mut b, nr::FUTEX, &[word, UNLOCK_PI, 0, 0]), -errno::EPERM, "not its own");
            call(&mut b, nr::FUTEX, &[word, LOCK_PI, 0, 0])
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(u32_at(word), 2 | WAITERS, "a waiter marked");
        assert_eq!(call(&mut a, nr::FUTEX, &[word, UNLOCK_PI, 0, 0]), 0);
        assert_eq!(b.join().unwrap(), 0);
        assert_eq!(u32_at(word), 3 | WAITERS, "handed to the waiter");
    });
    let mut b = Thread::main(&p).spawn(3);
    assert_eq!(call(&mut b, nr::FUTEX, &[word, UNLOCK_PI, 0, 0]), 0);
    assert_eq!(u32_at(word), 0, "no one waits");

    // A robust list: its head at H, one entry at E, the futex word 16 past
    // each entry. Thread 4 holds it; a waiter waits; 4 ends.
    let (h, e) = (BASE + 64, BASE + 128);
    let robust = e + 16;
    mem.write(h, &[e.to_le_bytes(), 16u64.to_le_bytes(), 0u64.to_le_bytes()].concat()).unwrap();
    mem.write(e, &h.to_le_bytes()).unwrap();
    mem.write(robust, &(4 | WAITERS).to_le_bytes()).unwrap();
    let mut owner = Thread::main(&p).spawn(4);
    assert_eq!(call(&mut owner, nr::SET_ROBUST_LIST, &[h, 23]), -errno::EINVAL);
    assert_eq!(call(&mut owner, nr::SET_ROBUST_LIST, &[h, 24]), 0);
    let got = BASE + 256;
    assert_eq!(call(&mut owner, nr::GET_ROBUST_LIST, &[0, got, got + 8]), 0);
    assert_eq!(u32_at(got) as u64, h);
    std::thread::scope(|s| {
        let waiter = s.spawn(|| {
            let mut w = Thread::main(&p).spawn(5);
            call(&mut w, nr::FUTEX, &[robust, 0, (4 | WAITERS) as u64, 0])
        });
        std::thread::sleep(Duration::from_millis(50));
        p.release_robust(4, &mem);
        assert_eq!(waiter.join().unwrap(), 0, "woken by the owner's end");
    });
    assert_eq!(u32_at(robust), WAITERS | OWNER_DIED);

    // WAKE_OP: word2 += 5, then (old 0 == 0) a wake on both; no one waits.
    let word2 = BASE + 512;
    let op = (1u64 << 28) | (5 << 12); // FUTEX_OP_ADD 5, FUTEX_OP_CMP_EQ 0
    assert_eq!(call(&mut a, nr::FUTEX, &[word, 5, 1, 1, word2, op]), 0);
    assert_eq!(u32_at(word2), 5);
    let shift = (8u64 << 28) | (2 << 28) | (4 << 12); // OR (1 << 4)
    assert_eq!(call(&mut a, nr::FUTEX, &[word, 5, 1, 1, word2, shift]), 0);
    assert_eq!(u32_at(word2), 5 | 16);

    // futex_waitv on three words, woken by the second.
    let v = BASE + 1024;
    let words = [BASE + 2048, BASE + 2052, BASE + 2056];
    let mut entries = Vec::new();
    for w in words {
        entries.extend([0u64.to_le_bytes(), w.to_le_bytes(), (2u64 | 128).to_le_bytes()].concat());
    }
    mem.write(v, &entries).unwrap();
    assert_eq!(call(&mut a, nr::FUTEX_WAITV, &[v, 0, 0, 0, 0]), -errno::EINVAL, "none");
    mem.write(words[2], &1u32.to_le_bytes()).unwrap();
    assert_eq!(call(&mut a, nr::FUTEX_WAITV, &[v, 3, 0, 0, 0]), -errno::EAGAIN, "one differs");
    mem.write(words[2], &0u32.to_le_bytes()).unwrap();
    std::thread::scope(|s| {
        let waiter = s.spawn(|| {
            let mut w = Thread::main(&p).spawn(6);
            call(&mut w, nr::FUTEX_WAITV, &[v, 3, 0, 0, 0])
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(call(&mut a, nr::FUTEX_WAKE, &[words[1], u32::MAX as u64, 1, 2 | 128]), 1, "futex2 wake");
        assert_eq!(waiter.join().unwrap(), 1, "the second woke it");
    });
    // Its other entries left their queues.
    assert_eq!(call(&mut a, nr::FUTEX, &[words[0], 129, 1]), 0);
}

#[test]
fn epoll_on_pipes() {
    let mut t = T::new();
    const IN: u32 = 0x1;
    const OUT: u32 = 0x4;
    const HUP: u32 = 0x10;
    const ONESHOT: u32 = 1 << 30;
    let ep = t.ok(nr::EPOLL_CREATE1, &[0o2000000]);
    let fds = t.buf(8);
    t.ok(nr::PIPE2, &[fds, 0]);
    let (r, w) = (t.u32_at(fds) as u64, t.u32_at(fds + 4) as u64);
    let ev = |t: &mut T, events: u32, data: u64| t.put(&[&events.to_le_bytes()[..], &[0; 4], &data.to_le_bytes()].concat());
    let e = ev(&mut t, IN, 0xabcd);
    t.ok(nr::EPOLL_CTL, &[ep, 1, r, e]);
    assert_eq!(t.call(nr::EPOLL_CTL, &[ep, 1, r, e]), -errno::EEXIST);
    let out = t.buf(16 * 4);
    let wait = |t: &mut T| t.ok(nr::EPOLL_PWAIT, &[ep, out, 4, 0, 0]);
    assert_eq!(wait(&mut t), 0, "nothing to read yet");
    t.write_str(w, "x");
    assert_eq!(wait(&mut t), 1);
    assert_eq!((t.u32_at(out), u64_at(&t, out + 8)), (IN, 0xabcd));
    assert_eq!(wait(&mut t), 1, "level-triggered: still readable");
    t.read_n(r, 1);
    assert_eq!(wait(&mut t), 0);

    // The write end: writable; one-shot, then quiet until re-armed.
    let e = ev(&mut t, OUT | ONESHOT, 7);
    t.ok(nr::EPOLL_CTL, &[ep, 1, w, e]);
    assert_eq!(wait(&mut t), 1);
    assert_eq!((t.u32_at(out), u64_at(&t, out + 8)), (OUT, 7));
    assert_eq!(wait(&mut t), 0, "one-shot");
    t.ok(nr::EPOLL_CTL, &[ep, 3, w, e]);
    assert_eq!(wait(&mut t), 1, "re-armed by MOD");
    t.ok(nr::EPOLL_CTL, &[ep, 2, w, 0]);
    assert_eq!(t.call(nr::EPOLL_CTL, &[ep, 2, w, 0]), -errno::ENOENT);

    // The writer goes: hangup.
    t.ok(nr::CLOSE, &[w]);
    assert_eq!(wait(&mut t), 1);
    assert_eq!(t.u32_at(out) & HUP, HUP);

    // Regular files can't be watched (they're always ready), as on Linux.
    std::fs::write(t.root().join("f"), b"").unwrap();
    let f = t.open("/f", O_RDONLY);
    let e = ev(&mut t, IN, 0);
    assert_eq!(t.call(nr::EPOLL_CTL, &[ep, 1, f, e]), -errno::EPERM);
}

#[test]
fn eventfd_and_timerfd() {
    use std::time::{Duration, Instant};
    let mut t = T::new();
    const NONBLOCK: u64 = 0o4000;
    // eventfd: writes add up; a read takes the total.
    let efd = t.ok(nr::EVENTFD2, &[0, NONBLOCK]);
    let b = t.buf(8);
    assert_eq!(t.call(nr::READ, &[efd, b, 8]), -errno::EAGAIN);
    for v in [2u64, 3] {
        let p = t.put(&v.to_le_bytes());
        assert_eq!(t.ok(nr::WRITE, &[efd, p, 8]), 8);
    }
    let ep = t.ok(nr::EPOLL_CREATE1, &[0]);
    let ev = t.put(&[&1u32.to_le_bytes()[..], &[0; 4], &42u64.to_le_bytes()].concat());
    t.ok(nr::EPOLL_CTL, &[ep, 1, efd, ev]);
    let out = t.buf(32);
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 2, 0, 0]), 1, "readable");
    assert_eq!(t.ok(nr::READ, &[efd, b, 8]), 8);
    assert_eq!(u64_at(&t, b), 5);
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 2, 0, 0]), 0, "drained");

    // timerfd: a 30 ms one-shot, then a 10 ms interval.
    let tfd = t.ok(nr::TIMERFD_CREATE, &[1 /* MONOTONIC */, 0]);
    let ev = t.put(&[&1u32.to_le_bytes()[..], &[0; 4], &7u64.to_le_bytes()].concat());
    t.ok(nr::EPOLL_CTL, &[ep, 1, tfd, ev]);
    let spec = t.put(&[0i64, 0, 0, 30_000_000].map(|v| v.to_le_bytes()).concat());
    t.ok(nr::TIMERFD_SETTIME, &[tfd, 0, spec, 0]);
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 2, 0, 0]), 0, "not yet");
    let start = Instant::now();
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 2, 1000, 0]), 1);
    assert_eq!(u64_at(&t, out + 8), 7);
    assert!(start.elapsed() >= Duration::from_millis(20));
    assert_eq!(t.ok(nr::READ, &[tfd, b, 8]), 8);
    assert_eq!(u64_at(&t, b), 1);
    let spec = t.put(&[0i64, 10_000_000, 0, 10_000_000].map(|v| v.to_le_bytes()).concat());
    t.ok(nr::TIMERFD_SETTIME, &[tfd, 0, spec, 0]);
    std::thread::sleep(Duration::from_millis(55));
    assert_eq!(t.ok(nr::READ, &[tfd, b, 8]), 8);
    assert!((4..=6).contains(&u64_at(&t, b)), "expirations: {}", u64_at(&t, b));
    let cur = t.buf(32);
    t.ok(nr::TIMERFD_GETTIME, &[tfd, cur]);
    assert_eq!(u64_at(&t, cur + 8), 10_000_000, "interval");
}

#[test]
fn pseudo_terminals() {
    let mut t = T::new();
    const O_NOCTTY: u64 = 0o400;
    // posix_openpt, then unlockpt and ptsname as glibc does them.
    let master = t.open("/dev/ptmx", O_RDWR | O_NOCTTY);
    let n = t.buf(4);
    t.ok(nr::IOCTL, &[master, 0x8004_5430 /* TIOCGPTN */, n]);
    let unlock = t.put(&0i32.to_le_bytes());
    t.ok(nr::IOCTL, &[master, 0x4004_5431 /* TIOCSPTLCK */, unlock]);
    let name = format!("/dev/pts/{}", t.u32_at(n));
    assert_eq!(t.stat_mode(&name, 0) & 0o170000, 0o020000, "{name} is a character device");
    let slave = t.open(&name, O_RDWR | O_NOCTTY);
    // Raw mode on the slave, as a shell's line editor would.
    let tio = t.buf(36);
    t.ok(nr::IOCTL, &[slave, 0x5401, tio]);
    let lflag = t.u32_at(tio + 12) & !(0o2 | 0o10); // ICANON, ECHO off
    t.m.write(tio + 12, &lflag.to_le_bytes()).unwrap();
    t.ok(nr::IOCTL, &[slave, 0x5402, tio]);
    t.write_str(master, "hi");
    assert_eq!(t.read_n(slave, 8), b"hi");
    t.write_str(slave, "ok");
    assert_eq!(t.read_n(master, 8), b"ok");
    // TIOCGPTPEER: the slave straight from the master.
    let peer = t.ok(nr::IOCTL, &[master, 0x5441, O_RDWR | O_NOCTTY]);
    t.write_str(peer, "!");
    assert_eq!(t.read_n(master, 8), b"!");
}

#[test]
fn pselect_on_a_pipe() {
    let mut t = T::new();
    let fds = t.buf(8);
    t.ok(nr::PIPE2, &[fds, 0]);
    let (r, w) = (t.u32_at(fds) as u64, t.u32_at(fds + 4) as u64);
    let set = |t: &mut T, fd: u64| t.put(&(1u64 << fd).to_le_bytes());
    let zero = t.put(&[0i64, 0].map(|v| v.to_le_bytes()).concat());
    let rs = set(&mut t, r);
    assert_eq!(t.ok(nr::PSELECT6, &[r + 1, rs, 0, 0, zero, 0]), 0, "nothing to read");
    assert_eq!(u64_at(&t, rs), 0, "cleared");
    t.write_str(w, "x");
    let rs = set(&mut t, r);
    let ws = set(&mut t, w);
    assert_eq!(t.ok(nr::PSELECT6, &[w.max(r) + 1, rs, ws, 0, zero, 0]), 2);
    assert_eq!((u64_at(&t, rs), u64_at(&t, ws)), (1 << r, 1 << w));
}

fn u64_at(t: &T, addr: u64) -> u64 {
    u64::from_le_bytes(t.get(addr, 8).try_into().unwrap())
}

#[test]
fn statx_and_statfs() {
    let mut t = T::new();
    std::fs::write(t.root().join("f"), b"twelve bytes").unwrap();
    let path = t.s("/f");
    let b = t.buf(256);
    t.ok(nr::STATX, &[FDCWD, path, 0, 0x7ff, b]);
    assert_eq!(t.u32_at(b) & 0xfff, 0xfff, "basic stats and birth time");
    assert_eq!(t.u32_at(b + 28) as u16 as u32 & 0o170000, 0o100000, "regular file");
    assert_eq!(u64_at(&t, b + 40), 12, "size");
    assert!(u64_at(&t, b + 80) > 0, "birth time");
    // AT_EMPTY_PATH on an fd.
    let fd = t.open("/f", O_RDONLY);
    let empty = t.s("");
    t.ok(nr::STATX, &[fd, empty, at::EMPTY_PATH, 0x7ff, b]);
    assert_eq!(u64_at(&t, b + 40), 12);
    assert_eq!(t.call(nr::STATX, &[FDCWD, path, 0x8000_0000, 0x7ff, b]), -errno::EINVAL);

    let fs = t.buf(120);
    t.ok(nr::STATFS, &[path, fs]);
    assert_eq!(u64_at(&t, fs), 0xef53, "host files look like ext4");
    assert!(u64_at(&t, fs + 8) > 0, "block size");
    t.ok(nr::FSTATFS, &[fd, fs]);
    let proc_self = t.s("/proc/self");
    t.ok(nr::STATFS, &[proc_self, fs]);
    assert_eq!(u64_at(&t, fs), 0x9fa0, "procfs");
}

#[test]
fn identity_and_prctl() {
    let mut t = T::new();
    let (uid, gid) = unsafe { (libc::getuid() as u64, libc::getgid() as u64) };
    // Dropping privileges we don't have is fine; gaining others is not.
    t.ok(nr::SETGID, &[gid]);
    t.ok(nr::SETUID, &[uid]);
    t.ok(nr::SETRESUID, &[u64::from(u32::MAX), uid, u64::from(u32::MAX)]);
    assert_eq!(t.call(nr::SETUID, &[0]), -errno::EPERM);
    let ids = [t.buf(4), t.buf(4), t.buf(4)];
    t.ok(nr::GETRESUID, &ids);
    assert_eq!(t.u32_at(ids[0]) as u64, uid);
    let n = t.ok(nr::GETGROUPS, &[0, 0]);
    let list = t.buf(n * 4);
    assert_eq!(t.ok(nr::GETGROUPS, &[n, list]), n);
    if n > 1 {
        assert_eq!(t.call(nr::GETGROUPS, &[1, list]), -errno::EINVAL);
    }

    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    t.p.set_exe(b"/usr/bin/a-rather-long-program-name".to_vec());
    let name = t.buf(16);
    t.ok(nr::PRCTL, &[PR_GET_NAME, name]);
    assert_eq!(t.get(name, 16), b"a-rather-long-p\0", "the exe's name, truncated");
    let new = t.s("worker");
    t.ok(nr::PRCTL, &[PR_SET_NAME, new]);
    t.ok(nr::PRCTL, &[PR_GET_NAME, name]);
    assert_eq!(&t.get(name, 7), b"worker\0");
    assert_eq!(t.call(nr::PRCTL, &[0x5356_4d41, 0, 0]), -errno::EINVAL, "PR_SET_VMA: unsupported, quietly");
}

#[test]
fn sendfile_copies_and_advances() {
    let mut t = T::new();
    std::fs::write(t.root().join("src"), b"0123456789").unwrap();
    let src = t.open("/src", O_RDONLY);
    let dst = t.open("/dst", O_RDWR | O_CREAT);
    // From the file position, which moves.
    assert_eq!(t.ok(nr::SENDFILE, &[dst, src, 0, 4]), 4);
    assert_eq!(t.ok(nr::LSEEK, &[src, 0, 1]), 4);
    // From an explicit offset, which is updated instead.
    let off = t.put(&6i64.to_le_bytes());
    assert_eq!(t.ok(nr::SENDFILE, &[dst, src, off, 100]), 4);
    assert_eq!(u64_at(&t, off), 10);
    assert_eq!(t.ok(nr::LSEEK, &[src, 0, 1]), 4);
    assert_eq!(std::fs::read(t.root().join("dst")).unwrap(), b"01236789");
}

#[test]
fn terminal_ioctls_on_a_non_terminal() {
    let mut t = T::new();
    let fd = t.open("/", O_RDONLY | O_DIRECTORY);
    let b = t.buf(64);
    // How isatty() answers no.
    assert_eq!(t.call(nr::IOCTL, &[fd, 0x5401, b]), -errno::ENOTTY);
}

#[test]
fn termios_round_trips_through_a_host_pty() {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut t = T::new();
    let (mut master, mut slave) = (0, 0);
    let r = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(r, 0);
    let _master = unsafe { OwnedFd::from_raw_fd(master) };
    let fd = t.p.fds.insert(FdEntry::new(unsafe { OwnedFd::from_raw_fd(slave) })) as u64;
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const ICANON: u32 = 0o2;
    const ECHO: u32 = 0o10;
    let b = t.buf(36);
    t.ok(nr::IOCTL, &[fd, TCGETS, b]);
    let lflag = t.u32_at(b + 12);
    assert_ne!(lflag & ICANON, 0, "a fresh pty is canonical");
    // Raw mode, as a line editor sets it.
    t.m.write(b + 12, &(lflag & !(ICANON | ECHO)).to_le_bytes()).unwrap();
    t.m.write(b + 17 + 6, &[1]).unwrap(); // c_cc[VMIN]
    t.ok(nr::IOCTL, &[fd, TCSETS, b]);
    let mut host: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(slave, &mut host) }, 0);
    assert_eq!(host.c_lflag & (libc::ICANON | libc::ECHO), 0, "the host terminal changed");
    assert_eq!(host.c_cc[libc::VMIN], 1);
    let b2 = t.buf(36);
    t.ok(nr::IOCTL, &[fd, TCGETS, b2]);
    assert_eq!(t.get(b2, 36), t.get(b, 36), "reads back as written");
}

fn ptr_array(t: &mut T, strs: &[&str]) -> u64 {
    let ptrs: Vec<u64> = strs.iter().map(|s| t.s(s)).collect();
    let bytes: Vec<u8> = ptrs.iter().chain([&0]).flat_map(|p| p.to_le_bytes()).collect();
    t.put(&bytes)
}

#[test]
fn execve_reads_programs_and_follows_shebangs() {
    use std::os::unix::fs::PermissionsExt;
    let mut t = T::new();
    let exec = |path: &Path, data: &[u8]| {
        std::fs::write(path, data).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    std::fs::create_dir(t.root().join("bin")).unwrap();
    exec(&t.root().join("bin/sh"), b"\x7fELF fake");
    exec(&t.root().join("script"), b"#!/bin/sh -e  \necho hi\n");
    std::fs::write(t.root().join("plain"), b"\x7fELF").unwrap(); // not executable

    let (path, argv, envp) = (t.s("/script"), ptr_array(&mut t, &["script", "arg"]), ptr_array(&mut t, &["A=1"]));
    let mut a = [0; 6];
    a[..3].copy_from_slice(&[path, argv, envp]);
    let Action::Exec(e) = dispatch(&t.p, &mut t.th, &t.m, nr::EXECVE, a) else { panic!("not an exec") };
    assert_eq!(e.argv, [&b"/bin/sh"[..], b"-e", b"/script", b"arg"], "interpreter, its argument, the script, the rest");
    assert_eq!(e.envp, [b"A=1"]);
    assert_eq!(e.elf, b"\x7fELF fake");
    assert!(e.exe.ends_with(b"/bin/sh"));

    let plain = t.s("/plain");
    assert_eq!(t.call(nr::EXECVE, &[plain, argv, envp]), -errno::EACCES);
    let dir = t.s("/bin");
    assert_eq!(t.call(nr::EXECVE, &[dir, argv, envp]), -errno::EACCES);
    let missing = t.s("/nope");
    assert_eq!(t.call(nr::EXECVE, &[missing, argv, envp]), -errno::ENOENT);
    exec(&t.root().join("text"), b"just text");
    let text = t.s("/text");
    assert_eq!(t.call(nr::EXECVE, &[text, argv, envp]), -errno::ENOEXEC);
    exec(&t.root().join("loop"), b"#!/loop\n");
    let lp = t.s("/loop");
    assert_eq!(t.call(nr::EXECVE, &[lp, argv, envp]), -errno::ELOOP);
}

#[test]
fn execve_env_shebangs_search_the_guest_path() {
    use std::os::unix::fs::PermissionsExt;
    let mut t = T::new();
    let exec = |path: &Path, data: &[u8]| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    exec(&t.root().join("usr/bin/env"), b"\x7fELF env");
    exec(&t.root().join("lbin/tool"), b"\x7fELF tool");
    exec(&t.root().join("found"), b"#!/usr/bin/env tool\n");
    exec(&t.root().join("missing"), b"#!/usr/bin/env nothing\n");
    exec(&t.root().join("flags"), b"#!/usr/bin/env -S tool -x\n");

    let envp = ptr_array(&mut t, &["A=1", "PATH=/other:/lbin"]);
    let run = |t: &mut T, script: &str| {
        let (path, argv) = (t.s(script), ptr_array(t, &["x", "arg"]));
        let Action::Exec(e) = dispatch(&t.p, &mut t.th, &t.m, nr::EXECVE, [path, argv, envp, 0, 0, 0]) else { panic!("not an exec") };
        e
    };
    let e = run(&mut t, "/found");
    assert_eq!(e.elf, b"\x7fELF tool", "the Linux tool on PATH, not env");
    assert_eq!(e.argv, [&b"tool"[..], b"/found", b"arg"], "what env would pass it");
    assert!(e.exe.ends_with(b"/lbin/tool"));
    let e = run(&mut t, "/missing");
    assert_eq!(e.elf, b"\x7fELF env", "nothing on PATH: env itself");
    assert_eq!(e.argv, [&b"/usr/bin/env"[..], b"nothing", b"/missing", b"arg"]);
    let e = run(&mut t, "/flags");
    assert_eq!(e.elf, b"\x7fELF env", "options are env's to handle");
}

#[test]
fn mac_programs_get_a_term_macos_knows() {
    use super::process::mac_has_terminfo;
    assert!(mac_has_terminfo(b"xterm-256color", &[]), "the system's");
    assert!(!mac_has_terminfo(b"foot", &[]));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("f")).unwrap();
    std::fs::write(dir.path().join("f/foot"), b"\x1e\x02 ncurses 6's format").unwrap();
    let terminfo = format!("TERMINFO={}", dir.path().display()).into_bytes();
    assert!(!mac_has_terminfo(b"foot", std::slice::from_ref(&terminfo)), "macOS can't read it");
    std::fs::create_dir_all(dir.path().join("66")).unwrap();
    std::fs::write(dir.path().join("66/foot"), b"\x1a\x01 the old format").unwrap();
    let dirs = format!("TERMINFO_DIRS=/nope:{}", dir.path().display()).into_bytes();
    assert!(mac_has_terminfo(b"foot", &[dirs]), "a readable entry, by hex code");
}

#[test]
fn clone_as_fork_and_what_it_refuses() {
    let mut t = T::new();
    const SIGCHLD: u64 = 17;
    let mut a = [0; 6];
    a[0] = SIGCHLD;
    assert!(matches!(dispatch(&t.p, &mut t.th, &t.m, nr::CLONE, a), Action::Fork(f) if !f.vfork && f.stack == 0));
    // posix_spawn's clone: shared memory, but vfork.
    a[0] = 0x100 | 0x4000 | SIGCHLD;
    a[1] = 0x7000;
    assert!(matches!(dispatch(&t.p, &mut t.th, &t.m, nr::CLONE, a), Action::Fork(f) if f.vfork && f.stack == 0x7000));
    assert_eq!(t.call(nr::CLONE, &[0x100 | SIGCHLD]), -errno::EINVAL, "shared memory without vfork");
    assert!(matches!(dispatch(&t.p, &mut t.th, &t.m, nr::CLONE, [0x10000 | 0x800 | 0x100 | 0x400 | 0x200, 0x8000, 0, 0, 0, 0]), Action::Spawn(s) if s.stack == 0x8000));
    assert_eq!(t.call(nr::CLONE, &[0x10000]), -errno::EINVAL, "a thread must share memory and handlers");
    assert_eq!(t.call(nr::CLONE3, &[0, 0]), -errno::ENOSYS, "glibc falls back on clone");
}

#[test]
fn signal_numbers_translate() {
    use super::signal::{from_darwin, to_darwin};
    for sig in 1..=31 {
        match to_darwin(sig) {
            Some(d) => assert_eq!(from_darwin(d), sig, "round trip of {sig}"),
            None => assert!(sig == 16 || sig == 30, "{sig} has no Darwin match"),
        }
    }
    assert_eq!(to_darwin(10), Some(libc::SIGUSR1));
    assert_eq!(to_darwin(7), Some(libc::SIGBUS));
    assert_eq!(from_darwin(libc::SIGCHLD), 17);
}

#[test]
fn fork_state_round_trips() {
    use crate::wire::{Decoder, Encoder};
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut t = T::new();
    let fd = t.open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    t.p.set_exe(b"/bin/prog".to_vec());
    let ign = t.put(&[1u64.to_le_bytes(), [0; 8], [0; 8], [0; 8]].concat());
    t.ok(nr::RT_SIGACTION, &[13, ign, 0, 8]);
    let mut e = Encoder::default();
    let host: Vec<OwnedFd> = t.p.encode_for_fork(&t.th, &mut e).into_iter().map(|h| unsafe { OwnedFd::from_raw_fd(libc::dup(h)) }).collect();
    let (ctl, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let (q, mut qt) = Process::decode_forked(&mut Decoder(&e.0), host, ctl.into()).unwrap();
    assert_eq!(q.exe(), t.p.exe());
    assert_eq!(q.fs.root(), t.p.fs.root());
    assert!(q.fds.cloexec(fd).unwrap());
    assert_eq!(q.fds.get(fd).unwrap().path, t.p.fds.get(fd).unwrap().path);
    // SIGPIPE stays ignored; exec drops close-on-exec fds and keeps SIG_IGN.
    q.exec_done(&mut qt, b"/bin/other".to_vec(), b"other");
    assert!(q.fds.get(fd).is_err());
    let old = t.buf(32);
    let mut a = [0; 6];
    a[..4].copy_from_slice(&[13, 0, old, 8]);
    assert_eq!(dispatch(&q, &mut qt, &t.m, nr::RT_SIGACTION, a), Action::Return(0));
    assert_eq!(t.get(old, 8), 1u64.to_le_bytes());
}

/// `newfstatat` on a deep path through the dispatcher vs. a native `stat`.
/// Run with `cargo test --release -p linux-abi -- --ignored --nocapture bench`.
#[test]
#[ignore = "benchmark"]
fn bench_stat_path() {
    const N: u32 = 100_000;
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/syscall/net.rs");
    let mut t = T { p: Process::new("/"), ..T::new() };
    let p = t.s(path);
    let buf = t.buf(128);
    let start = std::time::Instant::now();
    for _ in 0..N {
        t.ok(nr::NEWFSTATAT, &[FDCWD, p, buf, 0]);
    }
    let guest = start.elapsed() / N;
    let c = std::ffi::CString::new(path).unwrap();
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let start = std::time::Instant::now();
    for _ in 0..N {
        assert_eq!(unsafe { libc::stat(c.as_ptr(), &mut st) }, 0);
    }
    let native = start.elapsed() / N;
    let depth = path.matches('/').count();
    println!("stat of a {depth}-component path: native {native:?}, dispatcher {guest:?}");
}

#[test]
fn machine_info_in_proc_and_etc() {
    let mut t = T::new();
    let read_all = |t: &mut T, path: &str| -> String {
        let fd = t.open(path, O_RDONLY);
        let s = String::from_utf8(t.read_n(fd, 1 << 16)).unwrap();
        t.ok(nr::CLOSE, &[fd]);
        s
    };
    assert!(read_all(&mut t, "/proc/cpuinfo").starts_with("processor\t: 0\n"));
    let meminfo = read_all(&mut t, "/proc/meminfo");
    let total: u64 = meminfo.lines().next().unwrap().trim_start_matches("MemTotal:").trim().trim_end_matches(" kB").parse().unwrap();
    assert!(total > 0 && meminfo.contains("\nMemAvailable:"));
    // A separate root: it, then /proc.
    let mounts = read_all(&mut t, "/proc/self/mounts");
    assert!(mounts.lines().next().unwrap().split(' ').nth(1) == Some("/"), "{mounts}");
    assert!(mounts.contains("proc /proc proc "));
    assert!(read_all(&mut t, "/proc/self/mountinfo").starts_with("20 1 "));
    assert_eq!(read_all(&mut t, "/proc/uptime").split(' ').count(), 2);
    assert_eq!(t.stat_mode("/proc/meminfo", 0) & 0o170000, 0o100000, "a regular file");
    // A root without /etc/os-release gets one saying what this is; one with
    // its own keeps it.
    assert!(read_all(&mut t, "/etc/os-release").contains("ID=macwayland\n"));
    std::fs::create_dir(t.root().join("etc")).unwrap();
    std::fs::write(t.root().join("etc/os-release"), "ID=alpine\n").unwrap();
    assert_eq!(read_all(&mut t, "/etc/os-release"), "ID=alpine\n");
}

#[test]
fn record_locks_and_copy_fallbacks() {
    let mut t = T::new();
    let fd = t.open("/f", O_CREAT | O_RDWR);
    // struct flock: type, whence, start, len, pid.
    let flock = |t: &mut T, kind: i16| {
        let mut b = Vec::new();
        b.extend(kind.to_le_bytes());
        b.extend(0i16.to_le_bytes());
        b.extend([0u8; 4]);
        b.extend(0i64.to_le_bytes());
        b.extend(0i64.to_le_bytes());
        b.extend(0i32.to_le_bytes());
        b.extend([0u8; 4]);
        t.put(&b)
    };
    const F_GETLK: u64 = 5;
    const F_SETLK: u64 = 6;
    let lock = flock(&mut t, 1);
    t.ok(nr::FCNTL, &[fd, F_SETLK, lock]);
    // Our own lock doesn't conflict with us: F_GETLK says unlocked.
    let query = flock(&mut t, 1);
    t.ok(nr::FCNTL, &[fd, F_GETLK, query]);
    assert_eq!(t.get(query, 2), 2i16.to_le_bytes());
    let unlock = flock(&mut t, 2);
    t.ok(nr::FCNTL, &[fd, F_SETLK, unlock]);
    let bad = flock(&mut t, 9);
    assert_eq!(t.call(nr::FCNTL, &[fd, F_SETLK, bad]), -errno::EINVAL);

    // Callers copy with read and write when these say no.
    assert_eq!(t.call(nr::IOCTL, &[fd, 0x4004_9409, 3]), -errno::EOPNOTSUPP, "FICLONE");
    assert_eq!(t.call(nr::COPY_FILE_RANGE, &[3, 0, fd, 0, 10, 0]), -errno::EXDEV);
    assert_eq!(t.call(nr::PRCTL, &[35, 0, 0]), -errno::EPERM, "PR_SET_MM");
}

/// Read an inotify instance until `done` says the events so far are all
/// expected (or 5 s pass): (wd, mask, cookie, name) each.
fn inotify_events(t: &mut T, fd: u64, done: impl Fn(&[(i32, u32, u32, String)]) -> bool) -> Vec<(i32, u32, u32, String)> {
    let mut events = Vec::new();
    let start = std::time::Instant::now();
    while !done(&events) && start.elapsed() < std::time::Duration::from_secs(5) {
        let b = t.buf(4096);
        let n = t.call(nr::READ, &[fd, b, 4096]);
        if n == -errno::EAGAIN {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        assert!(n > 0, "read: {n}");
        let bytes = t.get(b, n as usize);
        let mut at = 0;
        while at < bytes.len() {
            let word = |i: usize| u32::from_le_bytes(bytes[at + i..at + i + 4].try_into().unwrap());
            let len = word(12) as usize;
            let name = &bytes[at + 16..at + 16 + len];
            let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(len)]).into_owned();
            events.push((word(0) as i32, word(4), word(8), name));
            at += 16 + len;
        }
    }
    events
}

#[test]
fn inotify_reports_changes() {
    const CREATE: u32 = 0x100;
    const DELETE: u32 = 0x200;
    const MODIFY: u32 = 0x2;
    const CLOSE_WRITE: u32 = 0x8;
    const MOVED_FROM: u32 = 0x40;
    const MOVED_TO: u32 = 0x80;
    const IGNORED: u32 = 0x8000;
    const ISDIR: u32 = 0x4000_0000;
    let mut t = T::new();
    let d = t.s("/d");
    t.ok(nr::MKDIRAT, &[FDCWD, d, 0o755]);
    let fd = t.ok(nr::INOTIFY_INIT1, &[0o4000 | 0o2000000]);
    let (dir, file) = (t.s("/d"), t.s("/d/watched"));
    let all = CREATE | DELETE | MODIFY | CLOSE_WRITE | MOVED_FROM | MOVED_TO;
    let wd = t.ok(nr::INOTIFY_ADD_WATCH, &[fd, dir, all as u64]) as i32;
    // The same directory again: the same watch.
    assert_eq!(t.ok(nr::INOTIFY_ADD_WATCH, &[fd, dir, all as u64]) as i32, wd);
    let b = t.buf(64);
    assert_eq!(t.call(nr::READ, &[fd, b, 64]), -errno::EAGAIN);
    // Not a directory, or not there.
    let missing = t.s("/nope");
    assert_eq!(t.call(nr::INOTIFY_ADD_WATCH, &[fd, missing, all as u64]), -errno::ENOENT);

    // An epoll instance sees it become readable.
    let ep = t.ok(nr::EPOLL_CREATE1, &[0]);
    let e = t.put(&[1u32.to_le_bytes(), [0; 4], 7u64.to_le_bytes()[..4].try_into().unwrap(), [0; 4]].concat());
    t.ok(nr::EPOLL_CTL, &[ep, 1, fd, e]);
    let out = t.buf(64);
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 4, 0, 0]), 0);

    // Created and written.
    let f = t.open("/d/a", 0o101);
    t.write_str(f, "hello");
    t.ok(nr::CLOSE, &[f]);
    assert_eq!(t.ok(nr::EPOLL_PWAIT, &[ep, out, 4, 2000, 0]), 1);
    // FIONREAD: what's queued.
    let n = t.buf(4);
    t.ok(nr::IOCTL, &[fd, abi::ioctl::FIONREAD, n]);
    assert!(t.u32_at(n) >= 32);
    let events = inotify_events(&mut t, fd, |e| e.iter().any(|e| e.1 == CLOSE_WRITE));
    assert_eq!(events[0], (wd, CREATE, 0, "a".into()), "{events:?}");
    assert!(events.contains(&(wd, MODIFY, 0, "a".into())), "{events:?}");

    // Renamed: the two halves share a cookie.
    let (a, b2) = (t.s("/d/a"), t.s("/d/b"));
    t.ok(nr::RENAMEAT2, &[FDCWD, a, FDCWD, b2, 0]);
    let events = inotify_events(&mut t, fd, |e| e.len() >= 2);
    let from = events.iter().find(|e| e.1 == MOVED_FROM).expect("moved from");
    let to = events.iter().find(|e| e.1 == MOVED_TO).expect("moved to");
    assert_eq!((from.3.as_str(), to.3.as_str()), ("a", "b"));
    assert!(from.2 != 0 && from.2 == to.2, "{events:?}");

    // A directory made, then a file deleted.
    let sub = t.s("/d/sub");
    t.ok(nr::MKDIRAT, &[FDCWD, sub, 0o755]);
    assert_eq!(inotify_events(&mut t, fd, |e| !e.is_empty()), [(wd, CREATE | ISDIR, 0, "sub".into())]);
    t.ok(nr::UNLINKAT, &[FDCWD, b2, 0]);
    assert_eq!(inotify_events(&mut t, fd, |e| !e.is_empty()), [(wd, DELETE, 0, "b".into())]);

    // A watch on one file: its changes have no name.
    let w = t.open("/d/watched", 0o101);
    inotify_events(&mut t, fd, |e| e.iter().any(|e| e.1 == CREATE));
    let wd2 = t.ok(nr::INOTIFY_ADD_WATCH, &[fd, file, MODIFY as u64]) as i32;
    assert_ne!(wd2, wd);
    t.write_str(w, "more");
    // macOS reports a write when the file is closed.
    t.ok(nr::CLOSE, &[w]);
    let events = inotify_events(&mut t, fd, |e| e.iter().any(|e| e.0 == wd2));
    assert!(events.contains(&(wd2, MODIFY, 0, String::new())), "{events:?}");

    // Removed: IN_IGNORED, and then nothing more from it.
    t.ok(nr::INOTIFY_RM_WATCH, &[fd, wd as u64]);
    assert_eq!(t.call(nr::INOTIFY_RM_WATCH, &[fd, wd as u64]), -errno::EINVAL);
    let events = inotify_events(&mut t, fd, |e| e.iter().any(|e| e.1 == IGNORED));
    assert_eq!(events.last(), Some(&(wd, IGNORED, 0, String::new())));
    let c = t.open("/d/c", 0o101);
    t.ok(nr::CLOSE, &[c]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let events = inotify_events(&mut t, fd, |_| true);
    assert!(events.iter().all(|e| e.0 != wd), "{events:?}");
    t.ok(nr::CLOSE, &[fd]);
}

#[test]
fn inotify_goes_to_a_forked_child() {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut t = T::new();
    let d = t.s("/d");
    t.ok(nr::MKDIRAT, &[FDCWD, d, 0o755]);
    let fd = t.ok(nr::INOTIFY_INIT1, &[0o4000]);
    let wd = t.ok(nr::INOTIFY_ADD_WATCH, &[fd, d, 0x100 /* IN_CREATE */]);
    // As a fork does: the table encoded, and decoded with its host fds.
    let mut e = crate::wire::Encoder::default();
    let host = t.p.fds.encode(&mut e);
    let mut fds = host.iter().map(|&h| unsafe { OwnedFd::from_raw_fd(libc::dup(h)) });
    let child = crate::fs::FdTable::decode(&mut crate::wire::Decoder(&e.0), &mut fds, &t.p.inbox).unwrap();
    let file = child.get(fd).unwrap();
    std::fs::write(t.root().join("d/new"), "x").unwrap();
    let mut buf = [0u8; 64];
    let start = std::time::Instant::now();
    let n = loop {
        match inotify_read(&t.p, &t.th, &file, &mut buf) {
            Ok(n) => break n as usize,
            Err(e) if e == errno::EAGAIN && start.elapsed().as_secs() < 5 => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("read: {e}"),
        }
    };
    assert_eq!(n, 32);
    assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()) as u64, wd);
    assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 0x100);
    assert_eq!(&buf[16..20], b"new\0");
}

#[test]
fn memfd_seals() {
    const ADD_SEALS: u64 = 1033;
    const GET_SEALS: u64 = 1034;
    const SEAL: u64 = 1;
    const SHRINK: u64 = 2;
    const GROW: u64 = 4;
    const WRITE: u64 = 8;
    let mut t = T::new();
    let name = t.s("sealed");
    let fd = t.ok(nr::MEMFD_CREATE, &[name, 2 /* MFD_ALLOW_SEALING */]);
    assert_eq!(t.ok(nr::FCNTL, &[fd, GET_SEALS]), 0);
    t.ok(nr::FTRUNCATE, &[fd, 8192]);
    // Firefox's: its size fixed, then it checks the seal is there.
    t.ok(nr::FCNTL, &[fd, ADD_SEALS, SHRINK | GROW]);
    assert_eq!(t.ok(nr::FCNTL, &[fd, GET_SEALS]), SHRINK | GROW);
    assert_eq!(t.call(nr::FTRUNCATE, &[fd, 4096]), -errno::EPERM);
    assert_eq!(t.call(nr::FTRUNCATE, &[fd, 16384]), -errno::EPERM);
    t.ok(nr::FTRUNCATE, &[fd, 8192]); // the same size: fine
    // Writing within it is fine; past the end would grow it.
    let b = t.put(b"data");
    assert_eq!(t.ok(nr::PWRITE64, &[fd, b, 4, 0]), 4);
    assert_eq!(t.call(nr::PWRITE64, &[fd, b, 4, 8190]), -errno::EPERM);
    // Sealed against writes: no writes, no writable shared mappings.
    t.ok(nr::FCNTL, &[fd, ADD_SEALS, WRITE]);
    assert_eq!(t.call(nr::PWRITE64, &[fd, b, 4, 0]), -errno::EPERM);
    let map = |t: &mut T, prot: u64| t.call(nr::MMAP, &[0, 8192, prot, 1 /* MAP_SHARED */, fd, 0]);
    assert_eq!(map(&mut t, 3), -errno::EPERM);
    // No more seals once F_SEAL_SEAL is on.
    t.ok(nr::FCNTL, &[fd, ADD_SEALS, SEAL]);
    assert_eq!(t.call(nr::FCNTL, &[fd, ADD_SEALS, 0x10]), -errno::EPERM);
    assert_eq!(t.ok(nr::FCNTL, &[fd, GET_SEALS]), SEAL | SHRINK | GROW | WRITE);
    // A dup sees the same seals (they're the object's).
    let dup = t.ok(nr::DUP, &[fd]);
    assert_eq!(t.ok(nr::FCNTL, &[dup, GET_SEALS]), SEAL | SHRINK | GROW | WRITE);
}

/// A fork inside the host (many processes in one VM): open files shared
/// (one offset), dispositions copied; signals posted from another process
/// taken in, ignored ones dropped; SIGCHLD's disposition read for the
/// kernel.
#[test]
fn a_fork_in_the_host_shares_open_files() {
    let mut t = T::new();
    let fd = t.open("f", O_RDWR | O_CREAT);
    t.write_str(fd, "abcdef");
    t.ok(nr::LSEEK, &[fd, 0, 0]);
    let ign = t.put(&[1u64.to_le_bytes(), [0; 8], [0; 8], [0; 8]].concat());
    t.ok(nr::RT_SIGACTION, &[12, ign, 0, 8]); // SIGUSR2 ignored
    let (child, mut cth) = t.p.fork_in_host(&t.th, 4242);
    assert_eq!((child.pid, cth.tid), (4242, 4242));
    let b = t.buf(3);
    assert_eq!(dispatch(&child, &mut cth, &t.m, nr::READ, [fd, b, 3, 0, 0, 0]), Action::Return(3));
    assert_eq!(t.get(b, 3), b"abc");
    assert_eq!(t.ok(nr::LSEEK, &[fd, 0, 1]), 3, "one open file description: the offset moved for the parent too");
    assert_eq!(dispatch(&child, &mut cth, &t.m, nr::CLOSE, [fd, 0, 0, 0, 0, 0]), Action::Return(0));
    assert_eq!(t.read_n(fd, 3), b"def", "closed in the child only");

    child.post(None, SigInfo::user(12, signal::SI_USER, 77));
    child.post(None, SigInfo::user(10, signal::SI_USER, 77));
    assert_eq!(child.next_delivery(&mut cth), Some(Delivery::Kill(10)), "SIGUSR2 ignored (copied), SIGUSR1 kills");
    assert_eq!(child.child_policy(), (false, true));
    t.ok(nr::RT_SIGACTION, &[17, ign, 0, 8]);
    assert_eq!(t.p.child_policy(), (true, true), "SIGCHLD ignored: children reaped as they end");
}

/// A thread blocked in a host call is woken by its waker's pipe alone, with
/// no interrupt to its host thread at all (the extreme of an interrupt lost
/// just before the call sleeps): a read of an empty pipe, a sleep, a poll
/// and a read of an eventfd all end once the process is ending.
#[test]
fn a_blocked_call_wakes_without_an_interrupt() {
    use std::sync::Arc;
    for case in ["read", "nanosleep", "ppoll", "eventfd"] {
        let root = tempfile::tempdir().unwrap();
        let p = Arc::new(Process::new(root.path()));
        let (tx, rx) = std::sync::mpsc::channel();
        let p2 = p.clone();
        let blocked = std::thread::spawn(move || {
            let mut th = Thread::main(&p2);
            // Its wake does nothing: only the waker's pipe can end the call.
            p2.register_thread(&th, Box::new(|| {}));
            let m = FlatMem(std::cell::RefCell::new(vec![0; 4096]));
            let call = |th: &mut Thread, nr: u64, a: [u64; 6]| match dispatch(&*p2, th, &m, nr, a) {
                Action::Return(v) => v,
                other => panic!("{other:?}"),
            };
            let (nr, args) = match case {
                "read" | "ppoll" => {
                    assert_eq!(call(&mut th, nr::PIPE2, [BASE, 0, 0, 0, 0, 0]), 0);
                    let mut fds = [0u8; 8];
                    m.read(BASE, &mut fds).unwrap();
                    let rfd = u32::from_le_bytes(fds[..4].try_into().unwrap()) as u64;
                    if case == "read" {
                        (nr::READ, [rfd, BASE + 64, 1, 0, 0, 0])
                    } else {
                        m.write(BASE + 64, &[rfd as u8, 0, 0, 0, 1, 0, 0, 0]).unwrap(); // { fd, POLLIN }
                        (nr::PPOLL, [BASE + 64, 1, 0, 0, 0, 0])
                    }
                }
                "nanosleep" => {
                    m.write(BASE + 64, &[100u64.to_le_bytes(), 0u64.to_le_bytes()].concat()).unwrap();
                    (nr::NANOSLEEP, [BASE + 64, 0, 0, 0, 0, 0])
                }
                _ => {
                    let efd = call(&mut th, nr::EVENTFD2, [0, 0, 0, 0, 0, 0]) as u64;
                    (nr::READ, [efd, BASE + 64, 8, 0, 0, 0])
                }
            };
            tx.send(()).unwrap();
            call(&mut th, nr, args)
        });
        rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let start = std::time::Instant::now();
        p.set_exiting(true);
        assert_eq!(blocked.join().unwrap(), -errno::EINTR, "{case}");
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{case}");
    }
}

/// Another process's threads are known by their tids alone: those its
/// threads get say which process they may be of.
#[test]
fn a_tid_says_which_process_it_may_be_of() {
    let p = Process::new("/");
    let tid = p.new_tid();
    assert!(Process::may_be_thread_of(p.pid, tid));
    assert!(Process::may_be_thread_of(p.pid, p.pid), "the first thread's tid is the pid");
    assert!(!Process::may_be_thread_of(p.pid + 1, tid));
    assert!(Process::is_other_thread(tid) && !Process::is_other_thread(p.pid));
}
