//! A Unix socket a process serves (a host's, `--serve`; the coordinator's),
//! the user's alone, with a lock beside it (PATH.lock) so one process
//! serves a path at a time.

use std::{
    io::ErrorKind,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{fs::DirBuilderExt, net::UnixListener},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The socket a process serves, and the lock that makes it its.
pub(crate) struct Endpoint {
    path: PathBuf,
    pub listener: Option<UnixListener>,
    lock: Option<OwnedFd>,
}

impl Endpoint {
    pub fn claim(path: &Path) -> std::io::Result<Endpoint> {
        let me = unsafe { libc::getuid() };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            // Directories the host makes are the user's alone.
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(dir)?.uid() != me {
                return Err(std::io::Error::new(ErrorKind::PermissionDenied, format!("{} isn't yours", dir.display())));
            }
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path)?;
        // A host that's just going still holds it for a moment.
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if Instant::now() > deadline {
                return Err(std::io::Error::new(ErrorKind::AddrInUse, format!("a host already serves {}", path.display())));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Holding the lock, whatever is at the path is a host's that has
        // gone.
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Endpoint { path: path.to_owned(), listener: Some(listener), lock: Some(lock.into()) })
    }

    /// No more clients: the path goes, then the lock, so that a new host
    /// can serve the path at once.
    pub fn close(&mut self) {
        if self.listener.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
        self.lock = None;
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.close();
    }
}

