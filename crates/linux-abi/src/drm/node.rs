//! An open render node: its ioctls (Linux's DRM core and virtio-gpu
//! driver's), and what it holds: GEM handles for blobs, sync objects, the
//! context its first `CONTEXT_INIT` made.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use super::sync::{self, Fence, Syncobj, Token};
use crate::{
    errno,
    memory::{GuestMemory, GuestMemoryExt},
    syscall::Thread,
    Process,
};

/// `_IOC` fields of an ioctl request.
const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}
const RW: u64 = 3;
const W: u64 = 1;

/// DRM's ioctls, by number (`'d'`): the size varies by uapi version, so
/// they're told by type and number.
mod nr {
    pub const VERSION: u8 = 0x00;
    pub const GEM_CLOSE: u8 = 0x09;
    pub const GET_CAP: u8 = 0x0c;
    pub const SET_CLIENT_CAP: u8 = 0x0d;
    pub const PRIME_HANDLE_TO_FD: u8 = 0x2d;
    pub const PRIME_FD_TO_HANDLE: u8 = 0x2e;
    pub const SYNCOBJ_CREATE: u8 = 0xbf;
    pub const SYNCOBJ_DESTROY: u8 = 0xc0;
    pub const SYNCOBJ_HANDLE_TO_FD: u8 = 0xc1;
    pub const SYNCOBJ_FD_TO_HANDLE: u8 = 0xc2;
    pub const SYNCOBJ_WAIT: u8 = 0xc3;
    pub const SYNCOBJ_RESET: u8 = 0xc4;
    pub const SYNCOBJ_SIGNAL: u8 = 0xc5;
    pub const SYNCOBJ_TIMELINE_WAIT: u8 = 0xca;
    pub const SYNCOBJ_QUERY: u8 = 0xcb;
    pub const SYNCOBJ_TRANSFER: u8 = 0xcc;
    pub const SYNCOBJ_TIMELINE_SIGNAL: u8 = 0xcd;
    // virtio-gpu's, from DRM_COMMAND_BASE (0x40).
    pub const VIRTGPU_MAP: u8 = 0x41;
    pub const VIRTGPU_EXECBUFFER: u8 = 0x42;
    pub const VIRTGPU_GETPARAM: u8 = 0x43;
    pub const VIRTGPU_RESOURCE_INFO: u8 = 0x45;
    pub const VIRTGPU_WAIT: u8 = 0x48;
    pub const VIRTGPU_GET_CAPS: u8 = 0x49;
    pub const VIRTGPU_RESOURCE_CREATE_BLOB: u8 = 0x4a;
    pub const VIRTGPU_CONTEXT_INIT: u8 = 0x4b;
}

const DRM_CAP_PRIME: u64 = 0x5;
const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
const DRM_PRIME_CAP_IMPORT: u64 = 1;
const DRM_PRIME_CAP_EXPORT: u64 = 2;

const SYNCOBJ_CREATE_SIGNALED: u32 = 1;
const SYNCOBJ_FD_TO_HANDLE_IMPORT_SYNC_FILE: u32 = 1;
const SYNCOBJ_HANDLE_TO_FD_EXPORT_SYNC_FILE: u32 = 1;
const SYNCOBJ_WAIT_ALL: u32 = 1;
const SYNCOBJ_WAIT_FOR_SUBMIT: u32 = 2;
const SYNCOBJ_WAIT_AVAILABLE: u32 = 4;
const SYNCOBJ_QUERY_LAST_SUBMITTED: u32 = 1;

/// An open render node.
#[derive(Debug, Default)]
pub struct Node {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    syncobjs: HashMap<u32, Arc<Syncobj>>,
    next_handle: u32,
    pub(super) gpu: Option<super::virgl::Context>,
}

impl Node {
    pub fn new() -> Arc<Node> {
        Arc::new(Node::default())
    }

    pub(super) fn syncobj(&self, handle: u32) -> Result<Arc<Syncobj>, i64> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).syncobjs.get(&handle).cloned().ok_or(errno::EINVAL)
    }

    fn add_syncobj(&self, s: Arc<Syncobj>) -> u32 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.next_handle += 1;
        let h = st.next_handle;
        st.syncobjs.insert(h, s);
        h
    }

    /// The node's GPU context, once `CONTEXT_INIT` has made it.
    pub(super) fn context(&self) -> Result<super::virgl::Context, i64> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).gpu.clone().ok_or(errno::EINVAL)
    }

    pub(super) fn set_context(&self, c: super::virgl::Context) -> Result<(), i64> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.gpu.is_some() {
            return Err(errno::EBUSY);
        }
        st.gpu = Some(c);
        Ok(())
    }
}

/// An ioctl on the node.
pub fn ioctl(node: &Arc<Node>, p: &Process, t: &Thread, mem: &dyn GuestMemory, req: u64, arg: u64) -> Result<i64, i64> {
    if (req >> 8) & 0xff != b'd' as u64 {
        return Err(errno::ENOTTY);
    }
    let size = ((req >> 16) & 0x3fff) as usize;
    match (req & 0xff) as u8 {
        nr::VERSION => version(mem, arg),
        nr::GET_CAP => {
            let cap: u64 = mem.read_pod(arg)?;
            let value = match cap {
                DRM_CAP_PRIME => DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT,
                DRM_CAP_SYNCOBJ | DRM_CAP_SYNCOBJ_TIMELINE => 1,
                _ => return Err(errno::EINVAL),
            };
            mem.write_pod(arg + 8, &value)?;
            Ok(0)
        }
        nr::SET_CLIENT_CAP => Err(errno::EINVAL),
        nr::SYNCOBJ_CREATE => {
            let [handle, flags]: [u32; 2] = mem.read_pod(arg)?;
            let _ = handle;
            let h = node.add_syncobj(Syncobj::new(flags & SYNCOBJ_CREATE_SIGNALED != 0));
            mem.write_pod(arg, &h)?;
            Ok(0)
        }
        nr::SYNCOBJ_DESTROY => {
            let [handle, _]: [u32; 2] = mem.read_pod(arg)?;
            node.state.lock().unwrap_or_else(|e| e.into_inner()).syncobjs.remove(&handle).map(|_| 0).ok_or(errno::EINVAL)
        }
        nr::SYNCOBJ_HANDLE_TO_FD => {
            // { handle, flags, fd, point (newer uapis; pad before) }
            let [handle, flags, _fd, _pad]: [u32; 4] = mem.read_pod(arg)?;
            let s = node.syncobj(handle)?;
            let token = if flags & SYNCOBJ_HANDLE_TO_FD_EXPORT_SYNC_FILE != 0 {
                let point = if size >= 24 { mem.read_pod::<u64>(arg + 16)? } else { 0 };
                Token::SyncFile(s.fence(point).ok_or(errno::EINVAL)?)
            } else {
                Token::Syncobj(s)
            };
            let fd = p.fds.insert(sync::entry(token)?);
            mem.write_pod(arg + 8, &(fd as i32))?;
            Ok(0)
        }
        nr::SYNCOBJ_FD_TO_HANDLE => {
            let [handle, flags, fd, _pad]: [u32; 4] = mem.read_pod(arg)?;
            let token = sync::token_of_fd(p, fd as i32 as u64).ok_or(errno::EINVAL)?;
            if flags & SYNCOBJ_FD_TO_HANDLE_IMPORT_SYNC_FILE != 0 {
                let Token::SyncFile(f) = token else { return Err(errno::EINVAL) };
                let point = if size >= 24 { mem.read_pod::<u64>(arg + 16)? } else { 0 };
                node.syncobj(handle)?.attach(point, f);
            } else {
                let Token::Syncobj(s) = token else { return Err(errno::EINVAL) };
                let h = node.add_syncobj(s);
                mem.write_pod(arg, &h)?;
            }
            Ok(0)
        }
        nr::SYNCOBJ_RESET => {
            let (handles, count) = handle_array(mem, arg)?;
            for h in handles_of(mem, handles, count)? {
                node.syncobj(h)?.reset();
            }
            Ok(0)
        }
        nr::SYNCOBJ_SIGNAL => {
            let (handles, count) = handle_array(mem, arg)?;
            for h in handles_of(mem, handles, count)? {
                node.syncobj(h)?.attach(0, Fence::signaled_now());
            }
            Ok(0)
        }
        nr::SYNCOBJ_TIMELINE_SIGNAL => {
            // { handles, points, count_handles, flags }
            let (handles, points, count) = (mem.read_pod::<u64>(arg)?, mem.read_pod::<u64>(arg + 8)?, mem.read_pod::<u32>(arg + 16)?);
            let hs = handles_of(mem, handles, count)?;
            for (i, h) in hs.into_iter().enumerate() {
                let point = mem.read_pod::<u64>(points + 8 * i as u64)?;
                node.syncobj(h)?.attach(point, Fence::signaled_now());
            }
            Ok(0)
        }
        nr::SYNCOBJ_QUERY => {
            let (handles, points, count, flags) = (mem.read_pod::<u64>(arg)?, mem.read_pod::<u64>(arg + 8)?, mem.read_pod::<u32>(arg + 16)?, mem.read_pod::<u32>(arg + 20)?);
            for (i, h) in handles_of(mem, handles, count)?.into_iter().enumerate() {
                let s = node.syncobj(h)?;
                let value = if flags & SYNCOBJ_QUERY_LAST_SUBMITTED != 0 { s.last_point() } else { s.signaled_point() };
                mem.write_pod(points + 8 * i as u64, &value)?;
            }
            Ok(0)
        }
        nr::SYNCOBJ_TRANSFER => {
            // { src_handle, dst_handle, src_point, dst_point, flags, pad }
            let [src, dst]: [u32; 2] = mem.read_pod(arg)?;
            let (src_point, dst_point) = (mem.read_pod::<u64>(arg + 8)?, mem.read_pod::<u64>(arg + 16)?);
            let fence = node.syncobj(src)?.fence(src_point).ok_or(errno::EINVAL)?;
            node.syncobj(dst)?.attach(dst_point, fence);
            Ok(0)
        }
        nr::SYNCOBJ_WAIT => {
            // { handles, timeout_nsec, count_handles, flags, first_signaled, pad, (deadline) }
            let (handles, timeout, count, flags) = (mem.read_pod::<u64>(arg)?, mem.read_pod::<i64>(arg + 8)?, mem.read_pod::<u32>(arg + 16)?, mem.read_pod::<u32>(arg + 20)?);
            let hs = handles_of(mem, handles, count)?;
            let points = vec![0; hs.len()];
            let first = wait(node, p, t, &hs, &points, timeout, flags)?;
            mem.write_pod(arg + 24, &first)?;
            Ok(0)
        }
        nr::SYNCOBJ_TIMELINE_WAIT => {
            // { handles, points, timeout_nsec, count_handles, flags, first_signaled, pad, (deadline) }
            let (handles, pts, timeout) = (mem.read_pod::<u64>(arg)?, mem.read_pod::<u64>(arg + 8)?, mem.read_pod::<i64>(arg + 16)?);
            let (count, flags) = (mem.read_pod::<u32>(arg + 24)?, mem.read_pod::<u32>(arg + 28)?);
            let hs = handles_of(mem, handles, count)?;
            let points = (0..hs.len()).map(|i| mem.read_pod::<u64>(pts + 8 * i as u64)).collect::<Result<Vec<_>, _>>()?;
            let first = wait(node, p, t, &hs, &points, timeout, flags)?;
            mem.write_pod(arg + 32, &first)?;
            Ok(0)
        }
        nr::GEM_CLOSE
        | nr::PRIME_HANDLE_TO_FD
        | nr::PRIME_FD_TO_HANDLE
        | nr::VIRTGPU_MAP
        | nr::VIRTGPU_EXECBUFFER
        | nr::VIRTGPU_GETPARAM
        | nr::VIRTGPU_RESOURCE_INFO
        | nr::VIRTGPU_WAIT
        | nr::VIRTGPU_GET_CAPS
        | nr::VIRTGPU_RESOURCE_CREATE_BLOB
        | nr::VIRTGPU_CONTEXT_INIT => super::virgl::ioctl(node, p, t, mem, (req & 0xff) as u8, size, arg),
        _ => Err(errno::EINVAL),
    }
}

/// `DRM_IOCTL_VERSION`: virtio_gpu 0.1.0, as Linux's driver says.
fn version(mem: &dyn GuestMemory, arg: u64) -> Result<i64, i64> {
    // { int major, minor, patchlevel; size_t name_len; char *name;
    //   size_t date_len; char *date; size_t desc_len; char *desc }
    mem.write_pod(arg, &[0i32, 1, 0])?;
    for (i, s) in [&b"virtio_gpu"[..], b"0", b"virtio GPU"].into_iter().enumerate() {
        let at = arg + 16 + 16 * i as u64;
        let (cap, ptr) = (mem.read_pod::<u64>(at)?, mem.read_pod::<u64>(at + 8)?);
        if ptr != 0 && cap > 0 {
            mem.write(ptr, &s[..s.len().min(cap as usize)])?;
        }
        mem.write_pod(at, &(s.len() as u64))?;
    }
    Ok(0)
}

/// A syncobj ioctl's `{ u64 handles; ... u32 count }` (count at 8 for
/// RESET and SIGNAL).
fn handle_array(mem: &dyn GuestMemory, arg: u64) -> Result<(u64, u32), i64> {
    Ok((mem.read_pod::<u64>(arg)?, mem.read_pod::<u32>(arg + 8)?))
}

fn handles_of(mem: &dyn GuestMemory, addr: u64, count: u32) -> Result<Vec<u32>, i64> {
    if count > 4096 {
        return Err(errno::EINVAL);
    }
    (0..count as u64).map(|i| mem.read_pod::<u32>(addr + 4 * i)).collect()
}

/// Wait for syncobjs' points (all, or any: which came first), as
/// `SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT` do. A missing fence is
/// EINVAL unless WAIT_FOR_SUBMIT (then waited for). `timeout` is an
/// absolute CLOCK_MONOTONIC time in ns.
fn wait(node: &Node, p: &Process, t: &Thread, handles: &[u32], points: &[u64], timeout: i64, flags: u32) -> Result<u32, i64> {
    let objs = handles.iter().map(|&h| node.syncobj(h)).collect::<Result<Vec<_>, _>>()?;
    let deadline = deadline_of(timeout);
    let all = flags & SYNCOBJ_WAIT_ALL != 0;
    let waited_from = super::trace::now();
    let mut first = 0u32;
    sync::wait_until(p, t, deadline, |register| {
        let mut done = 0;
        for (i, (s, &point)) in objs.iter().zip(points).enumerate() {
            let fence = s.fence(point);
            let ok = match &fence {
                Some(f) => flags & SYNCOBJ_WAIT_AVAILABLE != 0 || f.signaled(),
                None if flags & SYNCOBJ_WAIT_FOR_SUBMIT != 0 => {
                    s.on_attach(t.waker.clone());
                    false
                }
                None => return Err(errno::EINVAL),
            };
            if ok {
                if !all {
                    first = i as u32;
                    return Ok(true);
                }
                done += 1;
            } else if let Some(f) = &fence {
                register(f);
            }
        }
        Ok(all && done == objs.len())
    })?;
    // (A lost device fails its fences: say so.)
    if objs.iter().zip(points).any(|(s, &pt)| s.fence(pt).is_some_and(|f| f.failed())) {
        return Err(errno::ENODEV);
    }
    for (s, &pt) in objs.iter().zip(points) {
        if let Some(f) = s.fence(pt) {
            super::trace::waited(&f, waited_from);
        }
        s.trim();
    }
    Ok(first)
}

/// An absolute CLOCK_MONOTONIC time in ns as an `Instant` (None: for ever).
pub(super) fn deadline_of(abs_ns: i64) -> Option<Instant> {
    if abs_ns <= 0 {
        return Some(Instant::now());
    }
    if abs_ns == i64::MAX {
        return None;
    }
    // The guest's CLOCK_MONOTONIC is the host's UPTIME_RAW (`clock_now`;
    // the host's MONOTONIC counts time asleep too, and runs ahead).
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    let now = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
    Some(Instant::now() + Duration::from_nanos(abs_ns.saturating_sub(now).max(0) as u64))
}

/// A sync file's ioctls (`'>'`): MERGE and FILE_INFO.
pub fn sync_file_ioctl(p: &Process, mem: &dyn GuestMemory, fence: &Arc<Fence>, req: u64, arg: u64) -> Result<i64, i64> {
    const MERGE: u8 = 3;
    const FILE_INFO: u8 = 4;
    match (req & 0xff) as u8 {
        MERGE => {
            // { char name[32]; s32 fd2; s32 fence }
            let fd2: i32 = mem.read_pod(arg + 32)?;
            let Some(Token::SyncFile(other)) = sync::token_of_fd(p, fd2 as u64) else { return Err(errno::EINVAL) };
            let merged = Fence::all(&[fence.clone(), other]);
            let fd = p.fds.insert(sync::entry(Token::SyncFile(merged))?);
            mem.write_pod(arg + 36, &(fd as i32))?;
            Ok(0)
        }
        FILE_INFO => {
            // { char name[32]; s32 status; u32 flags; u32 num_fences; u32 pad; u64 sync_fence_info }
            let status: i32 = if fence.failed() { -errno::ENODEV as i32 } else { fence.signaled() as i32 };
            mem.write_pod(arg + 32, &status)?;
            mem.write_pod(arg + 40, &0u32)?;
            Ok(0)
        }
        _ => Err(errno::ENOTTY),
    }
}

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE` (`'b'`, 2 and
/// 3), on a blob's fd (host fd `host`): implicit sync, as Linux keeps it
/// for a dma-buf. An imported fence is the buffer's until the next: an
/// export gives it (or one signalled already), and the Mac's compositor is
/// told, to wait for it before reading the buffer (`fences`). With no
/// compositor listening, the import waits for it (docs/gpu.md, "Implicit
/// fencing").
pub fn dma_buf_ioctl(p: &Process, t: &Thread, host: i32, mem: &dyn GuestMemory, req: u64, arg: u64) -> Option<Result<i64, i64>> {
    const EXPORT_SYNC_FILE: u64 = ioc(RW, b'b', 2, 8);
    const IMPORT_SYNC_FILE: u64 = ioc(W, b'b', 3, 8);
    match req {
        // { u32 flags; s32 fd }
        EXPORT_SYNC_FILE => Some((|| {
            let fence = implicit(host).filter(|f| !f.signaled()).unwrap_or_else(Fence::signaled_now);
            let fd = p.fds.insert(sync::entry(Token::SyncFile(fence))?);
            mem.write_pod(arg + 4, &(fd as i32))?;
            Ok(0)
        })()),
        IMPORT_SYNC_FILE => Some((|| {
            let fd: i32 = mem.read_pod(arg + 4)?;
            let Some(Token::SyncFile(fence)) = sync::token_of_fd(p, fd as u64) else { return Err(errno::EINVAL) };
            if fence.signaled() {
                return Ok(0);
            }
            set_implicit(host, &fence);
            if !super::fences::announce(host, &fence) {
                sync::wait_until(p, t, None, |register| {
                    if fence.signaled() {
                        return Ok(true);
                    }
                    register(&fence);
                    Ok(false)
                })?;
            }
            Ok(0)
        })()),
        _ => None,
    }
}

/// Blobs' last imported fences, by what names their memory
/// (`fences::buffer_key`): a weak hold, so nothing here outlives its use.
static IMPLICIT: Mutex<Vec<([u8; 64], Weak<Fence>)>> = Mutex::new(Vec::new());

fn implicit(host: i32) -> Option<Arc<Fence>> {
    let id = super::fences::buffer_key(host)?;
    IMPLICIT.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == id).and_then(|(_, f)| f.upgrade())
}

fn set_implicit(host: i32, fence: &Arc<Fence>) {
    let Some(id) = super::fences::buffer_key(host) else { return };
    let mut all = IMPLICIT.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(i, f)| *i != id && f.strong_count() > 0);
    all.push((id, Arc::downgrade(fence)));
}
