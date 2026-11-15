//! The node's GPU work, through virglrenderer's public API (as a VMM
//! drives it; virglrenderer's own vtest server is the closest model):
//! `libvirglrenderer.1.dylib`, loaded once per host, in proxy mode, its
//! render server a Mac process of ours (`posix_spawn`, never a fork:
//! `get_server_fd`), which runs each Venus context in a worker of its own.
//!
//! virglrenderer's tables are unlocked globals, so every call into it holds
//! `Gpu::calls`. Fences come back on the proxy's sync threads
//! (`write_context_fence`), which take only the fence table.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::{c_char, c_int, c_void, CString},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
};

use super::{
    node::Node,
    sync::{self, Fence, Token},
};
use crate::{
    errno,
    fs::FdEntry,
    memory::{GuestMemory, GuestMemoryExt},
    syscall::Thread,
    Process,
};

const CAPSET_VENUS: u32 = 4;
const BLOB_MEM_HOST3D: u32 = 2;
const FD_TYPE_SHM: u32 = 3;
/// Blobs are whole host pages, so each is a shared window of its own.
const BLOB_ALIGNMENT: u64 = 16 << 10;

const VIRGL_RENDERER_THREAD_SYNC: c_int = 1 << 1;
const VIRGL_RENDERER_VENUS: c_int = 1 << 6;
const VIRGL_RENDERER_NO_VIRGL: c_int = 1 << 7;
const VIRGL_RENDERER_ASYNC_FENCE_CB: c_int = 1 << 8;
const VIRGL_RENDERER_RENDER_SERVER: c_int = 1 << 9;

#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<extern "C" fn(*mut c_void, u32)>,
    create_gl_context: *const c_void,
    destroy_gl_context: *const c_void,
    make_current: *const c_void,
    get_drm_fd: *const c_void,
    write_context_fence: Option<extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<extern "C" fn(*mut c_void, u32) -> c_int>,
    get_egl_display: *const c_void,
}

#[repr(C)]
struct CreateBlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const c_void,
    num_iovs: u32,
}

#[repr(C)]
struct ImportBlobArgs {
    res_handle: u32,
    blob_mem: u32,
    fd_type: u32,
    fd: c_int,
    size: u64,
}

/// virglrenderer's entry points.
struct Lib {
    init: unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int,
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create_with_flags: unsafe extern "C" fn(u32, u32, u32, *const c_char) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    submit_cmd: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    resource_create_blob: unsafe extern "C" fn(*const CreateBlobArgs) -> c_int,
    resource_export_blob: unsafe extern "C" fn(u32, *mut u32, *mut c_int) -> c_int,
    resource_import_blob: unsafe extern "C" fn(*const ImportBlobArgs) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    context_create_fence: unsafe extern "C" fn(u32, u32, u32, u64) -> c_int,
}

/// The host's GPU: virglrenderer, set up once.
struct Gpu {
    lib: Lib,
    /// Held across every call into virglrenderer.
    calls: Mutex<()>,
    next_ctx: AtomicU32,
    next_res: AtomicU32,
    /// Fences not yet retired, by context and ring, by fence id.
    pending: Mutex<HashMap<(u32, u32), BTreeMap<u64, Arc<Fence>>>>,
    /// Blobs whose fds the guest holds, by the fd's (device, inode): a
    /// PRIME import of one finds it again.
    exported: Mutex<HashMap<(u64, u64), Weak<Resource>>>,
    /// Contexts whose worker went (Metal aborting, say): their fences
    /// failed, their calls ENODEV, as a lost device.
    lost: Mutex<std::collections::HashSet<u32>>,
}

fn gpu() -> Result<&'static Gpu, i64> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| match Gpu::start() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("macwl-run: no GPU: {e}");
            None
        }
    })
    .as_ref()
    .ok_or(errno::ENODEV)
}

impl Gpu {
    fn start() -> Result<Gpu, String> {
        let dir = super::gpu_dir().ok_or("MACWL_GPU isn't set")?;
        let path = CString::new(dir.join("libvirglrenderer.1.dylib").into_os_string().into_encoded_bytes()).map_err(|e| e.to_string())?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) };
            return Err(format!("{}: {}", path.to_string_lossy(), err.to_string_lossy()));
        }
        macro_rules! sym {
            ($name:literal) => {{
                let name = concat!($name, "\0");
                let p = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
                if p.is_null() {
                    return Err(format!("virglrenderer has no {}", $name));
                }
                unsafe { std::mem::transmute::<*mut c_void, _>(p) }
            }};
        }
        let lib = Lib {
            init: sym!("virgl_renderer_init"),
            get_cap_set: sym!("virgl_renderer_get_cap_set"),
            fill_caps: sym!("virgl_renderer_fill_caps"),
            context_create_with_flags: sym!("virgl_renderer_context_create_with_flags"),
            context_destroy: sym!("virgl_renderer_context_destroy"),
            submit_cmd: sym!("virgl_renderer_submit_cmd"),
            resource_create_blob: sym!("virgl_renderer_resource_create_blob"),
            resource_export_blob: sym!("virgl_renderer_resource_export_blob"),
            resource_import_blob: sym!("virgl_renderer_resource_import_blob"),
            resource_unref: sym!("virgl_renderer_resource_unref"),
            ctx_attach_resource: sym!("virgl_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!("virgl_renderer_ctx_detach_resource"),
            context_create_fence: sym!("virgl_renderer_context_create_fence"),
        };
        // virglrenderer keeps the pointer: the callbacks live for ever.
        let cbs = Box::leak(Box::new(Callbacks {
            version: 3,
            write_fence: Some(write_fence),
            create_gl_context: std::ptr::null(),
            destroy_gl_context: std::ptr::null(),
            make_current: std::ptr::null(),
            get_drm_fd: std::ptr::null(),
            write_context_fence: Some(write_context_fence),
            get_server_fd: Some(get_server_fd),
            get_egl_display: std::ptr::null(),
        }));
        let flags = VIRGL_RENDERER_VENUS | VIRGL_RENDERER_NO_VIRGL | VIRGL_RENDERER_RENDER_SERVER | VIRGL_RENDERER_THREAD_SYNC | VIRGL_RENDERER_ASYNC_FENCE_CB;
        let r = unsafe { (lib.init)(std::ptr::null_mut(), flags, cbs) };
        if r != 0 {
            return Err(format!("virgl_renderer_init: {r}"));
        }
        Ok(Gpu {
            lib,
            calls: Mutex::new(()),
            next_ctx: AtomicU32::new(1),
            next_res: AtomicU32::new(1),
            pending: Mutex::default(),
            exported: Mutex::default(),
            lost: Mutex::default(),
        })
    }

    fn call<R>(&self, f: impl FnOnce(&Lib) -> R) -> R {
        let _held = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        f(&self.lib)
    }

    /// Fail every pending fence of context `ctx` (it's going, or its
    /// worker went).
    fn fail_context(&self, ctx: u32) {
        let gone: Vec<Arc<Fence>> = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<_> = p.keys().filter(|k| k.0 == ctx).copied().collect();
            keys.into_iter().flat_map(|k| p.remove(&k).unwrap_or_default().into_values()).collect()
        };
        for f in gone {
            f.fail();
        }
    }
}

extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {}

/// A context's fence `fence_id` on `ring_idx` retired: so did every
/// earlier one on that ring (fences retire in creation order per ring).
/// `u64::MAX` says the context is lost: its worker went, and the proxy is
/// about to retire its fences anyway (gpu/virglrenderer-macos-vtest.patch).
extern "C" fn write_context_fence(_cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64) {
    let Ok(gpu) = gpu_started() else { return };
    if fence_id == u64::MAX {
        if gpu.lost.lock().unwrap_or_else(|e| e.into_inner()).insert(ctx_id) {
            eprintln!("macwl-run: GPU context {ctx_id} lost (its render worker went)");
        }
        gpu.fail_context(ctx_id);
        return;
    }
    let done: Vec<Arc<Fence>> = {
        let mut p = gpu.pending.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ring) = p.get_mut(&(ctx_id, ring_idx)) else { return };
        let later = ring.split_off(&(fence_id + 1));
        std::mem::replace(ring, later).into_values().collect()
    };
    for f in done {
        f.signal();
    }
}

/// The GPU, only if it's been set up (the fence callback may run while
/// `gpu()`'s initialization is still in progress on another thread).
fn gpu_started() -> Result<&'static Gpu, i64> {
    // (`gpu()` holds the OnceLock while initializing; callbacks only come
    // once a context exists, after it's done.)
    gpu()
}

/// The render server, for the proxy: a socket pair's end, the other end
/// a `virgl_render_server` of our own (`--socket-fd 3`).
extern "C" fn get_server_fd(_cookie: *mut c_void, _version: u32) -> c_int {
    match spawn_server() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("macwl-run: no render server: {e}");
            -1
        }
    }
}

fn spawn_server() -> Result<c_int, String> {
    let dir = super::gpu_dir().ok_or("MACWL_GPU isn't set")?;
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
    let server = CString::new(dir.join("virgl_render_server").into_os_string().into_encoded_bytes()).map_err(|e| e.to_string())?;
    let argv = [server.as_ptr(), c"--socket-fd".as_ptr(), c"3".as_ptr(), std::ptr::null()];
    // Ours, plus where it finds itself again (it spawns a worker per
    // context from its own binary) and, as `MACWL_GPU_DYLD_LIBRARY_PATH`,
    // a library path (dyld drops DYLD_* from an entitled process's).
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| k != "RENDER_SERVER_EXEC_PATH" && k != "DYLD_LIBRARY_PATH")
        .filter_map(|(k, v)| CString::new([k.as_encoded_bytes(), b"=", v.as_encoded_bytes()].concat()).ok())
        .collect();
    env.push(CString::new([b"RENDER_SERVER_EXEC_PATH=", server.as_bytes()].concat()).map_err(|e| e.to_string())?);
    if let Some(path) = std::env::var_os("MACWL_GPU_DYLD_LIBRARY_PATH").or_else(|| std::env::var_os("DYLD_LIBRARY_PATH")) {
        env.push(CString::new([b"DYLD_LIBRARY_PATH=", path.as_encoded_bytes()].concat()).map_err(|e| e.to_string())?);
    }
    let mut envp: Vec<*const c_char> = env.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    let mut pid: libc::pid_t = 0;
    let r = unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawnattr_init(&mut attr);
        // Nothing of ours but stdio and its socket.
        libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short);
        for fd in 0..3 {
            libc::posix_spawn_file_actions_adddup2(&mut actions, fd, fd);
        }
        libc::posix_spawn_file_actions_adddup2(&mut actions, theirs.as_raw_fd(), 3);
        let r = libc::posix_spawn(&mut pid, server.as_ptr(), &actions, &attr, argv.as_ptr() as *const *mut c_char, envp.as_ptr() as *const *mut c_char);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        r
    };
    if r != 0 {
        return Err(format!("posix_spawn {}: {}", server.to_string_lossy(), std::io::Error::from_raw_os_error(r)));
    }
    // It goes when the socket does; reap it then.
    let _ = crate::host_threads::spawn("render server reaper", move || {
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
    });
    drop(theirs);
    let fd: OwnedFd = ours.into();
    Ok(std::os::fd::IntoRawFd::into_raw_fd(fd))
}

/// A blob, as virglrenderer has it, and its memory: an shm fd.
#[derive(Debug)]
pub struct Resource {
    id: u32,
    size: u64,
    blob_mem: u32,
    fd: OwnedFd,
}

impl Drop for Resource {
    fn drop(&mut self) {
        if let Ok(gpu) = gpu() {
            gpu.call(|l| unsafe { (l.resource_unref)(self.id) });
        }
    }
}

/// A node's Venus context.
#[derive(Debug, Clone)]
pub struct Context(Arc<ContextState>);

#[derive(Debug)]
pub struct ContextState {
    id: u32,
    next_fence: AtomicU64,
    /// GEM handles: blobs this context has, and where `MAP` put them.
    bos: Mutex<Bos>,
}

#[derive(Debug, Default)]
struct Bos {
    by_handle: HashMap<u32, Arc<Resource>>,
    next: u32,
}

impl Drop for ContextState {
    fn drop(&mut self) {
        if let Ok(gpu) = gpu() {
            let bos = std::mem::take(&mut self.bos.lock().unwrap_or_else(|e| e.into_inner()).by_handle);
            gpu.call(|l| unsafe {
                for r in bos.values() {
                    (l.ctx_detach_resource)(self.id as c_int, r.id as c_int);
                }
                (l.context_destroy)(self.id)
            });
            drop(bos);
            super::trace::report();
            gpu.fail_context(self.id);
            gpu.lost.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
        }
    }
}

impl Context {
    /// ENODEV once its worker has gone.
    fn alive(&self) -> Result<&Self, i64> {
        let lost = gpu().map(|g| g.lost.lock().unwrap_or_else(|e| e.into_inner()).contains(&self.0.id)).unwrap_or(true);
        if lost { Err(errno::ENODEV) } else { Ok(self) }
    }

    fn add(&self, r: Arc<Resource>) -> u32 {
        let mut bos = self.0.bos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((&h, _)) = bos.by_handle.iter().find(|(_, b)| Arc::ptr_eq(b, &r)) {
            return h;
        }
        bos.next += 1;
        let h = bos.next;
        bos.by_handle.insert(h, r);
        h
    }

    fn bo(&self, handle: u32) -> Result<Arc<Resource>, i64> {
        self.0.bos.lock().unwrap_or_else(|e| e.into_inner()).by_handle.get(&handle).cloned().ok_or(errno::ENOENT)
    }

    /// The blob `MAP` gave `offset` to, and the offset within it.
    pub fn mapping(&self, offset: u64) -> Result<(Arc<Resource>, u64), i64> {
        let r = self.bo((offset >> MAP_SHIFT) as u32).map_err(|_| errno::EINVAL)?;
        Ok((r, offset & ((1 << MAP_SHIFT) - 1)))
    }
}

/// `MAP`'s offsets: a blob's handle above this many bits.
const MAP_SHIFT: u32 = 36;

impl Resource {
    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

fn identity(fd: i32) -> Option<(u64, u64)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev as u64, st.st_ino))
}

fn page_round(n: u64) -> u64 {
    n.div_ceil(BLOB_ALIGNMENT) * BLOB_ALIGNMENT
}

/// The ioctls that need the GPU (`nr`: DRM's number; `size`: the guest's
/// struct size, which older uapis have smaller).
pub fn ioctl(node: &Arc<Node>, p: &Process, t: &Thread, mem: &dyn GuestMemory, nr: u8, size: usize, arg: u64) -> Result<i64, i64> {
    match nr {
        // GETPARAM { u64 param; u64 value (a pointer to an int) }
        0x43 => {
            let [param, value]: [u64; 2] = mem.read_pod(arg)?;
            let v: u32 = match param {
                1..=4 | 6 => 1,         // 3D features, capset fix, blobs, host-visible, context init
                5 => 0,                 // cross-device
                7 => 1 << CAPSET_VENUS, // supported capsets
                8 => 0,                 // explicit debug name
                9 => BLOB_ALIGNMENT as u32,
                _ => return Err(errno::EINVAL),
            };
            mem.write_pod(value, &v)?;
            Ok(0)
        }
        // GET_CAPS { u32 cap_set_id; u32 cap_set_ver; u64 addr; u32 size; u32 pad }
        0x49 => {
            let [id, _ver]: [u32; 2] = mem.read_pod(arg)?;
            let (addr, len) = (mem.read_pod::<u64>(arg + 8)?, mem.read_pod::<u32>(arg + 16)?);
            if id != CAPSET_VENUS {
                return Err(errno::EINVAL);
            }
            let gpu = gpu()?;
            let (mut max_ver, mut max_size) = (0u32, 0u32);
            let mut buf = gpu.call(|l| unsafe {
                (l.get_cap_set)(id, &mut max_ver, &mut max_size);
                let mut buf = vec![0u8; max_size as usize];
                (l.fill_caps)(id, 0, buf.as_mut_ptr().cast());
                buf
            });
            buf.truncate(len as usize);
            mem.write(addr, &buf)?;
            Ok(0)
        }
        // CONTEXT_INIT { u32 num_params; u32 pad; u64 ctx_set_params }
        0x4b => {
            let (n, params) = (mem.read_pod::<u32>(arg)?, mem.read_pod::<u64>(arg + 8)?);
            let mut capset = 0;
            let mut name = Vec::new();
            for i in 0..n.min(16) as u64 {
                let [param, value]: [u64; 2] = mem.read_pod(params + 16 * i)?;
                match param {
                    1 => capset = value as u32,
                    4 => {
                        name = vec![0u8; 64];
                        mem.read(value, &mut name)?;
                        name.truncate(name.iter().position(|&b| b == 0).unwrap_or(64));
                    }
                    _ => {}
                }
            }
            if capset != CAPSET_VENUS {
                return Err(errno::EINVAL);
            }
            let gpu = gpu()?;
            let id = gpu.next_ctx.fetch_add(1, Ordering::Relaxed);
            let r = gpu.call(|l| unsafe { (l.context_create_with_flags)(id, CAPSET_VENUS, name.len() as u32, name.as_ptr().cast()) });
            if r != 0 {
                return Err(errno::ENOMEM);
            }
            let ctx = Context(Arc::new(ContextState { id, next_fence: AtomicU64::new(1), bos: Mutex::default() }));
            node.set_context(ctx).map_err(|_| errno::EEXIST)?;
            Ok(0)
        }
        // RESOURCE_CREATE_BLOB { u32 blob_mem, blob_flags, bo_handle (out), res_handle (out);
        //   u64 size; u32 pad, cmd_size; u64 cmd; u64 blob_id }
        0x4a => {
            let ctx = node.context()?;
            ctx.alive()?;
            let gpu = gpu()?;
            let [blob_mem, blob_flags, _, _]: [u32; 4] = mem.read_pod(arg)?;
            let size = page_round(mem.read_pod::<u64>(arg + 16)?);
            let [_pad, cmd_size]: [u32; 2] = mem.read_pod(arg + 24)?;
            let (cmd, blob_id) = (mem.read_pod::<u64>(arg + 32)?, mem.read_pod::<u64>(arg + 40)?);
            let mut cmd_buf = vec![0u8; cmd_size as usize];
            if cmd_size > 0 {
                mem.read(cmd, &mut cmd_buf)?;
            }
            let res = gpu.next_res.fetch_add(1, Ordering::Relaxed);
            let (r, fd_type, fd) = gpu.call(|l| unsafe {
                // The inline command (waiting for the ring to make the
                // memory) goes first.
                if cmd_size > 0 && (l.submit_cmd)(cmd_buf.as_mut_ptr().cast(), ctx.0.id as c_int, (cmd_size / 4) as c_int) != 0 {
                    return (-1, 0, -1);
                }
                let args = CreateBlobArgs { res_handle: res, ctx_id: ctx.0.id, blob_mem, blob_flags, blob_id, size, iovecs: std::ptr::null(), num_iovs: 0 };
                if (l.resource_create_blob)(&args) != 0 {
                    return (-2, 0, -1);
                }
                let (mut fd_type, mut fd) = (0u32, -1 as c_int);
                if (l.resource_export_blob)(res, &mut fd_type, &mut fd) != 0 {
                    (l.resource_unref)(res);
                    return (-3, 0, -1);
                }
                (l.ctx_attach_resource)(ctx.0.id as c_int, res as c_int);
                (0, fd_type, fd)
            });
            if r != 0 || fd < 0 {
                return Err(errno::EINVAL);
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            if fd_type != FD_TYPE_SHM {
                return Err(errno::EOPNOTSUPP);
            }
            let resource = Arc::new(Resource { id: res, size, blob_mem, fd });
            let handle = ctx.add(resource);
            mem.write_pod(arg + 8, &[handle, res])?;
            Ok(0)
        }
        // RESOURCE_INFO { u32 bo_handle; u32 res_handle (out); u32 size (out); u32 blob_mem (out) }
        0x45 => {
            let handle: u32 = mem.read_pod(arg)?;
            let r = node.context()?.bo(handle)?;
            mem.write_pod(arg + 4, &[r.id, r.size.min(u32::MAX as u64) as u32, r.blob_mem])?;
            Ok(0)
        }
        // MAP { u64 offset (out); u32 handle; u32 pad }
        0x41 => {
            let handle: u32 = mem.read_pod(arg + 8)?;
            node.context()?.bo(handle)?;
            mem.write_pod(arg, &((handle as u64) << MAP_SHIFT))?;
            Ok(0)
        }
        // WAIT: blobs here are always idle to the CPU.
        0x48 => Ok(0),
        // GEM_CLOSE { u32 handle; u32 pad }
        0x09 => {
            let handle: u32 = mem.read_pod(arg)?;
            let ctx = node.context()?;
            let r = ctx.0.bos.lock().unwrap_or_else(|e| e.into_inner()).by_handle.remove(&handle).ok_or(errno::EINVAL)?;
            gpu()?.call(|l| unsafe { (l.ctx_detach_resource)(ctx.0.id as c_int, r.id as c_int) });
            Ok(0)
        }
        // PRIME_HANDLE_TO_FD { u32 handle; u32 flags; s32 fd (out) }
        0x2d => {
            let [handle, flags]: [u32; 2] = mem.read_pod(arg)?;
            let r = node.context()?.bo(handle)?;
            let dup = unsafe { libc::fcntl(r.fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if dup < 0 {
                return Err(crate::syscall::last_errno());
            }
            if let Some(id) = identity(dup) {
                gpu()?.exported.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Arc::downgrade(&r));
            }
            let cloexec = flags & 0o2000000 != 0;
            let fd = p.fds.insert(FdEntry::new(unsafe { OwnedFd::from_raw_fd(dup) }).cloexec(cloexec));
            mem.write_pod(arg + 8, &(fd as i32))?;
            Ok(0)
        }
        // PRIME_FD_TO_HANDLE { u32 handle (out); u32 flags; s32 fd }
        0x2e => {
            let fd: i32 = mem.read_pod(arg + 8)?;
            let host = p.fds.host(fd as u64)?;
            let ctx = node.context()?;
            let gpu = gpu()?;
            let known = identity(host).and_then(|id| gpu.exported.lock().unwrap_or_else(|e| e.into_inner()).get(&id).and_then(Weak::upgrade));
            let r = match known {
                Some(r) => r,
                None => {
                    // Another's shm: imported.
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    if unsafe { libc::fstat(host, &mut st) } != 0 {
                        return Err(crate::syscall::last_errno());
                    }
                    let dup = unsafe { libc::fcntl(host, libc::F_DUPFD_CLOEXEC, 3) };
                    if dup < 0 {
                        return Err(crate::syscall::last_errno());
                    }
                    let res = gpu.next_res.fetch_add(1, Ordering::Relaxed);
                    let size = st.st_size as u64;
                    let args = ImportBlobArgs { res_handle: res, blob_mem: BLOB_MEM_HOST3D, fd_type: FD_TYPE_SHM, fd: dup, size };
                    if gpu.call(|l| unsafe { (l.resource_import_blob)(&args) }) != 0 {
                        unsafe { libc::close(dup) };
                        return Err(errno::EINVAL);
                    }
                    let r = Arc::new(Resource { id: res, size, blob_mem: BLOB_MEM_HOST3D, fd: unsafe { OwnedFd::from_raw_fd(dup) } });
                    if let Some(id) = identity(host) {
                        gpu.exported.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Arc::downgrade(&r));
                    }
                    r
                }
            };
            let id = r.id;
            let handle = ctx.add(r);
            gpu.call(|l| unsafe { (l.ctx_attach_resource)(ctx.0.id as c_int, id as c_int) });
            mem.write_pod(arg, &handle)?;
            Ok(0)
        }
        // EXECBUFFER { u32 flags, size; u64 command, bo_handles; u32 num_bo_handles; s32 fence_fd;
        //   u32 ring_idx, syncobj_stride, num_in_syncobjs, num_out_syncobjs; u64 in_syncobjs, out_syncobjs }
        0x42 => execbuffer(node, p, t, mem, size, arg),
        _ => Err(errno::EINVAL),
    }
}

const EXECBUF_FENCE_FD_IN: u32 = 1;
const EXECBUF_FENCE_FD_OUT: u32 = 2;
const EXECBUF_RING_IDX: u32 = 4;
const EXECBUF_SYNCOBJ_RESET: u32 = 1;

fn execbuffer(node: &Arc<Node>, p: &Process, t: &Thread, mem: &dyn GuestMemory, size: usize, arg: u64) -> Result<i64, i64> {
    let ctx = node.context()?;
    ctx.alive()?;
    let gpu = gpu()?;
    let entered = super::trace::now();
    // The struct in one read (each read of guest memory walks its page
    // tables): older uapis' smaller ones, the rest zero.
    let mut raw = [0u8; 64];
    let n = size.clamp(28, 64);
    mem.read(arg, &mut raw[..n])?;
    let u32_at = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
    let (flags, len, command, fence_fd) = (u32_at(0), u32_at(4), u64_at(8), u32_at(28) as i32);
    let ring = if flags & EXECBUF_RING_IDX != 0 { u32_at(32) } else { 0 };
    let (stride, n_in, n_out, ins, outs) = (u32_at(36).max(16) as u64, u32_at(40), u32_at(44), u64_at(48), u64_at(56));
    // What it waits for first: a sync file, syncobjs' points (on the CPU:
    // Venus waits on the GPU through its ring, so these are rare).
    let mut waits = Vec::new();
    if flags & EXECBUF_FENCE_FD_IN != 0 {
        match sync::token_of(p.fds.host(fence_fd as u64)?) {
            Some(Token::SyncFile(f)) => waits.push(f),
            _ => return Err(errno::EINVAL),
        }
    }
    for i in 0..n_in.min(256) as u64 {
        let [handle, _flags]: [u32; 2] = mem.read_pod(ins + stride * i)?;
        let point: u64 = mem.read_pod(ins + stride * i + 8)?;
        let f = node_syncobj(node, handle)?.fence(point).ok_or(errno::EINVAL)?;
        waits.push(f);
    }
    if !waits.is_empty() {
        let all = Fence::all(&waits);
        sync::wait_until(p, t, None, |register| {
            register(&all);
            Ok(all.signaled())
        })?;
    }
    let mut cmd = vec![0u8; len as usize];
    mem.read(command, &mut cmd)?;
    super::trace::execbuffer(ctx.0.id, flags, len, ring, n_out, &cmd);
    let calling = super::trace::now();
    if gpu.call(|l| unsafe { (l.submit_cmd)(cmd.as_mut_ptr().cast(), ctx.0.id as c_int, (len / 4) as c_int) }) != 0 {
        return Err(errno::EINVAL);
    }
    let submitted = super::trace::now();
    if flags & EXECBUF_FENCE_FD_OUT == 0 && n_out == 0 {
        return Ok(0);
    }
    // A fence for the submission: pending from now, retired by the callback.
    let fence = Fence::new();
    super::trace::stamp_at(&fence, super::trace::Stage::Entered, entered);
    super::trace::stamp_at(&fence, super::trace::Stage::Calling, calling);
    super::trace::stamp_at(&fence, super::trace::Stage::Submitted, submitted);
    let id = ctx.0.next_fence.fetch_add(1, Ordering::Relaxed);
    gpu.pending.lock().unwrap_or_else(|e| e.into_inner()).entry((ctx.0.id, ring)).or_default().insert(id, fence.clone());
    let n_out = n_out.min(256) as u64;
    let mut out_entries = vec![0u8; (stride * n_out) as usize];
    mem.read(outs, &mut out_entries)?;
    for i in 0..n_out {
        let e = &out_entries[(stride * i) as usize..];
        let (handle, sflags) = (u32::from_le_bytes(e[0..4].try_into().unwrap()), u32::from_le_bytes(e[4..8].try_into().unwrap()));
        let point = u64::from_le_bytes(e[8..16].try_into().unwrap());
        let s = node_syncobj(node, handle)?;
        if sflags & EXECBUF_SYNCOBJ_RESET != 0 {
            s.reset();
        }
        s.attach(point, fence.clone());
    }
    if gpu.call(|l| unsafe { (l.context_create_fence)(ctx.0.id, 0, ring, id) }) != 0 {
        gpu.pending.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&(ctx.0.id, ring)).map(|r| r.remove(&id));
        fence.fail();
        return Err(errno::EINVAL);
    }
    super::trace::stamp(&fence, super::trace::Stage::Fenced);
    // Lost as it was made: no retire will come for it.
    if ctx.alive().is_err() {
        gpu.fail_context(ctx.0.id);
    }
    if flags & EXECBUF_FENCE_FD_OUT != 0 {
        let fd = p.fds.insert(FdEntry::new(sync::fd_for(Token::SyncFile(fence.clone()))?).cloexec(true));
        mem.write_pod(arg + 28, &(fd as i32))?;
    }
    super::trace::stamp(&fence, super::trace::Stage::Returned);
    Ok(0)
}

fn node_syncobj(node: &Node, handle: u32) -> Result<Arc<super::sync::Syncobj>, i64> {
    node.syncobj(handle)
}
