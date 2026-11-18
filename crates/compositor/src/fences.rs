//! Implicit sync for dma-bufs (docs/gpu.md, "Implicit fencing"): when a
//! buffer's pixels are ready, for a frame committed before the GPU has
//! drawn it.
//!
//! On Linux a dma-buf carries the fences of the GPU work writing it, and a
//! compositor waits for them before reading. Here the buffer is shared
//! memory and its fences are macwl-run's, so macwl-run tells us: it
//! connects to `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY.fences` and sends, as a
//! client attaches a fence to a buffer (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`,
//! before that client's commit), `WAIT seq key`, and once that fence has
//! signalled, `DONE seq`. Both are fixed-size [`Record`]s; `key` names
//! the buffer's memory ([`buffer_key`]), `seq` is per connection.
//!
//! A commit and its WAIT come on different sockets, but the WAIT was sent
//! first: it's in this socket's buffer when the commit is read, so
//! [`Fences::ready_for`] reads what's waiting before it looks. A buffer
//! whose fence is pending gets a [`Ready`], to wait on (or be told by)
//! before reading it. A connection that ends signals all it was waiting on.

use std::{
    collections::HashMap,
    io::Read,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use tracing::warn;

/// A record's size: op u32, 0 u32, seq u64, key [u8; KEY].
pub const RECORD: usize = 16 + KEY;
pub const KEY: usize = 64;
pub const WAIT: u32 = 1;
pub const DONE: u32 = 2;

/// What names a buffer's memory in every process that has it: a POSIX shm
/// object's name (which macOS keeps after it's unlinked: `fstat` says 0 for
/// its device and inode), or a file's device and inode. (macwl-run's
/// `linux-abi` `drm::fences` makes the same.)
pub fn buffer_key(fd: RawFd) -> Option<[u8; KEY]> {
    let mut key = [0u8; KEY];
    // struct pshm_fdinfo: proc_fileinfo (24 bytes), then pshm_info's
    // vinfo_stat (136) and mapping address (8), then its name.
    const PROC_PIDFDPSHMINFO: i32 = 5;
    const SIZE: usize = 1192;
    const NAME_AT: usize = 168;
    let mut info = [0u8; SIZE];
    let n = unsafe { libc::proc_pidfdinfo(libc::getpid(), fd, PROC_PIDFDPSHMINFO, info.as_mut_ptr().cast(), SIZE as i32) };
    if n as usize == SIZE {
        let name = &info[NAME_AT..];
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(0)];
        if !name.is_empty() && name.len() < KEY - 1 {
            key[0] = b's';
            key[1..1 + name.len()].copy_from_slice(name);
            return Some(key);
        }
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || st.st_ino == 0 {
        return None;
    }
    key[0] = b'f';
    key[1..9].copy_from_slice(&(st.st_dev as u64).to_le_bytes());
    key[9..17].copy_from_slice(&st.st_ino.to_le_bytes());
    Some(key)
}

type Callback = Box<dyn FnOnce() + Send>;

/// A buffer's pending fence, as told: done once it has signalled.
#[derive(Clone, Default)]
pub struct Ready(Arc<(Mutex<ReadyState>, Condvar)>);

#[derive(Default)]
struct ReadyState {
    done: bool,
    then: Vec<Callback>,
}

impl Ready {
    pub fn done(&self) -> bool {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).done
    }

    /// Run `f` once it's done (at once, here, if it is).
    pub fn then(&self, f: impl FnOnce() + Send + 'static) {
        {
            let mut s = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
            if !s.done {
                s.then.push(Box::new(f));
                return;
            }
        }
        f();
    }

    /// Block until it's done.
    pub fn wait(&self) {
        let mut s = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        while !s.done {
            s = self.0.1.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn signal(&self) {
        let then = {
            let mut s = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
            if s.done {
                return;
            }
            s.done = true;
            std::mem::take(&mut s.then)
        };
        self.0.1.notify_all();
        for f in then {
            f();
        }
    }
}

struct Conn {
    id: u64,
    stream: UnixStream,
    /// A record read in part.
    partial: Vec<u8>,
    waits: HashMap<u64, Ready>,
}

#[derive(Default)]
struct State {
    conns: Vec<Conn>,
    /// Each buffer's last fence, by key.
    by_key: HashMap<[u8; KEY], Ready>,
    next_id: u64,
}

impl State {
    /// Read what connection `i` has sent; false once it has ended.
    fn drain(&mut self, i: usize) -> bool {
        let mut buf = [0u8; RECORD * 32];
        loop {
            let conn = &mut self.conns[i];
            let n = match conn.stream.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            };
            conn.partial.extend_from_slice(&buf[..n]);
            let whole = conn.partial.len() / RECORD * RECORD;
            let records: Vec<u8> = conn.partial.drain(..whole).collect();
            for r in records.as_chunks::<RECORD>().0 {
                let op = u32::from_le_bytes(r[0..4].try_into().unwrap());
                let seq = u64::from_le_bytes(r[8..16].try_into().unwrap());
                match op {
                    WAIT => {
                        let ready = Ready::default();
                        self.conns[i].waits.insert(seq, ready.clone());
                        self.by_key.insert(r[16..].try_into().unwrap(), ready);
                    }
                    DONE => {
                        if let Some(ready) = self.conns[i].waits.remove(&seq) {
                            ready.signal();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Connection `i` ended: what it was waiting on won't come.
    fn close(&mut self, i: usize) {
        let conn = self.conns.swap_remove(i);
        for ready in conn.waits.into_values() {
            ready.signal();
        }
        self.by_key.retain(|_, r| !r.done());
    }

    fn drain_all(&mut self) {
        let mut i = 0;
        while i < self.conns.len() {
            if self.drain(i) {
                i += 1;
            } else {
                self.close(i);
            }
        }
    }
}

/// The fences socket and what's come on it.
#[derive(Clone)]
pub struct Fences(Arc<Mutex<State>>);

impl Fences {
    /// Listen next to the Wayland socket `display` in `runtime_dir`.
    pub fn listen(runtime_dir: &Path, display: &str) -> std::io::Result<Fences> {
        let path = socket_path(runtime_dir, display);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let fences = Fences(Arc::default());
        let state = fences.0.clone();
        std::thread::Builder::new().name("fences".into()).spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if stream.set_nonblocking(true).is_err() {
                    continue;
                }
                // (The reader polls a clone: still open, ending as the
                // stream does, if the state closes its own first.)
                let Ok(polled) = stream.try_clone() else { continue };
                let id = {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.next_id += 1;
                    let id = s.next_id;
                    s.conns.push(Conn { id, stream, partial: Vec::new(), waits: HashMap::new() });
                    id
                };
                let state = state.clone();
                let _ = std::thread::Builder::new().name("fences conn".into()).spawn(move || read_conn(&state, id, &polled));
            }
        })?;
        Ok(fences)
    }

    /// The pending fence of the buffer whose memory is `fd`, if it has one
    /// not yet signalled.
    pub fn ready_for(&self, fd: RawFd) -> Option<Ready> {
        let key = buffer_key(fd)?;
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.drain_all();
        if s.by_key.len() > 256 {
            s.by_key.retain(|_, r| !r.done());
        }
        s.by_key.get(&key).filter(|r| !r.done()).cloned()
    }
}

pub fn socket_path(runtime_dir: &Path, display: &str) -> PathBuf {
    let display = Path::new(display);
    let base = if display.is_absolute() { display.to_path_buf() } else { runtime_dir.join(display) };
    let mut name = base.into_os_string();
    name.push(".fences");
    name.into()
}

/// A connection's reader: wakes as it's readable, then reads it with the
/// state held (as `ready_for` does, so what's read is seen by both).
fn read_conn(state: &Mutex<State>, id: u64, polled: &UnixStream) {
    loop {
        let mut pfd = libc::pollfd { fd: polled.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            warn!("fences: poll failed");
        }
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(i) = s.conns.iter().position(|c| c.id == id) else { return };
        if !s.drain(i) {
            s.close(i);
            return;
        }
    }
}
