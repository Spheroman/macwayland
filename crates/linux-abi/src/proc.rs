//! The emulated `/proc`: the system files programs and `ps`/`top` read
//! (`cpuinfo`, `meminfo`, `stat`, `mounts`, `uptime`, `loadavg`, `version`,
//! from [`crate::hostinfo`] and the host), and `/proc/<pid>` for every
//! process the kernel has (`self` and `thread-self` too): `stat`, `status`,
//! `statm`, `cmdline`, `environ`, `comm`, `maps`, `fd/`, `exe`, `cwd`,
//! `task/<tid>/` and `mounts`/`mountinfo`. That's every process of the host,
//! from its process table and from each process's own state and memory
//! ([`Kernel::with_process`]) (for a process on its own, `HostKernel`, the
//! process itself). Directories list what's in them.
//!
//! [`Kernel::with_process`]: crate::syscall::Kernel::with_process

use std::{
    ffi::CStr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use crate::{
    errno,
    fs::{DirEntry, DirSnapshot, FdEntry, Target},
    hostinfo,
    memory::GuestMemory,
    shm,
    syscall::{last_errno, ProcStatus, Process},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    /// `/proc`.
    Root,
    /// A file of the machine's, whose contents are made when it's opened.
    Info(Generated),
    /// Something of process `pid` (its thread `tid`, under `task/`).
    Pid { pid: i64, tid: Option<i64>, what: Of },
}

/// What of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Of {
    Dir,
    FdDir,
    TaskDir,
    Exe,
    Cwd,
    Fd(u64),
    Maps,
    Stat,
    Status,
    Statm,
    Cmdline,
    Environ,
    Comm,
    Mounts,
    MountInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Generated {
    CpuInfo,
    MemInfo,
    Mounts,
    Uptime,
    LoadAvg,
    Version,
    Stat,
    PidMax,
}

/// The files of a process's directory (and of a thread's, under `task/`).
const PID_FILES: [(&[u8], Of); 11] = [
    (b"stat", Of::Stat),
    (b"status", Of::Status),
    (b"statm", Of::Statm),
    (b"cmdline", Of::Cmdline),
    (b"environ", Of::Environ),
    (b"comm", Of::Comm),
    (b"maps", Of::Maps),
    (b"exe", Of::Exe),
    (b"cwd", Of::Cwd),
    (b"mounts", Of::Mounts),
    (b"mountinfo", Of::MountInfo),
];

const MACHINE_FILES: [(&[u8], Generated); 7] = [
    (b"cpuinfo", Generated::CpuInfo),
    (b"meminfo", Generated::MemInfo),
    (b"mounts", Generated::Mounts),
    (b"uptime", Generated::Uptime),
    (b"loadavg", Generated::LoadAvg),
    (b"version", Generated::Version),
    (b"stat", Generated::Stat),
];

fn number(s: &[u8]) -> Option<i64> {
    std::str::from_utf8(s).ok()?.parse().ok().filter(|&n: &i64| n > 0)
}

/// Run `f` with process `pid`'s state and memory: the caller's own, or
/// another's the kernel runs.
fn with<T>(p: &Process, mem: &dyn GuestMemory, pid: i64, f: impl FnOnce(&Process, &dyn GuestMemory) -> T) -> Result<T, i64> {
    if pid == p.pid {
        return Ok(f(p, mem));
    }
    let mut f = Some(f);
    let mut out = None;
    p.kernel.with_process(p, pid, &mut |q, m| out = f.take().map(|f| f(q, m)))?;
    out.ok_or(errno::ESRCH)
}

/// What `/proc/<pid>` shows of a process this kernel doesn't run (one in
/// another host, docs/processes.md step 9), from its host: what would
/// otherwise be read from its state and memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteView {
    pub exe: Vec<u8>,
    pub cwd: Vec<u8>,
    pub tids: Vec<i64>,
    /// Its fds, and where each points (`fd/N`'s link).
    pub fds: Vec<(u64, Vec<u8>)>,
    pub environ: Vec<u8>,
    pub maps: Vec<u8>,
    pub mounts: Vec<u8>,
    pub mountinfo: Vec<u8>,
}

impl RemoteView {
    /// Process `q`'s (with memory `m`), for another host to show.
    pub fn of(q: &Process, m: &dyn GuestMemory) -> RemoteView {
        RemoteView {
            exe: q.exe(),
            cwd: q.fs.cwd(),
            tids: q.thread_ids(),
            fds: q.fds.numbers().into_iter().filter_map(|n| Some((n, fd_link(q, n).ok()?))).collect(),
            environ: strings(q, m, true),
            maps: maps(q, m),
            mounts: mounts(q, false).into_bytes(),
            mountinfo: mounts(q, true).into_bytes(),
        }
    }

    pub fn encode(&self, e: &mut crate::wire::Encoder) {
        e.bytes(&self.exe).bytes(&self.cwd).u64(self.tids.len() as u64);
        for &t in &self.tids {
            e.i64(t);
        }
        e.u64(self.fds.len() as u64);
        for (n, link) in &self.fds {
            e.u64(*n).bytes(link);
        }
        e.bytes(&self.environ).bytes(&self.maps).bytes(&self.mounts).bytes(&self.mountinfo);
    }

    pub fn decode(d: &mut crate::wire::Decoder) -> Result<RemoteView, crate::wire::Truncated> {
        let (exe, cwd) = (d.bytes()?, d.bytes()?);
        let tids = (0..d.count(8)?).map(|_| d.i64()).collect::<Result<_, _>>()?;
        let fds = (0..d.count(16)?).map(|_| Ok((d.u64()?, d.bytes()?))).collect::<Result<_, crate::wire::Truncated>>()?;
        Ok(RemoteView { exe, cwd, tids, fds, environ: d.bytes()?, maps: d.bytes()?, mounts: d.bytes()?, mountinfo: d.bytes()? })
    }
}

/// Process `pid` as its host shows it, if it's another host's.
fn remote(p: &Process, pid: i64) -> Result<RemoteView, i64> {
    p.kernel.remote_view(p, pid)
}

/// Parse a path below `/proc` (e.g. `self/fd/3`).
fn parse(p: &Process, mem: &dyn GuestMemory, rest: &[u8]) -> Result<Node, i64> {
    let parts: Vec<&[u8]> = rest.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let Some((&who, below)) = parts.split_first() else { return Ok(Node::Root) };
    if below.is_empty()
        && let Some(&(_, info)) = MACHINE_FILES.iter().find(|(name, _)| *name == who)
    {
        return Ok(Node::Info(info));
    }
    // How wide a pid gets (`top` sizes its column by it).
    if parts == [b"sys" as &[u8], b"kernel", b"pid_max"] {
        return Ok(Node::Info(Generated::PidMax));
    }
    let (pid, mut tid) = match who {
        b"self" => (p.pid, None),
        // The calling thread isn't known here: the main one's.
        b"thread-self" => (p.pid, Some(p.pid)),
        n => (number(n).ok_or(errno::ENOENT)?, None),
    };
    let status = p.kernel.proc_status(p, pid).map_err(|_| errno::ENOENT)?;
    let mut below = below;
    if let [b"task", rest @ ..] = below {
        match rest {
            [] => return Ok(Node::Pid { pid, tid: None, what: Of::TaskDir }),
            [t, more @ ..] => {
                let t = number(t).ok_or(errno::ENOENT)?;
                let known = t == pid
                    || with(p, mem, pid, |q, _| q.thread_ids().contains(&t)).or_else(|_| remote(p, pid).map(|r| r.tids.contains(&t))).unwrap_or(false);
                if !known {
                    return Err(errno::ENOENT);
                }
                tid = Some(t);
                below = more;
            }
        }
    }
    let zombie = status.state == b'Z';
    let what = match below {
        [] => Of::Dir,
        [b"fd"] => Of::FdDir,
        [b"fd", n] => {
            let n = std::str::from_utf8(n).ok().and_then(|n| n.parse().ok()).ok_or(errno::ENOENT)?;
            with(p, mem, pid, |q, _| q.fds.get(n).is_ok())
                .or_else(|_| remote(p, pid).map(|r| r.fds.iter().any(|&(m, _)| m == n)))
                .ok()
                .filter(|&ok| ok)
                .ok_or(errno::ENOENT)?;
            Of::Fd(n)
        }
        [name] => PID_FILES.iter().find(|(n, _)| n == name).map(|&(_, of)| of).ok_or(errno::ENOENT)?,
        _ => return Err(errno::ENOENT),
    };
    // A zombie has its table entry and nothing else.
    if zombie && !matches!(what, Of::Dir | Of::Stat | Of::Status | Of::Statm | Of::Cmdline | Of::Comm) {
        return Err(errno::ENOENT);
    }
    Ok(Node::Pid { pid, tid, what })
}

/// What `/proc/<pid>/fd/N` points to.
fn fd_link(p: &Process, fd: u64) -> Result<Vec<u8>, i64> {
    let entry = p.fds.get(fd)?;
    if let Some(path) = &entry.path {
        return Ok(path.clone());
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(entry.host.as_raw_fd(), &mut st) } < 0 {
        return Err(last_errno());
    }
    Ok(match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => format!("socket:[{}]", st.st_ino).into_bytes(),
        libc::S_IFIFO => format!("pipe:[{}]", st.st_ino).into_bytes(),
        // A file opened by the host (stdin from /dev/null, a terminal): the
        // path macOS knows it by.
        _ => {
            let mut buf = [0u8; libc::PATH_MAX as usize];
            if unsafe { libc::fcntl(entry.host.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == 0 {
                buf.iter().position(|&b| b == 0).map_or(buf.to_vec(), |n| buf[..n].to_vec())
            } else {
                b"anon_inode:[unknown]".to_vec()
            }
        }
    })
}

/// Host path of process `p`'s guest path, following symlinks.
fn host_of(p: &Process, guest: &[u8]) -> Result<std::ffi::CString, i64> {
    match p.fs.resolve(&p.fds, crate::abi::at::FDCWD as u64, guest, true)?.target {
        Target::Host(h) => Ok(h),
        Target::Proc(_) | Target::Shm(_) => Err(errno::ELOOP),
    }
}

pub(crate) fn readlink(p: &Process, mem: &dyn GuestMemory, rest: &[u8]) -> Result<Vec<u8>, i64> {
    // /proc/self and /proc/thread-self are links to the process's own.
    match rest.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect::<Vec<_>>().as_slice() {
        [b"self"] => return Ok(p.pid.to_string().into_bytes()),
        [b"thread-self"] => return Ok(format!("{}/task/{}", p.pid, p.pid).into_bytes()),
        _ => {}
    }
    match parse(p, mem, rest)? {
        Node::Pid { pid, what: Of::Exe, .. } => with(p, mem, pid, |q, _| q.exe()).or_else(|_| remote(p, pid).map(|r| r.exe))?.pipe_nonempty(),
        Node::Pid { pid, what: Of::Cwd, .. } => with(p, mem, pid, |q, _| q.fs.cwd()).or_else(|_| remote(p, pid).map(|r| r.cwd)),
        Node::Pid { pid, what: Of::Fd(n), .. } => match with(p, mem, pid, |q, _| fd_link(q, n)) {
            Ok(link) => link,
            Err(_) => remote_fd(p, pid, n),
        },
        _ => Err(errno::EINVAL),
    }
}

/// Where another host's process's fd `n` points.
fn remote_fd(p: &Process, pid: i64, n: u64) -> Result<Vec<u8>, i64> {
    remote(p, pid)?.fds.into_iter().find(|&(m, _)| m == n).map(|(_, link)| link).ok_or(errno::ENOENT)
}

/// A process with no executable has no `exe` link (ENOENT).
trait NonEmpty {
    fn pipe_nonempty(self) -> Result<Vec<u8>, i64>;
}

impl NonEmpty for Vec<u8> {
    fn pipe_nonempty(self) -> Result<Vec<u8>, i64> {
        if self.is_empty() { Err(errno::ENOENT) } else { Ok(self) }
    }
}

pub(crate) fn exists(p: &Process, mem: &dyn GuestMemory, rest: &[u8]) -> Result<(), i64> {
    parse(p, mem, rest).map(|_| ())
}

/// `stat` of a `/proc` path; `lstat` (`follow` false) of one of its links
/// (`self`, `exe`, `cwd`, `fd/N`) is the link's own.
pub(crate) fn stat(p: &Process, mem: &dyn GuestMemory, rest: &[u8], follow: bool) -> Result<libc::stat, i64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if !follow {
        let parts: Vec<&[u8]> = rest.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
        let link = matches!(parts.as_slice(), [b"self"] | [b"thread-self"])
            || matches!(parse(p, mem, rest)?, Node::Pid { what: Of::Exe | Of::Cwd | Of::Fd(_), .. });
        if link {
            st.st_mode = libc::S_IFLNK | 0o777;
            st.st_nlink = 1;
            st.st_uid = unsafe { libc::getuid() };
            st.st_gid = unsafe { libc::getgid() };
            return Ok(st);
        }
    }
    let dir = |st: &mut libc::stat| {
        st.st_mode = libc::S_IFDIR | 0o555;
        st.st_nlink = 2;
    };
    let node = parse(p, mem, rest)?;
    let r = match node {
        Node::Root => {
            dir(&mut st);
            return Ok(st);
        }
        Node::Pid { pid, what: Of::Dir | Of::FdDir | Of::TaskDir, .. } => {
            dir(&mut st);
            // A process's directory is its user's, and dated from its start
            // (what `ps` shows as the user, and some tools as the start).
            st.st_uid = unsafe { libc::getuid() };
            st.st_gid = unsafe { libc::getgid() };
            if let Ok(s) = p.kernel.proc_status(p, pid) {
                let wall = boot_time() as f64 + s.start_ns as f64 / 1e9;
                st.st_mtime = wall as i64;
                st.st_ctime = wall as i64;
            }
            return Ok(st);
        }
        // Like Linux's, these are empty to stat: their size is known only
        // once read.
        Node::Info(_) | Node::Pid { .. } if !matches!(node, Node::Pid { what: Of::Exe | Of::Cwd | Of::Fd(_), .. }) => {
            st.st_mode = libc::S_IFREG | 0o444;
            st.st_nlink = 1;
            st.st_uid = unsafe { libc::getuid() };
            st.st_gid = unsafe { libc::getgid() };
            return Ok(st);
        }
        // Another host's process's: its paths, as this process sees them
        // (hosts of a session share the filesystem's view).
        Node::Pid { pid, what: what @ (Of::Exe | Of::Cwd | Of::Fd(_)), .. } if with(p, mem, pid, |_, _| ()).is_err() => {
            let r = remote(p, pid)?;
            let path = match what {
                Of::Exe => r.exe,
                Of::Cwd => r.cwd,
                Of::Fd(n) => r.fds.into_iter().find(|&(m, _)| m == n).map(|(_, l)| l).filter(|l| l.starts_with(b"/")).ok_or(errno::ENOENT)?,
                _ => unreachable!(),
            };
            unsafe { libc::stat(host_of(p, &path)?.as_ptr(), &mut st) }
        }
        Node::Pid { pid, what: Of::Exe, .. } => with(p, mem, pid, |q, _| -> Result<i32, i64> { Ok(unsafe { libc::stat(host_of(q, &q.exe())?.as_ptr(), &mut st) }) })??,
        Node::Pid { pid, what: Of::Cwd, .. } => with(p, mem, pid, |q, _| -> Result<i32, i64> { Ok(unsafe { libc::stat(host_of(q, &q.fs.cwd())?.as_ptr(), &mut st) }) })??,
        Node::Pid { pid, what: Of::Fd(n), .. } => with(p, mem, pid, |q, _| -> Result<i32, i64> { Ok(unsafe { libc::fstat(q.fds.get(n)?.host.as_raw_fd(), &mut st) }) })??,
        _ => unreachable!(),
    };
    if r < 0 { Err(last_errno()) } else { Ok(st) }
}

/// A directory's listing, as an open fd's (`getdents64`).
fn listing(names: impl IntoIterator<Item = (Vec<u8>, u8)>) -> DirSnapshot {
    let mut entries = vec![DirEntry { ino: 1, kind: libc::DT_DIR, name: b".".to_vec() }, DirEntry { ino: 1, kind: libc::DT_DIR, name: b"..".to_vec() }];
    entries.extend(names.into_iter().enumerate().map(|(i, (name, kind))| DirEntry { ino: 2 + i as u64, kind, name }));
    DirSnapshot { entries, next: 0, fixed: true }
}

/// Open a `/proc` path; returns the new fd's entry.
pub(crate) fn open(p: &Process, mem: &dyn GuestMemory, rest: &[u8]) -> Result<FdEntry, i64> {
    let mut path = b"/proc/".to_vec();
    path.extend_from_slice(rest);
    let open_host = |q: &Process, guest: &[u8]| -> Result<FdEntry, i64> {
        let fd = unsafe { libc::open(host_of(q, guest)?.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(last_errno());
        }
        Ok(FdEntry::new(unsafe { OwnedFd::from_raw_fd(fd) }).path(guest.to_vec()))
    };
    let file = |contents: Vec<u8>| -> Result<FdEntry, i64> { Ok(generated(&contents)?.path(path.clone())) };
    let dir = |listing: DirSnapshot| -> Result<FdEntry, i64> { Ok(generated(b"")?.path(path.clone()).dir(listing)) };
    let (pid, tid, what) = match parse(p, mem, rest)? {
        Node::Root => {
            let mut names: Vec<(Vec<u8>, u8)> = MACHINE_FILES.iter().map(|(n, _)| (n.to_vec(), libc::DT_REG)).collect();
            names.push((b"self".to_vec(), libc::DT_LNK));
            names.push((b"thread-self".to_vec(), libc::DT_LNK));
            let mut pids = p.kernel.pids(p);
            pids.sort_unstable();
            names.extend(pids.into_iter().map(|pid| (pid.to_string().into_bytes(), libc::DT_DIR)));
            return dir(listing(names));
        }
        Node::Info(info) => {
            let contents = match info {
                Generated::CpuInfo => hostinfo::cpuinfo(),
                Generated::MemInfo => hostinfo::meminfo(),
                Generated::Mounts => mounts(p, false),
                Generated::Uptime => hostinfo::proc_uptime(),
                Generated::LoadAvg => hostinfo::proc_loadavg(p.kernel.pids(p).into_iter().max().unwrap_or(p.pid)),
                Generated::Version => hostinfo::proc_version(),
                Generated::Stat => system_stat(p),
                Generated::PidMax => "4194304\n".into(),
            };
            return file(contents.into_bytes());
        }
        Node::Pid { pid, tid, what } => (pid, tid, what),
    };
    let status = p.kernel.proc_status(p, pid)?;
    match what {
        Of::Dir => {
            let mut names: Vec<(Vec<u8>, u8)> = PID_FILES.iter().map(|(n, _)| (n.to_vec(), if matches!(*n, b"exe" | b"cwd") { libc::DT_LNK } else { libc::DT_REG })).collect();
            names.push((b"fd".to_vec(), libc::DT_DIR));
            if tid.is_none() {
                names.push((b"task".to_vec(), libc::DT_DIR));
            }
            dir(listing(names))
        }
        Of::FdDir => {
            let fds = with(p, mem, pid, |q, _| q.fds.numbers()).or_else(|_| remote(p, pid).map(|r| r.fds.into_iter().map(|(n, _)| n).collect()))?;
            dir(listing(fds.into_iter().map(|n| (n.to_string().into_bytes(), libc::DT_LNK))))
        }
        Of::TaskDir => {
            let mut tids = with(p, mem, pid, |q, _| q.thread_ids()).or_else(|_| remote(p, pid).map(|r| r.tids)).unwrap_or_default();
            if !tids.contains(&pid) {
                tids.push(pid);
            }
            tids.sort_unstable();
            dir(listing(tids.into_iter().map(|t| (t.to_string().into_bytes(), libc::DT_DIR))))
        }
        Of::Exe => with(p, mem, pid, |q, _| open_host(q, &q.exe())).or_else(|_| remote(p, pid).map(|r| open_host(p, &r.exe)))?,
        Of::Cwd => with(p, mem, pid, |q, _| open_host(q, &q.fs.cwd())).or_else(|_| remote(p, pid).map(|r| open_host(p, &r.cwd)))?,
        // Linux reopens the file; sharing the open file is close enough.
        Of::Fd(n) => with(p, mem, pid, |q, _| -> Result<FdEntry, i64> {
            let entry = q.fds.get(n)?;
            let host = entry.host.try_clone().map_err(|e| errno::from_io(&e))?;
            Ok(FdEntry::new(host).path_opt(entry.path.clone()))
        })
        // Another host's: its file reopened by path, if it has one.
        .or_else(|_| remote_fd(p, pid, n).map(|link| if link.starts_with(b"/") { open_host(p, &link) } else { Err(errno::ENXIO) }))?,
        Of::Maps => file(with(p, mem, pid, |q, m| maps(q, m)).or_else(|_| remote(p, pid).map(|r| r.maps))?),
        Of::Mounts => file(with(p, mem, pid, |q, _| mounts(q, false).into_bytes()).or_else(|_| remote(p, pid).map(|r| r.mounts))?),
        Of::MountInfo => file(with(p, mem, pid, |q, _| mounts(q, true).into_bytes()).or_else(|_| remote(p, pid).map(|r| r.mountinfo))?),
        Of::Cmdline => file(with(p, mem, pid, |q, m| strings(q, m, false)).unwrap_or_else(|_| status.cmdline.clone().unwrap_or_default())),
        Of::Environ => file(with(p, mem, pid, |q, m| strings(q, m, true)).or_else(|_| remote(p, pid).map(|r| r.environ))?),
        Of::Comm => file([comm(p, mem, pid), b"\n".to_vec()].concat()),
        Of::Stat => file(pid_stat(p, mem, pid, tid.unwrap_or(pid), &status).into_bytes()),
        Of::Status => file(pid_status(p, mem, pid, tid.unwrap_or(pid), &status).into_bytes()),
        Of::Statm => {
            let (size, rss) = with(p, mem, pid, |_, m| (vsize(m) / 4096, m.resident_pages())).unwrap_or((0, status.rss_pages));
            file(format!("{size} {rss} 0 0 0 {size} 0\n").into_bytes())
        }
    }
}

/// A process's argument (or environment) strings, read from its memory
/// where the loader put them, as Linux does: what a program wrote over its
/// own `argv` shows.
fn strings(p: &Process, mem: &dyn GuestMemory, environ: bool) -> Vec<u8> {
    let [a0, a1, e0, e1] = p.arg_area();
    let (start, end) = if environ { (e0, e1) } else { (a0, a1) };
    if end <= start || end - start > 1 << 21 {
        return Vec::new();
    }
    let mut out = vec![0u8; (end - start) as usize];
    if mem.read(start, &mut out).is_err() {
        return Vec::new();
    }
    out
}

/// A process's name: its main thread's executable's base name, at most
/// 15 bytes (Linux's TASK_COMM_LEN less the NUL).
fn comm(p: &Process, mem: &dyn GuestMemory, pid: i64) -> Vec<u8> {
    with(p, mem, pid, |q, _| q.comm()).or_else(|_| p.kernel.proc_status(p, pid).map(|s| s.comm)).unwrap_or_default()
}

/// The bytes a process has mapped.
fn vsize(mem: &dyn GuestMemory) -> u64 {
    mem.regions().iter().map(|r| r.end - r.start).sum()
}

/// When the Mac booted, in seconds since the epoch.
fn boot_time() -> i64 {
    let mut boot = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = size_of::<libc::timeval>();
    unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), (&mut boot as *mut libc::timeval).cast(), &mut len, std::ptr::null_mut(), 0) };
    boot.tv_sec
}

/// Clock ticks (Linux's USER_HZ, 100 a second) in `ns`.
fn ticks(ns: u64) -> u64 {
    ns / 10_000_000
}

/// `/proc/<pid>/stat` (or a thread's, under `task/`): the fields `ps`,
/// `top` and `pgrep` read, Linux's 52, in order.
fn pid_stat(p: &Process, mem: &dyn GuestMemory, pid: i64, tid: i64, s: &ProcStatus) -> String {
    let comm = String::from_utf8_lossy(&comm(p, mem, pid)).into_owned();
    let (vsize, rss, area) = with(p, mem, pid, |q, m| (vsize(m), m.resident_pages(), q.arg_area())).unwrap_or((0, s.rss_pages, [0; 4]));
    let (blocked, ignored, caught) = with(p, mem, pid, |q, _| q.signal_masks()).unwrap_or_default();
    let fields: Vec<String> = vec![
        tid.to_string(),
        format!("({comm})"),
        (s.state as char).to_string(),
        s.ppid.to_string(),
        s.pgid.to_string(),
        s.sid.to_string(),
        // tty_nr: /dev/pts/N is Linux's major 136, the minor split as
        // new_encode_dev does.
        s.tty.map_or(0, |n| (136u64 << 8) | (n as u64 & 0xff) | ((n as u64 >> 8) << 20)).to_string(),
        s.tpgid.to_string(),
        "4194304".into(), // flags: PF_RANDOMIZE
        "0".into(),
        "0".into(),
        "0".into(),
        "0".into(), // minflt, cminflt, majflt, cmajflt
        ticks(s.user_ns).to_string(),
        ticks(s.sys_ns).to_string(),
        ticks(s.children_user_ns).to_string(),
        ticks(s.children_sys_ns).to_string(),
        "20".into(), // priority
        "0".into(),  // nice
        s.threads.to_string(),
        "0".into(), // itrealvalue
        ticks(s.start_ns).to_string(),
        vsize.to_string(),
        rss.to_string(),
        u64::MAX.to_string(), // rsslim
        "0".into(),
        "0".into(),
        "0".into(),
        "0".into(),
        "0".into(), // startcode, endcode, startstack, kstkesp, kstkeip
        "0".into(), // signal (pending: obsolete, as Linux)
        blocked.to_string(),
        ignored.to_string(),
        caught.to_string(),
        "0".into(),
        "0".into(),
        "0".into(), // wchan, nswap, cnswap
        "17".into(), // exit_signal
        "0".into(),  // processor
        "0".into(),
        "0".into(),
        "0".into(),
        "0".into(),
        "0".into(), // rt_priority, policy, delayacct, guest_time, cguest_time
        "0".into(),
        "0".into(),
        "0".into(), // start_data, end_data, start_brk
        area[0].to_string(),
        area[1].to_string(),
        area[2].to_string(),
        area[3].to_string(),
        "0".into(), // exit_code
    ];
    fields.join(" ") + "\n"
}

/// `/proc/<pid>/status`.
fn pid_status(p: &Process, mem: &dyn GuestMemory, pid: i64, tid: i64, s: &ProcStatus) -> String {
    let comm = String::from_utf8_lossy(&comm(p, mem, pid)).into_owned();
    let state = match s.state {
        b'R' => "R (running)",
        b'T' => "T (stopped)",
        b'Z' => "Z (zombie)",
        _ => "S (sleeping)",
    };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (vsize, rss, umask, fds) = with(p, mem, pid, |q, m| (vsize(m), m.resident_pages(), q.umask(), q.fds.numbers().len())).unwrap_or((0, s.rss_pages, 0, 0));
    let (blocked, ignored, caught) = with(p, mem, pid, |q, _| q.signal_masks()).unwrap_or_default();
    let kb = |b: u64| b / 1024;
    format!(
        "Name:\t{comm}\nUmask:\t{umask:04o}\nState:\t{state}\nTgid:\t{pid}\nNgid:\t0\nPid:\t{tid}\nPPid:\t{}\nTracerPid:\t0\n\
         Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{gid}\t{gid}\t{gid}\t{gid}\nFDSize:\t{}\nGroups:\t\nNStgid:\t{pid}\nNSpid:\t{tid}\n\
         NSpgid:\t{}\nNSsid:\t{}\nVmPeak:\t{:>8} kB\nVmSize:\t{:>8} kB\nVmLck:\t       0 kB\nVmPin:\t       0 kB\nVmHWM:\t{:>8} kB\n\
         VmRSS:\t{:>8} kB\nRssAnon:\t{:>8} kB\nRssFile:\t       0 kB\nRssShmem:\t       0 kB\nThreads:\t{}\nSigQ:\t0/0\n\
         SigPnd:\t0000000000000000\nShdPnd:\t0000000000000000\nSigBlk:\t{blocked:016x}\nSigIgn:\t{ignored:016x}\nSigCgt:\t{caught:016x}\n\
         CapInh:\t0000000000000000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\nCapBnd:\t000001ffffffffff\n\
         CapAmb:\t0000000000000000\nNoNewPrivs:\t0\nSeccomp:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n",
        s.ppid,
        fds.next_power_of_two().max(64),
        s.pgid,
        s.sid,
        kb(vsize),
        kb(vsize),
        rss * 4,
        rss * 4,
        rss * 4,
        s.threads,
    )
}

/// `/proc/stat`: the Mac's CPUs' time (the host's: a session's processes
/// share its CPUs), boot time, and the processes the kernel has.
fn system_stat(p: &Process) -> String {
    let cpus = hostinfo::cpu_ticks();
    let total = cpus.iter().fold([0u64; 4], |a, c| [a[0] + c[0], a[1] + c[1], a[2] + c[2], a[3] + c[3]]);
    let line = |name: &str, [user, system, idle, nice]: [u64; 4]| format!("{name} {user} {nice} {system} {idle} 0 0 0 0 0 0\n");
    let mut out = line("cpu ", total);
    for (i, c) in cpus.iter().enumerate() {
        out.push_str(&line(&format!("cpu{i}"), *c));
    }
    let pids = p.kernel.pids(p);
    let running = pids.iter().filter(|&&pid| p.kernel.proc_status(p, pid).is_ok_and(|s| s.state == b'R')).count();
    out.push_str(&format!(
        "intr 0\nctxt 0\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
        boot_time(),
        pids.iter().max().copied().unwrap_or(p.pid),
        running.max(1)
    ));
    out
}

/// A read-only file holding `contents`: a shared memory object, so it never
/// reaches the disk.
pub(crate) fn generated(contents: &[u8]) -> Result<FdEntry, i64> {
    let fd = shm::create()?;
    shm::ShmFile::of(fd.as_raw_fd()).ok_or(errno::EIO)?.write_at(0, contents)?;
    Ok(FdEntry::new(fd))
}

/// One line of a mount table.
struct Mount {
    guest: Vec<u8>,
    source: String,
    fstype: String,
    /// Linux mount options: `ro`/`rw` and the like.
    options: String,
    dev: libc::dev_t,
}

/// Linux's names for a host mount's flags.
fn options(flags: u32) -> String {
    let mut out = vec![if flags & libc::MNT_RDONLY as u32 != 0 { "ro" } else { "rw" }];
    for (flag, name) in [(libc::MNT_NOSUID, "nosuid"), (libc::MNT_NODEV, "nodev"), (libc::MNT_NOEXEC, "noexec"), (libc::MNT_NOATIME, "noatime")] {
        if flags & flag as u32 != 0 {
            out.push(name);
        }
    }
    out.join(",")
}

fn c_field(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn mount_of(guest: Vec<u8>, fs: &libc::statfs, host_mountpoint: &CStr) -> Mount {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::stat(host_mountpoint.as_ptr(), &mut st) };
    Mount {
        guest,
        source: c_field(&fs.f_mntfromname),
        fstype: c_field(&fs.f_fstypename),
        options: options(fs.f_flags),
        dev: st.st_dev,
    }
}

/// What's mounted where, as the guest sees it: with the Mac's `/` as root,
/// the Mac's own mounts, less those macOS keeps out of sight (`nobrowse`:
/// its system volumes, `/nix`, `/dev`); with a separate root, the root and each bind, as
/// the host file systems they're on.
fn mount_table(p: &Process) -> Vec<Mount> {
    let mut out = Vec::new();
    if p.fs.is_host_root() {
        let mut list: *mut libc::statfs = std::ptr::null_mut();
        let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
        for fs in (0..n.max(0) as usize).map(|i| unsafe { &*list.add(i) }) {
            if fs.f_flags & libc::MNT_DONTBROWSE as u32 != 0 || matches!(c_field(&fs.f_fstypename).as_str(), "devfs" | "autofs") {
                continue;
            }
            let on = unsafe { CStr::from_ptr(fs.f_mntonname.as_ptr()) };
            out.push(mount_of(on.to_bytes().to_vec(), fs, on));
        }
    } else {
        for (guest, host) in p.fs.mounts() {
            let Ok(host) = std::ffi::CString::new(host) else { continue };
            let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statfs(host.as_ptr(), &mut fs) } == 0 {
                out.push(mount_of(guest, &fs, &host));
            }
        }
    }
    out.push(Mount { guest: b"/proc".to_vec(), source: "proc".into(), fstype: "proc".into(), options: "rw,nosuid,nodev,noexec".into(), dev: 0 });
    out
}

/// A mount table field, with the octal escapes Linux uses for whitespace.
fn escape(field: &[u8]) -> String {
    let mut out = String::new();
    for &b in field {
        match b {
            b' ' | b'\t' | b'\n' | b'\\' => out.push_str(&format!("\\{b:03o}")),
            _ => out.push(b as char),
        }
    }
    out
}

/// `/proc/mounts` (`info`: false) or `/proc/self/mountinfo` (true).
pub(crate) fn mounts(p: &Process, info: bool) -> String {
    let mut out = String::new();
    for (i, m) in mount_table(p).iter().enumerate() {
        let opts = &m.options;
        let (mnt, src) = (escape(&m.guest), escape(m.source.as_bytes()));
        if info {
            let (id, parent) = (20 + i, if i == 0 { 1 } else { 20 });
            let (major, minor) = ((m.dev as u32 >> 24) & 0xff, m.dev as u32 & 0xff_ffff);
            out.push_str(&format!("{id} {parent} {major}:{minor} / {mnt} {opts} - {} {src} {opts}\n", m.fstype));
        } else {
            out.push_str(&format!("{src} {mnt} {} {opts} 0 0\n", m.fstype));
        }
    }
    out
}

/// `/proc/self/maps`, from the core's regions, split where the files mapped
/// in them begin and end, and named.
fn maps<M: GuestMemory + ?Sized>(p: &Process, mem: &M) -> Vec<u8> {
    let files = p.file_maps();
    let mut out = Vec::new();
    for r in mem.regions() {
        let flag = |bit: u64, c: char| if r.prot & bit != 0 { c } else { '-' };
        let mut line = |start: u64, end: u64, offset: u64, path: &[u8]| {
            let perms = [flag(1, 'r'), flag(2, 'w'), flag(4, 'x'), if r.shared { 's' } else { 'p' }];
            let perms: String = perms.iter().collect();
            out.extend_from_slice(format!("{start:08x}-{end:08x} {perms} {offset:08x} 00:00 0").as_bytes());
            if !path.is_empty() {
                out.extend_from_slice(b"                          ");
                out.extend_from_slice(path);
            }
            out.push(b'\n');
        };
        let mut at = r.start;
        for (start, end, path, offset) in files.within(r.start, r.end) {
            if start > at {
                line(at, start, 0, b"");
            }
            line(start, end, offset, path);
            at = end;
        }
        if at < r.end {
            line(at, r.end, 0, b"");
        }
    }
    out
}
