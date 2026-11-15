//! The GPU: a virtio-gpu render node (docs/gpu.md, step 2). Mesa's Venus
//! driver in the guest talks to `/dev/dri/renderD128` as to Linux's
//! virtio-gpu driver; the node's ioctls are carried out with
//! virglrenderer (`virgl`), whose render server runs Venus contexts on
//! KosmicKrisp in Mac processes of their own.
//!
//! On while `MACWL_GPU` (in macwl-run's own environment) names a directory
//! holding `libvirglrenderer.1.dylib` and `virgl_render_server`; the
//! render server finds its Vulkan driver as any Vulkan program does
//! (`VK_ICD_FILENAMES`, also from macwl-run's environment).

pub mod vfs;

use std::{
    os::fd::{FromRawFd, OwnedFd},
    path::PathBuf,
    sync::OnceLock,
};

use crate::{
    errno,
    fs::{DirEntry, DirSnapshot, FdEntry},
};

/// Where virglrenderer is, if the GPU is on.
pub fn gpu_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("MACWL_GPU")?);
        (dir.join("libvirglrenderer.1.dylib").exists() && dir.join("virgl_render_server").exists()).then_some(dir)
    })
    .as_ref()
}

/// Whether the guest has a GPU.
pub fn enabled() -> bool {
    gpu_dir().is_some()
}

/// `stat` of a path in the tree (as resolved, `Target::Dev`).
pub fn stat(path: &[u8]) -> Result<libc::stat, i64> {
    Ok(vfs::stat(vfs::lookup(path).ok_or(errno::ENOENT)?))
}

pub fn readlink(path: &[u8]) -> Result<Vec<u8>, i64> {
    match vfs::lookup(path).ok_or(errno::ENOENT)? {
        vfs::Node::Link(target) => Ok(target.as_bytes().to_vec()),
        _ => Err(errno::EINVAL),
    }
}

pub fn exists(path: &[u8]) -> Result<(), i64> {
    vfs::lookup(path).map(|_| ()).ok_or(errno::ENOENT)
}

/// Open a path in the tree: a directory (its listing), a file (its
/// contents), or the render node.
pub fn open(path: &[u8], guest: Vec<u8>) -> Result<FdEntry, i64> {
    match vfs::lookup(path).ok_or(errno::ENOENT)? {
        vfs::Node::Dir(children) => {
            let mut entries = vec![DirEntry { ino: 1, kind: libc::DT_DIR, name: b".".to_vec() }, DirEntry { ino: 1, kind: libc::DT_DIR, name: b"..".to_vec() }];
            entries.extend(children.iter().enumerate().map(|(i, (name, kind))| DirEntry { ino: 2 + i as u64, kind: *kind, name: name.as_bytes().to_vec() }));
            Ok(crate::proc::generated(b"")?.path(guest).dir(DirSnapshot { entries, next: 0, fixed: true }))
        }
        vfs::Node::File(contents) => Ok(crate::proc::generated(contents)?.path(guest)),
        vfs::Node::Link(_) => Err(errno::ELOOP),
        vfs::Node::Render => {
            // A host fd for the node to stand on (nothing reads it): the
            // node's state is the entry's kind.
            let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(crate::syscall::last_errno());
            }
            let mut entry = FdEntry::new(unsafe { OwnedFd::from_raw_fd(fd) }).path(guest);
            entry.kind = crate::fs::FileKind::Drm(node::Node::new());
            Ok(entry)
        }
    }
}

pub mod node;
pub mod sync;
mod trace;
mod virgl;

/// The blob `offset` (from `VIRTGPU_MAP`) of node `node` names: its host
/// shm fd, and the offset in it, for `mmap`.
pub fn mapping(node: &node::Node, offset: u64) -> Result<(i32, u64, std::sync::Arc<dyn std::any::Any + Send + Sync>), i64> {
    let (r, inner) = node.context()?.mapping(offset)?;
    Ok((r.fd(), inner, r))
}
