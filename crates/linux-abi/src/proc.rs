//! The emulated `/proc`: just the parts programs commonly rely on —
//! `/proc/self/{exe,cwd,fd/N,maps,mounts,mountinfo}` (and the same under
//! `/proc/<our pid>`), and the machine's `cpuinfo`, `meminfo`, `mounts`,
//! `uptime`, `loadavg` and `version` (from [`crate::hostinfo`]).

use std::{
    ffi::CStr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use crate::{
    errno,
    fs::{FdEntry, Target},
    hostinfo,
    memory::GuestMemory,
    shm,
    syscall::{last_errno, Process},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    /// `/proc`, `/proc/self`, `/proc/self/fd`.
    Dir,
    Exe,
    Cwd,
    Fd(u64),
    Maps,
    /// A file whose contents are made when it's opened.
    Info(Generated),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Generated {
    CpuInfo,
    MemInfo,
    Mounts,
    MountInfo,
    Uptime,
    LoadAvg,
    Version,
}

/// Parse a path below `/proc` (e.g. `self/fd/3`).
fn parse(p: &Process, rest: &[u8]) -> Result<Node, i64> {
    let parts: Vec<&[u8]> = rest.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let Some((&who, below)) = parts.split_first() else { return Ok(Node::Dir) };
    if below.is_empty() {
        let info = match who {
            b"cpuinfo" => Some(Generated::CpuInfo),
            b"meminfo" => Some(Generated::MemInfo),
            b"mounts" => Some(Generated::Mounts),
            b"uptime" => Some(Generated::Uptime),
            b"loadavg" => Some(Generated::LoadAvg),
            b"version" => Some(Generated::Version),
            _ => None,
        };
        if let Some(info) = info {
            return Ok(Node::Info(info));
        }
    }
    let pid = p.pid.to_string();
    if !matches!(who, b"self" | b"thread-self") && who != pid.as_bytes() {
        return Err(errno::ENOENT);
    }
    match below {
        [] | [b"fd"] => Ok(Node::Dir),
        [b"exe"] if !p.exe().is_empty() => Ok(Node::Exe),
        [b"cwd"] => Ok(Node::Cwd),
        [b"maps"] => Ok(Node::Maps),
        [b"mounts"] => Ok(Node::Info(Generated::Mounts)),
        [b"mountinfo"] => Ok(Node::Info(Generated::MountInfo)),
        [b"fd", n] => {
            let n = std::str::from_utf8(n).ok().and_then(|n| n.parse().ok()).ok_or(errno::ENOENT)?;
            p.fds.get(n).map_err(|_| errno::ENOENT)?;
            Ok(Node::Fd(n))
        }
        _ => Err(errno::ENOENT),
    }
}

/// What `/proc/self/fd/N` points to.
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
        libc::S_IFSOCK => format!("socket:[{}]", st.st_ino),
        libc::S_IFIFO => format!("pipe:[{}]", st.st_ino),
        _ => "anon_inode:[unknown]".into(),
    }
    .into_bytes())
}

/// Host path of a guest path, following symlinks.
fn host_of(p: &Process, guest: &[u8]) -> Result<std::ffi::CString, i64> {
    match p.fs.resolve(&p.fds, crate::abi::at::FDCWD as u64, guest, true)?.target {
        Target::Host(h) => Ok(h),
        Target::Proc(_) | Target::Shm(_) => Err(errno::ELOOP),
    }
}

pub(crate) fn readlink(p: &Process, rest: &[u8]) -> Result<Vec<u8>, i64> {
    match parse(p, rest)? {
        Node::Exe => Ok(p.exe()),
        Node::Cwd => Ok(p.fs.cwd()),
        Node::Fd(n) => fd_link(p, n),
        Node::Dir | Node::Maps | Node::Info(_) => Err(errno::EINVAL),
    }
}

pub(crate) fn exists(p: &Process, rest: &[u8]) -> Result<(), i64> {
    parse(p, rest).map(|_| ())
}

/// `stat` (following links) of a `/proc` path.
pub(crate) fn stat(p: &Process, rest: &[u8]) -> Result<libc::stat, i64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = match parse(p, rest)? {
        Node::Dir => {
            st.st_mode = libc::S_IFDIR | 0o555;
            st.st_nlink = 2;
            return Ok(st);
        }
        // Like Linux's, these are empty to stat: their size is known only
        // once read.
        Node::Maps | Node::Info(_) => {
            st.st_mode = libc::S_IFREG | 0o444;
            st.st_nlink = 1;
            return Ok(st);
        }
        Node::Exe => unsafe { libc::stat(host_of(p, &p.exe())?.as_ptr(), &mut st) },
        Node::Cwd => unsafe { libc::stat(host_of(p, &p.fs.cwd())?.as_ptr(), &mut st) },
        Node::Fd(n) => unsafe { libc::fstat(p.fds.host(n)?, &mut st) },
    };
    if r < 0 { Err(last_errno()) } else { Ok(st) }
}

/// Open a `/proc` path; returns the new fd's entry.
pub(crate) fn open<M: GuestMemory + ?Sized>(p: &Process, mem: &M, rest: &[u8]) -> Result<FdEntry, i64> {
    let open_host = |guest: &[u8]| -> Result<FdEntry, i64> {
        let fd = unsafe { libc::open(host_of(p, guest)?.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(last_errno());
        }
        Ok(FdEntry::new(unsafe { OwnedFd::from_raw_fd(fd) }).path(guest.to_vec()))
    };
    match parse(p, rest)? {
        Node::Exe => open_host(&p.exe()),
        Node::Cwd => open_host(&p.fs.cwd()),
        // Linux reopens the file; sharing the open file is close enough.
        Node::Fd(n) => {
            let entry = p.fds.get(n)?;
            let host = entry.host.try_clone().map_err(|e| errno::from_io(&e))?;
            Ok(FdEntry { path: entry.path.clone(), ..FdEntry::new(host) })
        }
        Node::Maps => Ok(generated(&maps(p, mem))?.path(b"/proc/self/maps".to_vec())),
        Node::Info(info) => {
            let contents = match info {
                Generated::CpuInfo => hostinfo::cpuinfo(),
                Generated::MemInfo => hostinfo::meminfo(),
                Generated::Mounts => mounts(p, false),
                Generated::MountInfo => mounts(p, true),
                Generated::Uptime => hostinfo::proc_uptime(),
                Generated::LoadAvg => hostinfo::proc_loadavg(p.pid),
                Generated::Version => hostinfo::proc_version(),
            };
            let mut path = b"/proc/".to_vec();
            path.extend_from_slice(rest);
            Ok(generated(contents.as_bytes())?.path(path))
        }
        Node::Dir => Err(errno::EOPNOTSUPP),
    }
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
