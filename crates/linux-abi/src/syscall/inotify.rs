//! `inotify`: Linux's file change notifications, from macOS's FSEvents.
//!
//! kqueue's vnode events say only that a directory changed, not which entry,
//! and nothing of changes to the files in it, which is what GLib's file
//! monitors watch for (they watch a file's directory and pick out its name).
//! FSEvents reports each file by path. Each watch is an FSEvent stream on its
//! directory (a file's parent, for a file), whose events about the watched
//! entries are turned into `inotify_event`s. The instance is a host kqueue
//! with a user event, triggered while events are queued, so it can be polled
//! and watched by epoll, as `eventfd`s are.
//!
//! FSEvents reports what has happened to a path since it last said, not each
//! step, so a directory watch keeps its entries (name and inode) and works
//! out what changed: an entry that has appeared was created (or moved in),
//! one that's gone deleted (or moved out). Not reported, as FSEvents has no
//! such events: `IN_ACCESS`, `IN_OPEN`, `IN_CLOSE_NOWRITE`. macOS reports a
//! file's writes when it's closed, so `IN_MODIFY` comes then, once, with
//! `IN_CLOSE_WRITE`, rather than at each write.

use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_char, c_void},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    sync::{Arc, Mutex, MutexGuard},
};

use super::{kfd, ok, path_arg, Process, Thread};
use crate::{
    abi::at,
    errno,
    fs::{FdEntry, FileKind, OpenFile, Target},
    memory::GuestMemory,
};

const IN_MODIFY: u32 = 0x2;
const IN_ATTRIB: u32 = 0x4;
const IN_CLOSE_WRITE: u32 = 0x8;
const IN_MOVED_FROM: u32 = 0x40;
const IN_MOVED_TO: u32 = 0x80;
const IN_CREATE: u32 = 0x100;
const IN_DELETE: u32 = 0x200;
const IN_DELETE_SELF: u32 = 0x400;
const IN_MOVE_SELF: u32 = 0x800;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// Always reported, whatever the mask.
const IN_ALWAYS: u32 = IN_IGNORED | IN_Q_OVERFLOW | 0x2000 /* IN_UNMOUNT */;

const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;

/// Linux's default `max_queued_events`.
const MAX_QUEUED: usize = 16384;
/// `sizeof(struct inotify_event)`: names are padded to a multiple of it.
const EVENT_SIZE: usize = 16;

/// One `inotify_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl Event {
    /// The name's length as Linux pads it (0 without one).
    fn name_len(&self) -> usize {
        if self.name.is_empty() { 0 } else { (self.name.len() + 1).next_multiple_of(EVENT_SIZE) }
    }

    fn size(&self) -> usize {
        EVENT_SIZE + self.name_len()
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.wd.to_le_bytes());
        out.extend_from_slice(&self.mask.to_le_bytes());
        out.extend_from_slice(&self.cookie.to_le_bytes());
        out.extend_from_slice(&(self.name_len() as u32).to_le_bytes());
        let start = out.len();
        out.extend_from_slice(&self.name);
        out.resize(start + self.name_len(), 0);
    }
}

/// A watch on a file or directory.
struct Watch {
    /// Its host path, as FSEvents reports it (no symlinks).
    path: Vec<u8>,
    mask: u32,
    ino: u64,
    /// For a directory, its entries: name to inode (and whether it's a
    /// directory), as last seen.
    entries: Option<HashMap<Vec<u8>, (u64, bool)>>,
    stream: Option<Stream>,
}

/// An inotify instance's state.
#[derive(Default)]
struct State {
    watches: HashMap<i32, Watch>,
    next_wd: i32,
    queue: VecDeque<Event>,
    next_cookie: u32,
}

/// What an instance's FSEvents callbacks share with the guest's calls.
struct Shared {
    state: Mutex<State>,
    /// The instance's kqueue, its own copy: callbacks may still run while
    /// the guest's fd is being closed.
    kq: OwnedFd,
    /// The serial queue its streams call back on.
    queue: DispatchQueue,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue an event (merging it into the last if they're the same, as
    /// Linux does), and make the instance readable.
    fn push(&self, state: &mut State, event: Event) {
        if state.queue.back() == Some(&event) {
            return;
        }
        if state.queue.len() >= MAX_QUEUED {
            let overflow = Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() };
            if state.queue.back() != Some(&overflow) {
                state.queue.push_back(overflow);
            }
        } else {
            state.queue.push_back(event);
        }
        let _ = kfd::change(self.kq.as_raw_fd(), kfd::user_event(0, libc::NOTE_TRIGGER));
    }
}

/// An inotify instance.
pub struct Inotify(Arc<Shared>);

impl std::fmt::Debug for Inotify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inotify").field("watches", &self.0.lock().watches.len()).finish()
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        // Outside the lock: a callback may be waiting for it.
        let streams: Vec<Stream> = self.0.lock().watches.values_mut().filter_map(|w| w.stream.take()).collect();
        for stream in streams {
            stream.stop(&self.0.queue);
        }
    }
}


impl Inotify {
    /// For a process moving to another host: each watch's (wd, host path,
    /// mask). (Events not yet read stay behind.)
    pub fn state(&self) -> Vec<(i32, Vec<u8>, u32)> {
        self.0.lock().watches.iter().map(|(&wd, w)| (wd, w.path.clone(), w.mask)).collect()
    }
}

/// A new inotify instance watching what `Inotify::state` gave, with the
/// same watch descriptors.
pub(crate) fn inotify_copy(watches: Vec<(i32, Vec<u8>, u32)>, nonblock: bool) -> Result<FdEntry, i64> {
    let (entry, shared) = new_instance(nonblock)?;
    for (wd, path, mask) in watches {
        let Ok(path) = CString::new(path) else { continue };
        if let Ok(watch) = new_watch(&shared, wd, &path, mask) {
            let mut state = shared.lock();
            state.next_wd = state.next_wd.max(wd + 1);
            state.watches.insert(wd, watch);
        }
    }
    Ok(entry)
}

fn new_instance(nonblock: bool) -> Result<(FdEntry, Arc<Shared>), i64> {
    let kq = kfd::new_kqueue(nonblock)?;
    kfd::change(kq.as_raw_fd(), kfd::user_event(libc::EV_ADD | libc::EV_CLEAR, 0))?;
    let own = ok(unsafe { libc::dup(kq.as_raw_fd()) } as i64)? as RawFd;
    let shared = Arc::new(Shared {
        state: Mutex::new(State { next_wd: 1, next_cookie: 1, ..Default::default() }),
        kq: unsafe { OwnedFd::from_raw_fd(own) },
        queue: DispatchQueue::new(),
    });
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::Inotify(Inotify(shared.clone()));
    Ok((entry, shared))
}

pub(super) fn inotify_init1(p: &Process, flags: u64) -> Result<i64, i64> {
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let (entry, _) = new_instance(flags & O_NONBLOCK != 0)?;
    Ok(p.fds.insert(entry.cloexec(flags & O_CLOEXEC != 0)))
}

fn instance(file: &OpenFile) -> Result<&Arc<Shared>, i64> {
    match &file.kind {
        FileKind::Inotify(i) => Ok(&i.0),
        _ => Err(errno::EINVAL),
    }
}

/// (inode, is a directory) of a host path, following symlinks or not.
fn stat(path: &CStr, follow: bool) -> Result<(u64, bool), i64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = if follow { unsafe { libc::stat(path.as_ptr(), &mut st) } } else { unsafe { libc::lstat(path.as_ptr(), &mut st) } };
    ok(r as i64)?;
    Ok((st.st_ino, st.st_mode & libc::S_IFMT == libc::S_IFDIR))
}

/// A directory's entries: name to (inode, is a directory).
fn entries(dir: &[u8]) -> HashMap<Vec<u8>, (u64, bool)> {
    let Ok(read) = std::fs::read_dir(std::ffi::OsStr::from_bytes(dir)) else { return HashMap::new() };
    read.flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.file_name().as_bytes().to_vec(), (std::os::unix::fs::MetadataExt::ino(&meta), meta.is_dir())))
        })
        .collect()
}

/// Watch `path` (a host path) as watch `wd`.
fn new_watch(shared: &Arc<Shared>, wd: i32, path: &CStr, mask: u32) -> Result<Watch, i64> {
    // FSEvents reports real paths: /private/tmp for /tmp.
    let real = std::fs::canonicalize(std::ffi::OsStr::from_bytes(path.to_bytes())).map_err(|e| e.raw_os_error().map_or(errno::ENOENT, |e| -(e as i64)))?;
    let real = real.as_os_str().as_bytes().to_vec();
    let real_c = CString::new(real.clone()).map_err(|_| errno::ENOENT)?;
    let (ino, is_dir) = stat(&real_c, true)?;
    let dir = if is_dir { real.clone() } else { parent(&real).to_vec() };
    let stream = Stream::start(&dir, Arc::new(Context { shared: Arc::downgrade(shared), wd }), &shared.queue)?;
    Ok(Watch { path: real.clone(), mask, ino, entries: is_dir.then(|| entries(&real)), stream: Some(stream) })
}

fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

pub(super) fn inotify_add_watch<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, path: u64, mask: u64) -> Result<i64, i64> {
    let file = p.fds.get(fd)?;
    let shared = instance(&file)?.clone();
    let mask = mask as u32;
    if mask & IN_ALL_EVENTS == 0 || (mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0) {
        return Err(errno::EINVAL);
    }
    let guest = path_arg(mem, path)?;
    let resolved = p.fs.resolve(&p.fds, at::FDCWD as u64, &guest, mask & IN_DONT_FOLLOW == 0)?;
    let Target::Host(host) = resolved.target else { return Err(errno::EACCES) };
    let (ino, is_dir) = stat(&host, mask & IN_DONT_FOLLOW == 0)?;
    if mask & IN_ONLYDIR != 0 && !is_dir {
        return Err(errno::ENOTDIR);
    }
    let wanted = mask & !(IN_MASK_ADD | IN_MASK_CREATE | IN_ONLYDIR | IN_DONT_FOLLOW);
    let wd = {
        let mut state = shared.lock();
        // A second watch on the same file is the first one, its mask changed.
        if let Some((&wd, watch)) = state.watches.iter_mut().find(|(_, w)| w.ino == ino) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(errno::EEXIST);
            }
            watch.mask = if mask & IN_MASK_ADD != 0 { watch.mask | wanted } else { wanted };
            return Ok(wd as i64);
        }
        state.next_wd += 1;
        state.next_wd - 1
    };
    let watch = new_watch(&shared, wd, &host, wanted)?;
    shared.lock().watches.insert(wd, watch);
    Ok(wd as i64)
}

pub(super) fn inotify_rm_watch(p: &Process, fd: u64, wd: u64) -> Result<i64, i64> {
    let file = p.fds.get(fd)?;
    let shared = instance(&file)?;
    let stream = {
        let mut state = shared.lock();
        let mut watch = state.watches.remove(&(wd as i32)).ok_or(errno::EINVAL)?;
        shared.push(&mut state, Event { wd: wd as i32, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        watch.stream.take()
    };
    if let Some(stream) = stream {
        stream.stop(&shared.queue);
    }
    Ok(0)
}

/// `read` on an instance: as many whole events as fit.
pub(crate) fn inotify_read(p: &Process, t: &Thread, file: &OpenFile, buf: &mut [u8]) -> Result<i64, i64> {
    let shared = instance(file)?;
    let kq = file.host.as_raw_fd();
    loop {
        {
            let mut state = shared.lock();
            if let Some(first) = state.queue.front() {
                if first.size() > buf.len() {
                    return Err(errno::EINVAL);
                }
                let mut out = Vec::new();
                while let Some(event) = state.queue.front() {
                    if out.len() + event.size() > buf.len() {
                        break;
                    }
                    event.write(&mut out);
                    state.queue.pop_front();
                }
                kfd::drain(kq);
                if !state.queue.is_empty() {
                    kfd::change(kq, kfd::user_event(0, libc::NOTE_TRIGGER))?;
                }
                buf[..out.len()].copy_from_slice(&out);
                return Ok(out.len() as i64);
            }
        }
        if kfd::nonblocking(file) {
            return Err(errno::EAGAIN);
        }
        kfd::wait(p, t, kq)?;
    }
}

/// `FIONREAD`: the bytes the queued events take.
pub(crate) fn inotify_pending(file: &OpenFile) -> Option<usize> {
    let shared = instance(file).ok()?;
    Some(shared.lock().queue.iter().map(Event::size).sum())
}

/// What a stream's callback needs: its instance (weakly: the instance owns
/// the stream) and the watch it's for.
struct Context {
    shared: std::sync::Weak<Shared>,
    wd: i32,
}

// FSEvents event flags.
const MUST_SCAN_SUBDIRS: u32 = 0x1;
const ITEM_REMOVED: u32 = 0x200;
const ITEM_INODE_META_MOD: u32 = 0x400;
const ITEM_RENAMED: u32 = 0x800;
const ITEM_MODIFIED: u32 = 0x1000;
const ITEM_FINDER_INFO_MOD: u32 = 0x2000;
const ITEM_CHANGE_OWNER: u32 = 0x4000;
const ITEM_XATTR_MOD: u32 = 0x8000;

/// FSEvents' report for watch `wd`: `(path, flags)` in order.
fn handle(shared: &Shared, wd: i32, events: &[(Vec<u8>, u32)]) {
    let mut state = shared.lock();
    let state = &mut *state;
    // Renames in one report, by inode: the cookie pairing a move's two halves.
    let mut moves: HashMap<u64, u32> = HashMap::new();
    let mut out = Vec::new();
    let mut gone = false;
    {
        let Some(watch) = state.watches.get_mut(&wd) else { return };
        for (path, flags) in events {
            if flags & MUST_SCAN_SUBDIRS != 0 {
                out.push(Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() });
                continue;
            }
            let attrib = flags & (ITEM_INODE_META_MOD | ITEM_CHANGE_OWNER | ITEM_XATTR_MOD | ITEM_FINDER_INFO_MOD) != 0;
            if *path == watch.path {
                // The watched file or directory itself.
                let now = CString::new(path.clone()).ok().and_then(|p| stat(&p, false).ok());
                match now {
                    Some((ino, _)) if ino == watch.ino => {
                        if flags & ITEM_MODIFIED != 0 && watch.entries.is_none() {
                            out.push(Event { wd, mask: IN_MODIFY, cookie: 0, name: Vec::new() });
                            out.push(Event { wd, mask: IN_CLOSE_WRITE, cookie: 0, name: Vec::new() });
                        }
                        if attrib {
                            out.push(Event { wd, mask: IN_ATTRIB, cookie: 0, name: Vec::new() });
                        }
                    }
                    // Moved away (and maybe something else put there).
                    _ if flags & ITEM_RENAMED != 0 => out.push(Event { wd, mask: IN_MOVE_SELF, cookie: 0, name: Vec::new() }),
                    _ => {
                        out.push(Event { wd, mask: IN_DELETE_SELF, cookie: 0, name: Vec::new() });
                        gone = true;
                        break;
                    }
                }
                continue;
            }
            // An entry of a watched directory (not deeper: FSEvents reports
            // the whole tree).
            let Some(entries) = watch.entries.as_mut() else { continue };
            if parent(path) != watch.path.as_slice() {
                continue;
            }
            let name = path[watch.path.len() + usize::from(watch.path != b"/")..].to_vec();
            let now = CString::new(path.clone()).ok().and_then(|p| stat(&p, false).ok());
            let before = entries.get(&name).copied();
            let isdir = |d: bool| if d { IN_ISDIR } else { 0 };
            let event = |mask, cookie| Event { wd, mask, cookie, name: name.clone() };
            match (before, now) {
                // New here: created, or moved in.
                (None, Some((ino, dir))) => {
                    if flags & ITEM_RENAMED != 0 {
                        out.push(event(IN_MOVED_TO | isdir(dir), *moves.entry(ino).or_insert_with(|| next(&mut state.next_cookie))));
                    } else {
                        out.push(event(IN_CREATE | isdir(dir), 0));
                        if flags & ITEM_MODIFIED != 0 && !dir {
                            out.push(event(IN_MODIFY, 0));
                            out.push(event(IN_CLOSE_WRITE, 0));
                        }
                    }
                    entries.insert(name, (ino, dir));
                }
                // Gone: deleted, or moved out. (FSEvents' flags pile up over
                // a path's recent past: a file moved here and then deleted
                // is "renamed" too, but "removed" only if it was.)
                (Some((ino, dir)), None) => {
                    if flags & ITEM_RENAMED != 0 && flags & ITEM_REMOVED == 0 {
                        out.push(event(IN_MOVED_FROM | isdir(dir), *moves.entry(ino).or_insert_with(|| next(&mut state.next_cookie))));
                    } else {
                        out.push(event(IN_DELETE | isdir(dir), 0));
                    }
                    entries.remove(&name);
                }
                // Something else under the same name: moved (or written)
                // over it.
                (Some((old, _)), Some((ino, dir))) if old != ino => {
                    let cookie = if flags & ITEM_RENAMED != 0 { *moves.entry(ino).or_insert_with(|| next(&mut state.next_cookie)) } else { 0 };
                    out.push(event(if cookie != 0 { IN_MOVED_TO } else { IN_CREATE } | isdir(dir), cookie));
                    entries.insert(name, (ino, dir));
                }
                (Some(_), Some((_, dir))) => {
                    if flags & ITEM_MODIFIED != 0 && !dir {
                        out.push(event(IN_MODIFY, 0));
                        out.push(event(IN_CLOSE_WRITE, 0));
                    }
                    if attrib {
                        out.push(event(IN_ATTRIB | isdir(dir), 0));
                    }
                }
                (None, None) => {}
            }
        }
    }
    let watch = state.watches.get(&wd).map(|w| w.mask);
    let Some(mask) = watch else { return };
    let mut fired = false;
    for event in out {
        if event.mask & (mask | IN_ALWAYS) & !IN_ISDIR != 0 {
            fired |= event.mask & IN_ALWAYS == 0;
            shared.push(state, event);
        }
    }
    if gone || (fired && mask & IN_ONESHOT != 0) {
        shared.push(state, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        if let Some(stream) = state.watches.remove(&wd).and_then(|mut w| w.stream.take()) {
            // This is the stream's own callback: stopping it can't wait.
            stream.stop_here(&shared.queue);
        }
    }
}

fn next(cookie: &mut u32) -> u32 {
    let c = *cookie;
    *cookie = cookie.wrapping_add(1).max(1);
    c
}

// --- FSEvents, CoreFoundation and libdispatch -------------------------------

type FsEventStreamRef = *mut c_void;
type Callback = extern "C" fn(FsEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct FsEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x2;
const CREATE_WATCH_ROOT: u32 = 0x4;
const CREATE_FILE_EVENTS: u32 = 0x10;
const UTF8: u32 = 0x0800_0100;
/// Seconds FSEvents may gather events before reporting them.
const LATENCY: f64 = 0.01;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: Callback,
        context: *const FsEventStreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FsEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FsEventStreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: FsEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FsEventStreamRef);
    fn FSEventStreamInvalidate(stream: FsEventStreamRef);
    fn FSEventStreamRelease(stream: FsEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(allocator: *const c_void, bytes: *const u8, len: isize, encoding: u32, external: u8) -> *const c_void;
    fn CFArrayCreate(allocator: *const c_void, values: *const *const c_void, n: isize, callbacks: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    static kCFTypeArrayCallBacks: c_void;
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// A serial dispatch queue.
struct DispatchQueue(*mut c_void);

// Safety: dispatch queues are thread-safe.
unsafe impl Send for DispatchQueue {}
unsafe impl Sync for DispatchQueue {}

impl DispatchQueue {
    fn new() -> Self {
        DispatchQueue(unsafe { dispatch_queue_create(c"macwl.inotify".as_ptr(), std::ptr::null()) })
    }
}

impl Drop for DispatchQueue {
    fn drop(&mut self) {
        unsafe { dispatch_release(self.0) };
    }
}

/// A running FSEvent stream.
struct Stream(FsEventStreamRef);

// Safety: FSEvent streams may be stopped from any thread.
unsafe impl Send for Stream {}

extern "C" fn retain(info: *const c_void) -> *const c_void {
    unsafe { Arc::increment_strong_count(info as *const Context) };
    info
}

extern "C" fn release(info: *const c_void) {
    unsafe { Arc::decrement_strong_count(info as *const Context) };
}

extern "C" fn callback(_: FsEventStreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _ids: *const u64) {
    let context = unsafe { &*(info as *const Context) };
    let Some(shared) = context.shared.upgrade() else { return };
    let paths = unsafe { std::slice::from_raw_parts(paths as *const *const c_char, n) };
    let flags = unsafe { std::slice::from_raw_parts(flags, n) };
    let events: Vec<(Vec<u8>, u32)> =
        paths.iter().zip(flags).map(|(&p, &f)| (unsafe { CStr::from_ptr(p) }.to_bytes().to_vec(), f)).collect();
    handle(&shared, context.wd, &events);
}

impl Stream {
    /// Watch `dir` (a real host path), calling back on `queue`.
    fn start(dir: &[u8], context: Arc<Context>, queue: &DispatchQueue) -> Result<Stream, i64> {
        unsafe {
            let path = CFStringCreateWithBytes(std::ptr::null(), dir.as_ptr(), dir.len() as isize, UTF8, 0);
            if path.is_null() {
                return Err(errno::ENOMEM);
            }
            let paths = CFArrayCreate(std::ptr::null(), &path, 1, &raw const kCFTypeArrayCallBacks);
            CFRelease(path);
            let info = Arc::into_raw(context) as *mut c_void;
            let ctx = FsEventStreamContext { version: 0, info, retain: Some(retain), release: Some(release), copy_description: None };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                callback,
                &ctx,
                paths,
                SINCE_NOW,
                LATENCY,
                CREATE_NO_DEFER | CREATE_FILE_EVENTS | CREATE_WATCH_ROOT,
            );
            CFRelease(paths);
            // The stream holds its own reference now.
            release(info);
            if stream.is_null() {
                return Err(errno::ENOMEM);
            }
            FSEventStreamSetDispatchQueue(stream, queue.0);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err(errno::ENOSPC);
            }
            Ok(Stream(stream))
        }
    }

    /// Stop it, waiting out a callback that's running (so not from one).
    fn stop(self, queue: &DispatchQueue) {
        extern "C" fn work(stream: *mut c_void) {
            unsafe {
                FSEventStreamStop(stream);
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
        }
        unsafe { dispatch_sync_f(queue.0, self.0, work) };
    }

    /// Stop it from its own callback (on `queue`): released once the
    /// callback is done.
    fn stop_here(self, queue: &DispatchQueue) {
        extern "C" fn work(stream: *mut c_void) {
            unsafe { FSEventStreamRelease(stream) };
        }
        unsafe {
            FSEventStreamStop(self.0);
            FSEventStreamInvalidate(self.0);
            dispatch_async_f(queue.0, self.0, work);
        }
    }
}
