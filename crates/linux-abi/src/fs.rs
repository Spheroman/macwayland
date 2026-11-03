//! File-descriptor table and guest → host path translation.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::CString,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    cell::RefCell,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use crate::{
    abi::at,
    errno,
    wire::{Decoder, Encoder, Truncated},
};

/// An open file — what Linux calls an open file description: the host
/// descriptor and what goes with it. Shared by the guest fds `dup` makes, and
/// kept alive by a call using it even if another thread closes the fd.
#[derive(Debug)]
pub struct OpenFile {
    pub host: OwnedFd,
    /// The resolved guest path it was opened as, if any: the base for `*at()`
    /// calls relative to it, and what `/proc/self/fd/N` points to.
    pub path: Option<Vec<u8>>,
    /// `getdents64` state, for directories.
    pub dir: Mutex<Option<DirSnapshot>>,
    /// The file position, for files whose host side has none of its own
    /// (shared memory, [`crate::shm`]).
    pub pos: AtomicU64,
    /// What it is, if the dispatcher emulates it.
    pub kind: FileKind,
    /// Whether a call on it can block (a pipe, socket, device), found out
    /// the first time it's asked.
    can_block: std::sync::OnceLock<bool>,
    /// Which pty it's an end of, if any, found out the first time it's
    /// asked.
    tty: std::sync::OnceLock<Option<crate::syscall::TtyId>>,
    /// What to do as the last reference to it goes (a pty master's hangup,
    /// with many processes in a host).
    on_close: CloseHook,
}

impl OpenFile {
    /// Whether a read or write on it can block (not a regular file or a
    /// directory).
    pub fn can_block(&self) -> bool {
        *self.can_block.get_or_init(|| crate::syscall::can_block(self.host.as_raw_fd()))
    }

    /// The pty it's an end of, if it is one.
    pub fn tty(&self) -> Option<crate::syscall::TtyId> {
        *self.tty.get_or_init(|| if self.can_block() { crate::syscall::tty_of(self.host.as_raw_fd()) } else { None })
    }

    /// Run `f` as the file closes for good (its last guest fd closed, and
    /// no call holding it), before its host fd closes.
    pub fn on_close(&self, f: Box<dyn FnOnce() + Send>) {
        *self.on_close.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        if let Some(f) = self.on_close.0.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            f();
        }
    }
}

/// A file's [`OpenFile::on_close`].
#[derive(Default)]
struct CloseHook(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl std::fmt::Debug for CloseHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloseHook")
    }
}

/// Open files the dispatcher emulates on top of a host descriptor.
#[derive(Debug, Default)]
pub enum FileKind {
    #[default]
    Plain,
    /// An epoll instance: a host kqueue, and the guest's interest list by
    /// guest fd (see `syscall::epoll`).
    Epoll(Mutex<BTreeMap<i32, Interest>>),
    /// An eventfd (a host kqueue with a user event).
    EventFd(Mutex<crate::syscall::EventFd>),
    /// A timerfd (a host kqueue with a timer).
    TimerFd(Mutex<crate::syscall::TimerFd>),
    /// An inotify instance (a host kqueue with a user event, fed by
    /// FSEvents).
    Inotify(crate::syscall::Inotify),
    /// A signalfd (a host kqueue with a user event, triggered as signals
    /// become pending).
    SignalFd(crate::syscall::SignalFd),
    /// A pidfd for this pid: its host fd reads as ready once the process
    /// has ended (see `syscall::pidfd`).
    PidFd(i64),
}

/// One fd an epoll instance watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
    /// `EPOLL*` bits.
    pub events: u32,
    /// Handed back with each event.
    pub data: u64,
}

/// A new open file and its fd flag, for [`FdTable::insert`].
#[derive(Debug)]
pub struct FdEntry {
    pub host: OwnedFd,
    pub cloexec: bool,
    pub path: Option<Vec<u8>>,
    pub kind: FileKind,
    /// A directory listing it has from the start (`/proc`'s directories).
    pub dir: Option<DirSnapshot>,
}

impl FdEntry {
    pub fn new(host: OwnedFd) -> Self {
        FdEntry { host, cloexec: false, path: None, kind: FileKind::Plain, dir: None }
    }

    pub fn cloexec(mut self, cloexec: bool) -> Self {
        self.cloexec = cloexec;
        self
    }

    pub fn path(mut self, path: Vec<u8>) -> Self {
        self.path = Some(path);
        self
    }

    pub fn path_opt(mut self, path: Option<Vec<u8>>) -> Self {
        self.path = path;
        self
    }

    /// A directory whose listing is `listing`.
    pub fn dir(mut self, listing: DirSnapshot) -> Self {
        self.dir = Some(listing);
        self
    }

    fn open(self) -> Slot {
        let file = OpenFile { host: self.host, path: self.path, dir: Mutex::new(self.dir), pos: AtomicU64::new(0), kind: self.kind, can_block: Default::default(), tty: Default::default(), on_close: Default::default() };
        Slot { file: Arc::new(file), cloexec: self.cloexec }
    }
}

/// A directory's entries, read in full on the first `getdents64` and handed
/// out from `next` on. `lseek(fd, 0, SEEK_SET)` drops it to start over.
#[derive(Debug, Default)]
pub struct DirSnapshot {
    pub entries: Vec<DirEntry>,
    pub next: usize,
    /// A listing made rather than read (`/proc`'s): rewinding starts it over
    /// rather than reading the host directory again.
    pub fixed: bool,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    /// `DT_*`; Darwin and Linux use the same values.
    pub kind: u8,
    pub name: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Slot {
    file: Arc<OpenFile>,
    cloexec: bool,
}

thread_local! {
    /// Open files the current system call is using (see [`FdTable::host`]).
    static IN_FLIGHT: RefCell<Vec<Arc<OpenFile>>> = const { RefCell::new(Vec::new()) };
}

/// The system call on this thread is done with the files it used.
pub fn end_call() {
    IN_FLIGHT.with(|f| f.borrow_mut().clear());
}

/// The current system call uses `file` too (its host fd stays open until
/// the call ends, or while the call waits parked: [`take_in_flight`]).
pub(crate) fn hold(file: &Arc<OpenFile>) {
    IN_FLIGHT.with(|f| f.borrow_mut().push(file.clone()));
}

/// The files the current call uses, for a wait that leaves this host
/// thread (a parked task) to keep open: the host fds it watches mustn't be
/// closed and reused while it waits.
pub(crate) fn take_in_flight() -> Vec<Arc<OpenFile>> {
    IN_FLIGHT.with(|f| std::mem::take(&mut *f.borrow_mut()))
}

/// Guest fd numbers → open files. Guest numbering is independent of the
/// host's, so a guest's fd 3 can be any host fd. Shared by the process's
/// threads.
#[derive(Debug, Default)]
pub struct FdTable {
    slots: Mutex<Vec<Option<Slot>>>,
}

impl FdTable {
    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Option<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn place(slots: &mut Vec<Option<Slot>>, min: usize, slot: Slot) -> i64 {
        let i = (min..).find(|&i| slots.get(i).is_none_or(Option::is_none)).unwrap();
        if i >= slots.len() {
            slots.resize_with(i + 1, || None);
        }
        slots[i] = Some(slot);
        i as i64
    }

    /// Install `entry` at the lowest free guest fd ≥ `min` (POSIX semantics).
    pub fn insert_from(&self, min: usize, entry: FdEntry) -> i64 {
        Self::place(&mut self.slots(), min, entry.open())
    }

    pub fn insert(&self, entry: FdEntry) -> i64 {
        self.insert_from(0, entry)
    }

    /// Install `entry` at exactly guest fd `fd`, closing whatever was there.
    pub fn insert_at(&self, fd: usize, entry: FdEntry) {
        let mut slots = self.slots();
        if fd >= slots.len() {
            slots.resize_with(fd + 1, || None);
        }
        slots[fd] = Some(entry.open());
    }

    /// A new fd for an open file another process has (`pidfd_getfd`).
    pub fn insert_file(&self, file: Arc<OpenFile>, cloexec: bool) -> i64 {
        Self::place(&mut self.slots(), 0, Slot { file, cloexec })
    }

    /// `dup`: a new fd ≥ `min` for `fd`'s open file.
    pub fn dup_from(&self, fd: u64, min: usize, cloexec: bool) -> Result<i64, i64> {
        let mut slots = self.slots();
        let file = slots.get(fd as usize).and_then(Option::as_ref).ok_or(errno::EBADF)?.file.clone();
        Ok(Self::place(&mut slots, min, Slot { file, cloexec }))
    }

    /// `dup3`: `fd`'s open file at exactly `new` (closing what was there).
    pub fn dup_to(&self, fd: u64, new: usize, cloexec: bool) -> Result<(), i64> {
        let mut slots = self.slots();
        let file = slots.get(fd as usize).and_then(Option::as_ref).ok_or(errno::EBADF)?.file.clone();
        if new >= slots.len() {
            slots.resize_with(new + 1, || None);
        }
        slots[new] = Some(Slot { file, cloexec });
        Ok(())
    }

    /// `fd`'s open file.
    pub fn get(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        Ok(self.slots().get(fd as usize).and_then(Option::as_ref).ok_or(errno::EBADF)?.file.clone())
    }

    /// `fd`'s host descriptor. The open file stays open until the current
    /// system call ends ([`end_call`]), whatever other threads do with `fd`.
    pub fn host(&self, fd: u64) -> Result<RawFd, i64> {
        let file = self.get(fd)?;
        let raw = file.host.as_raw_fd();
        IN_FLIGHT.with(|f| f.borrow_mut().push(file));
        Ok(raw)
    }

    /// The open fds' numbers, in order.
    pub fn numbers(&self) -> Vec<u64> {
        self.slots().iter().enumerate().filter(|(_, s)| s.is_some()).map(|(i, _)| i as u64).collect()
    }

    pub fn cloexec(&self, fd: u64) -> Result<bool, i64> {
        Ok(self.slots().get(fd as usize).and_then(Option::as_ref).ok_or(errno::EBADF)?.cloexec)
    }

    pub fn set_cloexec(&self, fd: u64, cloexec: bool) -> Result<(), i64> {
        self.slots().get_mut(fd as usize).and_then(Option::as_mut).ok_or(errno::EBADF)?.cloexec = cloexec;
        Ok(())
    }

    /// `close`.
    pub fn remove(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        Ok(self.slots().get_mut(fd as usize).and_then(Option::take).ok_or(errno::EBADF)?.file)
    }

    /// `close_range`: close the open fds from `first` to `last`, or mark
    /// them close-on-exec.
    pub fn close_range(&self, first: u64, last: u64, cloexec: bool) {
        let mut slots = self.slots();
        let end = slots.len().min(last.saturating_add(1) as usize);
        let range = slots.get_mut(first as usize..end).unwrap_or_default();
        let closed: Vec<Slot> = if cloexec {
            range.iter_mut().flatten().for_each(|s| s.cloexec = true);
            Vec::new()
        } else {
            range.iter_mut().filter_map(Option::take).collect()
        };
        // Closing some files waits (an inotify instance's streams): not
        // while holding the table.
        drop(slots);
        drop(closed);
    }

    /// Close every fd marked close-on-exec.
    /// The fds a new program inherits (those without close-on-exec): guest
    /// number and host fd.
    pub fn inherited(&self) -> Vec<(usize, RawFd)> {
        let slots = self.slots();
        slots.iter().enumerate().filter_map(|(n, s)| s.as_ref().filter(|s| !s.cloexec).map(|s| (n, s.file.host.as_raw_fd()))).collect()
    }

    /// The same, holding each open file (so another thread's `close`
    /// can't free its host fd, or give the number to another file, while
    /// it's being passed on).
    pub fn inherited_files(&self) -> Vec<(usize, Arc<OpenFile>)> {
        let slots = self.slots();
        slots.iter().enumerate().filter_map(|(n, s)| s.as_ref().filter(|s| !s.cloexec).map(|s| (n, s.file.clone()))).collect()
    }

    pub fn close_on_exec(&self) {
        for slot in self.slots().iter_mut() {
            if slot.as_ref().is_some_and(|s| s.cloexec) {
                *slot = None;
            }
        }
    }

    /// Encode the table for a forked child. The host fds aren't in the
    /// message: they're returned, in order, to be passed alongside it.
    /// (Directory read positions aren't kept: the child's `getdents64`
    /// starts a fresh snapshot.)
    pub fn encode(&self, e: &mut Encoder) -> Vec<RawFd> {
        let slots = self.slots();
        let mut host = Vec::new();
        e.u64(slots.len() as u64);
        for slot in slots.iter() {
            e.bool(slot.is_some());
            if let Some(s) = slot {
                e.bool(s.cloexec).opt_bytes(s.file.path.as_deref()).u64(s.file.pos.load(Ordering::Relaxed));
                match &s.file.kind {
                    FileKind::Plain => {
                        e.u64(0);
                        host.push(s.file.host.as_raw_fd());
                    }
                    // A kqueue can't go to another process: the child makes
                    // its own and watches the same fds.
                    FileKind::Epoll(set) => {
                        let set = set.lock().unwrap_or_else(|e| e.into_inner());
                        e.u64(1).u64(set.len() as u64);
                        for (&fd, i) in set.iter() {
                            e.i64(fd as i64).u64(i.events as u64).u64(i.data);
                        }
                        host.push(libc::STDERR_FILENO); // a placeholder
                    }
                    // Likewise kqueue-based eventfds and timerfds: the child
                    // makes its own, in the same state.
                    FileKind::EventFd(state) => {
                        let (count, semaphore) = state.lock().unwrap_or_else(|e| e.into_inner()).state();
                        e.u64(2).u64(count).bool(semaphore).bool(nonblocking(&s.file));
                        host.push(libc::STDERR_FILENO);
                    }
                    FileKind::TimerFd(timer) => {
                        let (clock, interval, left) = timer.lock().unwrap_or_else(|e| e.into_inner()).state();
                        e.u64(3).u64(clock).u64(interval.as_nanos() as u64);
                        e.bool(left.is_some()).u64(left.map_or(0, |l| l.as_nanos() as u64)).bool(nonblocking(&s.file));
                        host.push(libc::STDERR_FILENO);
                    }
                    // And inotify instances: the child watches the same paths
                    // (events not yet read stay with the parent).
                    FileKind::SignalFd(signalfd) => {
                        e.u64(5).u64(signalfd.mask()).bool(nonblocking(&s.file));
                        host.push(libc::STDERR_FILENO);
                    }
                    // And pidfds: the child watches the same process.
                    FileKind::PidFd(pid) => {
                        e.u64(6).i64(*pid).bool(nonblocking(&s.file));
                        host.push(libc::STDERR_FILENO);
                    }
                    FileKind::Inotify(inotify) => {
                        let watches = inotify.state();
                        e.u64(4).u64(watches.len() as u64);
                        for (wd, path, mask) in watches {
                            e.i64(wd as i64).bytes(&path).u64(mask as u64);
                        }
                        e.bool(nonblocking(&s.file));
                        host.push(libc::STDERR_FILENO);
                    }
                }
            }
        }
        host
    }

    /// A forked child's table, in the same host process: the same open files
    /// (one open file description each, shared, as on Linux: offsets,
    /// epoll sets, eventfd counts), except signalfds, which read the
    /// signals of the process reading them, so the child gets its own,
    /// for the process with `inbox`.
    pub fn fork(&self, inbox: &std::sync::Arc<crate::syscall::Inbox>) -> FdTable {
        let slots = self
            .slots()
            .iter()
            .map(|slot| {
                let s = slot.as_ref()?;
                let FileKind::SignalFd(signalfd) = &s.file.kind else { return Some(s.clone()) };
                // Without a copy (no kqueue to be had) the child just doesn't
                // have it.
                let entry = crate::syscall::signalfd_copy(inbox, signalfd.mask(), nonblocking(&s.file)).ok()?.cloexec(s.cloexec);
                Some(entry.open())
            })
            .collect();
        FdTable { slots: Mutex::new(slots) }
    }

    /// The inverse of [`FdTable::encode`], taking host fds from `fds`, for
    /// the process with `inbox` (its signalfds).
    pub fn decode(d: &mut Decoder, fds: &mut impl Iterator<Item = OwnedFd>, inbox: &std::sync::Arc<crate::syscall::Inbox>) -> Result<Self, Truncated> {
        let n = d.count(8)?;
        let mut slots = Vec::with_capacity(n);
        let mut epolls = Vec::new();
        for i in 0..n {
            slots.push(if d.bool()? {
                let (cloexec, path, pos) = (d.bool()?, d.opt_bytes()?, d.u64()?);
                let kind = d.u64()?;
                let host = fds.next().ok_or(Truncated)?;
                let ns = std::time::Duration::from_nanos;
                let entry = if kind == 2 {
                    drop(host);
                    let (count, semaphore, nonblock) = (d.u64()?, d.bool()?, d.bool()?);
                    crate::syscall::eventfd_copy(count, semaphore, nonblock).map_err(|_| Truncated)?.cloexec(cloexec)
                } else if kind == 3 {
                    drop(host);
                    let (clock, interval) = (d.u64()?, ns(d.u64()?));
                    let left = (d.bool()?, ns(d.u64()?));
                    let nonblock = d.bool()?;
                    let left = left.0.then_some(left.1);
                    crate::syscall::timerfd_copy(clock, interval, left, nonblock).map_err(|_| Truncated)?.cloexec(cloexec)
                } else if kind == 5 {
                    drop(host);
                    let (mask, nonblock) = (d.u64()?, d.bool()?);
                    crate::syscall::signalfd_copy(inbox, mask, nonblock).map_err(|_| Truncated)?.cloexec(cloexec)
                } else if kind == 6 {
                    drop(host);
                    let (pid, nonblock) = (d.i64()?, d.bool()?);
                    let host = crate::syscall::pidfd::host_exit_watch(pid).map_err(|_| Truncated)?;
                    crate::syscall::pidfd::entry(pid, host, nonblock)
                } else if kind == 4 {
                    drop(host);
                    let watches = (0..d.count(24)?)
                        .map(|_| Ok((d.i64()? as i32, d.bytes()?, d.u64()? as u32)))
                        .collect::<Result<Vec<_>, Truncated>>()?;
                    let nonblock = d.bool()?;
                    crate::syscall::inotify_copy(watches, nonblock).map_err(|_| Truncated)?.cloexec(cloexec)
                } else if kind == 1 {
                    let watched = (0..d.count(24)?)
                        .map(|_| Ok((d.i64()? as i32, Interest { events: d.u64()? as u32, data: d.u64()? })))
                        .collect::<Result<Vec<_>, Truncated>>()?;
                    epolls.push((i, watched));
                    drop(host);
                    crate::syscall::new_epoll().map_err(|_| Truncated)?.cloexec(cloexec)
                } else {
                    FdEntry { host, cloexec, path, kind: FileKind::Plain, dir: None }
                };
                let slot = entry.open();
                slot.file.pos.store(pos, Ordering::Relaxed);
                Some(slot)
            } else {
                None
            });
        }
        let table = FdTable { slots: Mutex::new(slots) };
        for (i, watched) in epolls {
            for (fd, interest) in watched {
                crate::syscall::rewatch(&table, i as u64, fd, interest);
            }
        }
        Ok(table)
    }
}

/// Lexically normalise an absolute guest path: collapse `//`, `.` and `..`
/// (`..` at `/` stays at `/`). Always returns a path starting with `/`.
pub fn normalize(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = Vec::with_capacity(path.len() + 1);
    for p in &parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// Most guest symlinks a lookup may follow, as on Linux.
const MAX_LINKS: u32 = 40;

/// What a guest path refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A host file (that may not exist yet).
    Host(CString),
    /// Something under `/proc`, emulated ([`crate::proc`]); the path below
    /// `/proc`, e.g. `self/fd/3`.
    Proc(Vec<u8>),
    /// `/dev/shm/<name>`: a shared memory object ([`crate::shm`]), by name.
    Shm(Vec<u8>),
}

/// A resolved guest path: the guest's view (absolute, symlinks followed as
/// requested) and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub guest: Vec<u8>,
    pub target: Target,
}

/// Where guest paths land on the host.
///
/// With a separate guest root, paths are resolved here, component by
/// component, so symlinks behave as they would on Linux with `root` as `/` (an
/// absolute link target stays inside the root). When the root is the host's
/// `/`, macOS resolves paths itself with the same semantics, so they're passed
/// through untouched — the per-component walk costs ~15x a native `stat`.
/// Either way, some paths are special: `/proc` is emulated, `/dev`
/// entries are the host's devices, and what's in `/dev/shm` (where
/// `shm_open` puts shared memory) is shared memory objects.
///
/// This is a compatibility mapping, not a sandbox: the guest runs with the
/// user's own permissions and `..` or `*at()` tricks can reach host files.
#[derive(Debug)]
pub struct Filesystem {
    root: PathBuf,
    /// Guest working directory, absolute and resolved (shared by threads).
    cwd: RwLock<Vec<u8>>,
    /// Host directory for runtime-private files (`/dev/shm`, abstract sockets).
    private: PathBuf,
    /// Bind mounts: (guest path, host path), both absolute and without a
    /// trailing slash, longest guest path first.
    binds: Vec<(Vec<u8>, Vec<u8>)>,
}

fn components(path: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

fn join(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// The part of `path` below `dir` ("" for `dir` itself, else starting with
/// `/`), if `path` is `dir` or inside it.
fn below<'a>(path: &'a [u8], dir: &[u8]) -> Option<&'a [u8]> {
    let rest = path.strip_prefix(dir)?;
    (rest.is_empty() || rest.starts_with(b"/") || dir == b"/").then_some(rest)
}

fn nonblocking(file: &OpenFile) -> bool {
    let flags = unsafe { libc::fcntl(file.host.as_raw_fd(), libc::F_GETFL) };
    flags & libc::O_NONBLOCK != 0
}

fn cstring(bytes: Vec<u8>) -> Result<CString, i64> {
    CString::new(bytes).map_err(|_| errno::EINVAL)
}

fn last_errno() -> i64 {
    errno::from_io(&std::io::Error::last_os_error())
}

/// `lstat` a host path.
pub fn host_lstat(path: &CString) -> Result<libc::stat, i64> {
    let mut st = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(path.as_ptr(), &mut st) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

/// `readlink` a host path.
pub fn host_readlink(path: &CString) -> Result<Vec<u8>, i64> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let n = unsafe { libc::readlink(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(last_errno());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

impl Filesystem {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let uid = unsafe { libc::getuid() };
        Filesystem {
            root: root.into(),
            cwd: RwLock::new(b"/".to_vec()),
            private: std::env::temp_dir().join(format!("macwl-{uid}")),
            binds: Vec::new(),
        }
    }

    /// Make the host directory `host` appear at `guest` (absolute), like a
    /// bind mount: paths below it go to the host, and a symlink in there with
    /// an absolute target is resolved from the guest's `/`. As on Linux, the
    /// mount point must exist in the root; it's created if it doesn't.
    pub fn bind(&mut self, guest: &[u8], host: impl Into<PathBuf>) -> std::io::Result<()> {
        let guest = normalize(guest);
        if self.is_host_root() || guest == b"/" {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "binds need a separate root, below its /"));
        }
        let host = host.into();
        let mount_point = self.host_path(&guest).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        std::fs::create_dir_all(std::ffi::OsStr::from_bytes(mount_point.as_bytes()))?;
        let host = host.as_os_str().as_encoded_bytes();
        let host = if host.len() > 1 { host.strip_suffix(b"/").unwrap_or(host) } else { host };
        self.binds.retain(|(g, _)| *g != guest);
        self.binds.push((guest, host.to_vec()));
        self.binds.sort_by_key(|(g, _)| std::cmp::Reverse(g.len()));
        Ok(())
    }

    pub fn encode(&self, e: &mut Encoder) {
        e.bytes(self.root.as_os_str().as_bytes()).bytes(&self.cwd()).bytes(self.private.as_os_str().as_bytes());
        e.u64(self.binds.len() as u64);
        for (guest, host) in &self.binds {
            e.bytes(guest).bytes(host);
        }
    }

    pub fn decode(d: &mut Decoder) -> Result<Self, Truncated> {
        let path = |b: Vec<u8>| PathBuf::from(std::ffi::OsString::from_vec(b));
        let (root, cwd, private) = (path(d.bytes()?), d.bytes()?, path(d.bytes()?));
        let n = d.count(16)?;
        let binds = (0..n).map(|_| Ok((d.bytes()?, d.bytes()?))).collect::<Result<_, Truncated>>()?;
        Ok(Filesystem { root, cwd: RwLock::new(cwd), private, binds })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the guest's `/` and each bind come from: (guest, host) pairs.
    pub fn mounts(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = vec![(b"/".to_vec(), self.root.as_os_str().as_encoded_bytes().to_vec())];
        out.extend(self.binds.iter().rev().cloned());
        out
    }

    /// Whether the guest sees the host's own `/`.
    pub fn is_host_root(&self) -> bool {
        self.root == Path::new("/")
    }

    pub fn cwd(&self) -> Vec<u8> {
        self.cwd.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_cwd(&self, guest_abs: Vec<u8>) {
        *self.cwd.write().unwrap_or_else(|e| e.into_inner()) = guest_abs;
    }

    /// Absolute, normalised guest path for `path` relative to the guest cwd
    /// (lexically: `..` doesn't account for symlinks).
    pub fn absolute(&self, path: &[u8]) -> Vec<u8> {
        if path.starts_with(b"/") {
            normalize(path)
        } else {
            let mut joined = self.cwd();
            joined.push(b'/');
            joined.extend_from_slice(path);
            normalize(&joined)
        }
    }

    /// Host path for an absolute guest path, taken literally.
    pub fn host_path(&self, guest_abs: &[u8]) -> Result<CString, i64> {
        for (guest, host) in &self.binds {
            if let Some(rest) = below(guest_abs, guest) {
                let mut out = host.clone();
                out.extend_from_slice(rest);
                return cstring(out);
            }
        }
        let mut host = self.root.as_os_str().as_encoded_bytes().to_vec();
        if host.last() == Some(&b'/') {
            host.pop();
        }
        host.extend_from_slice(guest_abs);
        cstring(host)
    }

    /// The guest's name for a host path (for socket addresses the host hands
    /// back): inside the root, the path below it; otherwise the host path.
    pub fn guest_path_of_host(&self, host: &[u8]) -> Vec<u8> {
        let root = self.root.as_os_str().as_encoded_bytes();
        let root = if root.len() > 1 { root.strip_suffix(b"/").unwrap_or(root) } else { root };
        // The most specific match wins: a root kept inside a bound directory
        // (say /tmp) is still the guest's `/`.
        let best = self
            .binds
            .iter()
            .map(|(g, h)| (g.as_slice(), h.as_slice()))
            .chain([(&b""[..], root)])
            .filter_map(|(g, h)| below(host, h).map(|rest| (h.len(), g, rest)))
            .max_by_key(|&(len, ..)| len);
        match best {
            Some((_, b"", _)) if root == b"/" => host.to_vec(),
            Some((_, b"", b"")) => b"/".to_vec(),
            Some((_, guest, rest)) => [guest, rest].concat(),
            None => host.to_vec(),
        }
    }

    /// A host directory under the runtime's private directory, created on
    /// first use.
    pub fn private_dir(&self, name: &str) -> Result<PathBuf, i64> {
        let dir = self.private.join(name);
        if !dir.is_dir() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| errno::from_io(&e))?;
        }
        Ok(dir)
    }

    /// Resolve a `*at()`-style (dirfd, path) pair. `follow` says whether a
    /// symlink in the last component is followed (as for `stat` but not
    /// `lstat`). Intermediate symlinks are always followed.
    pub fn resolve(&self, fds: &FdTable, dirfd: u64, path: &[u8], follow: bool) -> Result<Resolved, i64> {
        if path.is_empty() {
            return Err(errno::ENOENT);
        }
        let base = if path.starts_with(b"/") {
            Vec::new()
        } else if dirfd as i64 == at::FDCWD {
            self.cwd()
        } else {
            fds.get(dirfd)?.path.clone().ok_or(errno::ENOTDIR)?
        };
        if self.is_host_root() {
            // The host resolves the path as Linux would. `guest` is only
            // lexical here (`..` after a symlink may differ); callers that
            // keep it refine it from the host (see `openat`, `chdir`).
            let mut joined = base.clone();
            if !path.starts_with(b"/") {
                joined.push(b'/');
            }
            joined.extend_from_slice(path);
            let guest = normalize(&joined);
            let special = |prefix: &[u8]| guest == prefix || guest.strip_prefix(prefix).is_some_and(|r| r.starts_with(b"/"));
            if !special(b"/proc") && !special(b"/dev") {
                return Ok(Resolved { target: Target::Host(cstring(joined)?), guest });
            }
        }
        let mut todo: VecDeque<Vec<u8>> = components(&base).chain(components(path)).map(<[u8]>::to_vec).collect();
        let mut done: Vec<Vec<u8>> = Vec::new();
        let mut links = 0;
        while let Some(c) = todo.pop_front() {
            match c.as_slice() {
                b"." => continue,
                b".." => {
                    done.pop();
                    continue;
                }
                _ => done.push(c),
            }
            if let Some(special) = self.special(&done, &todo)? {
                return Ok(special);
            }
            // `/dev` is the host's whatever the root holds; the next
            // component decides (see `special`).
            if matches!(done.as_slice(), [d] if d == b"dev") {
                continue;
            }
            let last = todo.is_empty();
            if last && !follow {
                break;
            }
            let host = self.host_path(&join(&done))?;
            let st = match host_lstat(&host) {
                Ok(st) => st,
                // The last component may be about to be created.
                Err(errno::ENOENT) if last => break,
                Err(e) => return Err(e),
            };
            match st.st_mode & libc::S_IFMT {
                libc::S_IFLNK => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(errno::ELOOP);
                    }
                    let target = host_readlink(&host)?;
                    done.pop();
                    if target.starts_with(b"/") {
                        done.clear();
                    }
                    for comp in components(&target).rev() {
                        todo.push_front(comp.to_vec());
                    }
                }
                libc::S_IFDIR => {}
                _ if !last => return Err(errno::ENOTDIR),
                _ => {}
            }
        }
        let guest = join(&done);
        Ok(Resolved { target: Target::Host(self.host_path(&guest)?), guest })
    }

    /// `/proc` and `/dev` handling, checked as each component is resolved.
    fn special(&self, done: &[Vec<u8>], todo: &VecDeque<Vec<u8>>) -> Result<Option<Resolved>, i64> {
        // The rest of the path, taken lexically (nothing below is a guest symlink).
        let rest = || {
            let mut parts: Vec<Vec<u8>> = Vec::new();
            for c in todo {
                match c.as_slice() {
                    b"." => {}
                    b".." => {
                        parts.pop();
                    }
                    _ => parts.push(c.clone()),
                }
            }
            parts
        };
        let resolved = |guest: Vec<Vec<u8>>, target| Ok(Some(Resolved { guest: join(&guest), target }));
        match done {
            [d] if d == b"dev" && todo.is_empty() => resolved(done.to_vec(), Target::Host(cstring(b"/dev".to_vec())?)),
            [p] if p == b"proc" => {
                let below = rest();
                let mut guest = vec![b"proc".to_vec()];
                guest.extend(below.iter().cloned());
                resolved(guest, Target::Proc(join(&below)[1..].to_vec()))
            }
            [d, name] if d == b"dev" => {
                let below = rest();
                let fd_link = |n: &[u8]| {
                    let mut p = b"self/fd/".to_vec();
                    p.extend_from_slice(n);
                    Target::Proc(p)
                };
                let target = match name.as_slice() {
                    b"stdin" => fd_link(b"0"),
                    b"stdout" => fd_link(b"1"),
                    b"stderr" => fd_link(b"2"),
                    b"fd" if !below.is_empty() => fd_link(&below[0]),
                    // Linux's pseudo-terminal slaves are macOS's /dev/ttysNNN.
                    b"pts" => match below.as_slice() {
                        [] => Target::Host(cstring(b"/dev".to_vec())?),
                        [n] => {
                            let n: u32 = std::str::from_utf8(n).ok().and_then(|n| n.parse().ok()).ok_or(errno::ENOENT)?;
                            Target::Host(cstring(format!("/dev/ttys{n:03}").into_bytes())?)
                        }
                        _ => return Err(errno::ENOTDIR),
                    },
                    // The directory itself is an empty private one; what's in
                    // it lives in RAM, not in files.
                    b"shm" => match below.as_slice() {
                        [] => Target::Host(cstring(self.private_dir("shm")?.into_os_string().into_encoded_bytes())?),
                        [name] => Target::Shm(name.clone()),
                        _ => return Err(errno::ENOTDIR),
                    },
                    _ => {
                        let mut host = b"/dev/".to_vec();
                        host.extend_from_slice(name);
                        if !below.is_empty() {
                            host.extend(join(&below));
                        }
                        Target::Host(cstring(host)?)
                    }
                };
                let mut guest = done.to_vec();
                guest.extend(below);
                resolved(guest, target)
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize(b"/"), b"/");
        assert_eq!(normalize(b"/a//b/./c/"), b"/a/b/c");
        assert_eq!(normalize(b"/a/b/../c"), b"/a/c");
        assert_eq!(normalize(b"/../../etc"), b"/etc");
    }

    #[test]
    fn absolute_uses_cwd() {
        let fs = Filesystem::new("/tmp/root");
        fs.set_cwd(b"/home/user".to_vec());
        assert_eq!(fs.absolute(b"file"), b"/home/user/file");
        assert_eq!(fs.absolute(b"../x"), b"/home/x");
        assert_eq!(fs.absolute(b"/etc/passwd"), b"/etc/passwd");
        assert_eq!(fs.host_path(b"/etc/passwd").unwrap().as_bytes(), b"/tmp/root/etc/passwd");
    }

    fn host(r: &Resolved) -> &[u8] {
        match &r.target {
            Target::Host(h) => h.as_bytes(),
            t => panic!("not a host path: {t:?}"),
        }
    }

    #[test]
    fn binds_map_guest_paths_onto_host_directories() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("home"));
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/os-release"), b"guest").unwrap();
        std::fs::create_dir_all(outside.join("proj")).unwrap();
        std::fs::write(outside.join("proj/notes"), b"host").unwrap();
        symlink("/etc/os-release", outside.join("proj/link")).unwrap(); // the guest's /etc
        let mut fs = Filesystem::new(&root);
        fs.bind(b"/home/me/", &outside).unwrap();
        assert!(root.join("home/me").is_dir(), "the mount point is created");
        let fds = FdTable::default();

        let r = fs.resolve(&fds, at::FDCWD as u64, b"/home/me/proj/notes", true).unwrap();
        assert_eq!((r.guest.as_slice(), host(&r)), (&b"/home/me/proj/notes"[..], outside.join("proj/notes").as_os_str().as_bytes()));
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/home/me/proj/link", true).unwrap();
        assert_eq!(host(&r), root.join("etc/os-release").as_os_str().as_bytes());
        // `..` out of the bind lands back in the root.
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/home/me/../x", true).unwrap();
        assert_eq!(host(&r), root.join("home/x").as_os_str().as_bytes());
        // A host path inside the bind maps back to the guest's name for it.
        let mut h = outside.as_os_str().as_bytes().to_vec();
        h.extend_from_slice(b"/proj/sock");
        assert_eq!(fs.guest_path_of_host(&h), b"/home/me/proj/sock");
        // Prefixes only match whole components.
        assert_eq!(fs.host_path(b"/home/meow").unwrap().as_bytes(), root.join("home/meow").as_os_str().as_bytes());

        // A root inside a bound directory is still `/`.
        let mut fs = Filesystem::new(&root);
        fs.bind(b"/tmp", dir.path()).unwrap();
        let mut h = root.as_os_str().as_bytes().to_vec();
        h.extend_from_slice(b"/etc/os-release");
        assert_eq!(fs.guest_path_of_host(&h), b"/etc/os-release");
        assert_eq!(fs.guest_path_of_host(root.as_os_str().as_bytes()), b"/");

        assert!(Filesystem::new("/").bind(b"/x", &outside).is_err(), "the host root needs no binds");
    }

    #[test]
    fn symlinks_resolve_inside_the_root() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::write(root.join("usr/lib/libc.so"), b"").unwrap();
        symlink("/usr/lib", root.join("lib")).unwrap(); // absolute: means <root>/usr/lib
        symlink("../usr/lib/libc.so", root.join("usr/libc-rel")).unwrap();
        symlink("loop", root.join("loop")).unwrap();
        let fs = Filesystem::new(root);
        let fds = FdTable::default();

        let r = fs.resolve(&fds, at::FDCWD as u64, b"/lib/libc.so", true).unwrap();
        assert_eq!(r.guest, b"/usr/lib/libc.so");
        assert_eq!(host(&r), root.join("usr/lib/libc.so").as_os_str().as_encoded_bytes());

        let r = fs.resolve(&fds, at::FDCWD as u64, b"/usr/libc-rel", true).unwrap();
        assert_eq!(r.guest, b"/usr/lib/libc.so");
        // Not following the last component: the link itself.
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/usr/libc-rel", false).unwrap();
        assert_eq!(r.guest, b"/usr/libc-rel");
        // `..` after a symlink is relative to where the link points.
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/lib/../lib/libc.so", true).unwrap();
        assert_eq!(r.guest, b"/usr/lib/libc.so");

        assert_eq!(fs.resolve(&fds, at::FDCWD as u64, b"/loop", true), Err(errno::ELOOP));
        assert_eq!(fs.resolve(&fds, at::FDCWD as u64, b"/nope/x", true), Err(errno::ENOENT));
        assert_eq!(fs.resolve(&fds, at::FDCWD as u64, b"/usr/lib/libc.so/x", true), Err(errno::ENOTDIR));
        // A missing last component is fine (it may be created).
        assert_eq!(fs.resolve(&fds, at::FDCWD as u64, b"/usr/new", true).unwrap().guest, b"/usr/new");
    }

    #[test]
    fn dirfd_relative() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        let fs = Filesystem::new(dir.path());
        let fds = FdTable::default();
        let etc = std::fs::File::open(dir.path().join("etc")).unwrap();
        let n = fds.insert(FdEntry::new(etc.into()).path(b"/etc".to_vec())) as u64;
        assert_eq!(fs.resolve(&fds, n, b"hosts", true).unwrap().guest, b"/etc/hosts");
        let file = std::fs::File::open("/dev/null").unwrap();
        let m = fds.insert(FdEntry::new(file.into())) as u64;
        assert_eq!(fs.resolve(&fds, m, b"x", true), Err(errno::ENOTDIR));
    }

    #[test]
    fn proc_and_dev() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Filesystem::new(dir.path());
        let fds = FdTable::default();
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/proc/self/fd/3", true).unwrap();
        assert_eq!(r.target, Target::Proc(b"self/fd/3".to_vec()));
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/dev/null", true).unwrap();
        assert_eq!(host(&r), b"/dev/null");
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/dev/stderr", true).unwrap();
        assert_eq!(r.target, Target::Proc(b"self/fd/2".to_vec()));
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/dev/shm/wl-buf", true).unwrap();
        assert_eq!(r.target, Target::Shm(b"wl-buf".to_vec()));
        assert_eq!(r.guest, b"/dev/shm/wl-buf");
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/dev/shm", true).unwrap();
        assert!(host(&r).ends_with(b"/shm"), "the directory itself: {r:?}");
        assert_eq!(fs.resolve(&fds, at::FDCWD as u64, b"/dev/shm/a/b", true), Err(errno::ENOTDIR));
    }

    #[test]
    fn host_root_passes_paths_through() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(base.join("real/sub")).unwrap();
        symlink(base.join("real/sub"), base.join("link")).unwrap();
        let fs = Filesystem::new("/");
        let fds = FdTable::default();
        let path = [base.as_os_str().as_encoded_bytes(), b"/link/../x"].concat();
        let r = fs.resolve(&fds, at::FDCWD as u64, &path, true).unwrap();
        // Untouched for the host, which follows `link` before `..` (giving
        // real/x, as Linux would)...
        assert_eq!(host(&r), &path[..]);
        // ...while the recorded guest path is lexical.
        assert_eq!(r.guest, [base.as_os_str().as_encoded_bytes(), b"/x"].concat());
        // /proc and /dev still go through the special handling.
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/proc/self/exe", true).unwrap();
        assert_eq!(r.target, Target::Proc(b"self/exe".to_vec()));
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/dev/stdout", true).unwrap();
        assert_eq!(r.target, Target::Proc(b"self/fd/1".to_vec()));
        let r = fs.resolve(&fds, at::FDCWD as u64, b"/devices", true).unwrap();
        assert_eq!(host(&r), b"/devices", "only /dev itself is special");
    }

    #[test]
    fn fd_table_lowest_free() {
        let t = FdTable::default();
        let devnull = || FdEntry::new(std::fs::File::open("/dev/null").unwrap().into());
        assert_eq!(t.insert(devnull()), 0);
        assert_eq!(t.insert(devnull()), 1);
        assert_eq!(t.insert(devnull()), 2);
        t.remove(1).unwrap();
        assert_eq!(t.insert(devnull()), 1);
        assert_eq!(t.insert_from(10, devnull()), 10);
        assert_eq!(t.get(5).unwrap_err(), errno::EBADF);
    }
}
