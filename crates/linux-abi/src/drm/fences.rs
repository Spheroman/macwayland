//! Telling the Mac's compositor when a dma-buf's pixels are drawn
//! (docs/gpu.md, "Implicit fencing"): the other end of
//! `macwl_compositor::fences`, whose module says what's sent.
//!
//! Venus with implicit fencing (`venus_implicit_fencing`, which macwl-run
//! sets for guests) commits a frame as soon as it's submitted, having
//! attached the fence of its drawing to the buffer
//! (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`). We tell the compositor, which waits
//! for it on its own GPU queue. With no compositor listening, the import
//! waits for the fence itself (`node::dma_buf_ioctl`), as Venus would have.

use std::{
    io::Write,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::sync::Fence;

const KEY: usize = 64;
const WAIT: u32 = 1;
const DONE: u32 = 2;

/// What names a buffer's memory in every process that has it: a POSIX shm
/// object's name (which macOS keeps after it's unlinked: `fstat` says 0 for
/// its device and inode), or a file's device and inode. (As
/// `macwl_compositor::fences::buffer_key`.)
pub(super) fn buffer_key(fd: i32) -> Option<[u8; KEY]> {
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

/// The compositor's fences socket: `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY.fences`
/// (macwl-run's own, which its guests get too).
fn socket_path() -> Option<PathBuf> {
    let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY")?);
    let base = if display.is_absolute() { display } else { PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join(display) };
    let mut name = base.into_os_string();
    name.push(".fences");
    Some(name.into())
}

struct Channel {
    stream: Option<UnixStream>,
    /// When connecting last failed (tried again after a second).
    failed: Option<Instant>,
}

static CHANNEL: Mutex<Channel> = Mutex::new(Channel { stream: None, failed: None });
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Send a record; false if there's no compositor to send it to.
fn send(op: u32, seq: u64, key: &[u8; KEY]) -> bool {
    let mut record = [0u8; 16 + KEY];
    record[0..4].copy_from_slice(&op.to_le_bytes());
    record[8..16].copy_from_slice(&seq.to_le_bytes());
    record[16..].copy_from_slice(key);
    let mut c = CHANNEL.lock().unwrap_or_else(|e| e.into_inner());
    if c.stream.is_none() {
        if c.failed.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return false;
        }
        match socket_path().map(UnixStream::connect) {
            Some(Ok(s)) => {
                // A compositor gone is an error, not SIGPIPE.
                let on: libc::c_int = 1;
                unsafe {
                    libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&raw const on).cast(), size_of::<libc::c_int>() as u32)
                };
                c.stream = Some(s);
                c.failed = None;
            }
            _ => {
                c.failed = Some(Instant::now());
                return false;
            }
        }
    }
    let ok = c.stream.as_mut().is_some_and(|s| s.write_all(&record).is_ok());
    if !ok {
        c.stream = None;
        c.failed = Some(Instant::now());
    }
    ok
}

/// Tell the compositor that `fence` guards the buffer whose memory is host
/// fd `fd`. False if it can't be told: then nothing will wait for the
/// fence but the caller.
pub fn announce(fd: i32, fence: &Arc<Fence>) -> bool {
    let Some(key) = buffer_key(fd) else { return false };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if !send(WAIT, seq, &key) {
        return false;
    }
    fence.then(move || {
        send(DONE, seq, &[0; KEY]);
    });
    true
}
