//! What a `macwl-run` invocation asks to run: the program, its arguments
//! and environment, the guest's view of the filesystem (root and binds),
//! its working directory and umask. Made where `macwl-run` was started (from
//! that process's environment, directory and options) and turned into a
//! process there, or by the host a client sends it to.

use std::{ffi::OsStr, os::{fd::OwnedFd, unix::ffi::OsStrExt}, path::{Path, PathBuf}};

use linux_abi::{
    fs::{FdEntry, Resolved, Target},
    wire::{Decoder, Encoder, Truncated},
    Process,
};

#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// The host directory the guest sees as `/`.
    pub root: PathBuf,
    /// Host directories (real paths) bound at guest paths.
    pub binds: Vec<(PathBuf, Vec<u8>)>,
    /// Where the guest starts, if it can see the directory it was run from.
    pub cwd: Option<Vec<u8>>,
    pub umask: u32,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
    /// Linux signals the program starts out ignoring, and blocking (bit
    /// n-1 for signal n), as it would inherit them across `execve` from
    /// whatever started `macwl-run`. A client sends them; a `macwl-run`
    /// that is the host leaves them empty, as before.
    pub ignored: u64,
    pub blocked: u64,
}

impl Launch {
    pub fn encode(&self, e: &mut Encoder) {
        e.bytes(self.root.as_os_str().as_bytes());
        e.u64(self.binds.len() as u64);
        for (host, guest) in &self.binds {
            e.bytes(host.as_os_str().as_bytes()).bytes(guest);
        }
        e.opt_bytes(self.cwd.as_deref()).u64(self.umask as u64);
        for list in [&self.argv, &self.env] {
            e.u64(list.len() as u64);
            for item in list {
                e.bytes(item);
            }
        }
        e.u64(self.ignored).u64(self.blocked);
    }

    pub fn decode(d: &mut Decoder) -> Result<Launch, Truncated> {
        let path = |b: Vec<u8>| PathBuf::from(OsStr::from_bytes(&b));
        let root = path(d.bytes()?);
        let mut binds = Vec::new();
        for _ in 0..d.count(16)? {
            binds.push((path(d.bytes()?), d.bytes()?));
        }
        let cwd = d.opt_bytes()?;
        let umask = d.u64()? as u32 & 0o777;
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            for _ in 0..d.count(8)? {
                list.push(d.bytes()?);
            }
        }
        let [argv, env] = lists;
        if argv.is_empty() {
            return Err(Truncated);
        }
        let (ignored, blocked) = (d.u64()?, d.u64()?);
        Ok(Launch { root, binds, cwd, umask, argv, env, ignored, blocked })
    }

    /// A process for it, with `stdio` as its fds (guest number, host fd),
    /// and its program's contents. Its umask is left for the caller to set
    /// once the process has its kernel (a host's must never reach the host
    /// process's own umask).
    pub fn process(&self, stdio: Vec<(usize, OwnedFd)>) -> Result<(Process, Vec<u8>), String> {
        let mut process = Process::new(&self.root);
        for (fd, host) in stdio {
            process.fds.insert_at(fd, FdEntry::new(host));
        }
        for (host, guest) in &self.binds {
            process.fs.bind(guest, host.clone()).map_err(|e| format!("--bind {}: {e}", host.display()))?;
        }
        if let Some(cwd) = &self.cwd {
            process.fs.set_cwd(cwd.clone());
        }
        let path = OsStr::from_bytes(&self.argv[0]);
        let (elf, exe) = read_program(&process, path)?;
        process.set_comm(self.argv[0].rsplit(|&b| b == b'/').next().unwrap_or_default());
        process.set_exe(exe);
        Ok((process, elf))
    }
}

/// The program, as a guest path: its contents, and its guest path with
/// symlinks resolved.
pub(crate) fn read_program(process: &Process, path: &OsStr) -> Result<(Vec<u8>, Vec<u8>), String> {
    const AT_FDCWD: u64 = -100i64 as u64;
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let host = match process.fs.resolve(&process.fds, AT_FDCWD, path.as_bytes(), true) {
        Ok(Resolved { target: Target::Host(host), .. }) => host,
        Ok(_) => return Err(err(std::io::ErrorKind::Unsupported.into())),
        Err(e) => return Err(err(std::io::Error::from_raw_os_error(e as i32))),
    };
    let host = Path::new(OsStr::from_bytes(host.as_bytes()));
    let elf = std::fs::read(host).map_err(err)?;
    // /proc/self/exe: the guest's name for the file, symlinks resolved.
    let canonical = std::fs::canonicalize(host).map_err(err)?;
    let exe = process.fs.guest_path_of_host(canonical.as_os_str().as_bytes());
    Ok((elf, exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let l = Launch {
            root: "/r".into(),
            binds: vec![("/h".into(), b"/g".to_vec())],
            cwd: Some(b"/w".to_vec()),
            umask: 0o027,
            argv: vec![b"/bin/true".to_vec(), b"x".to_vec()],
            env: vec![b"A=1".to_vec()],
            ignored: 1 << 1,
            blocked: 1 << 9,
        };
        let mut e = Encoder::default();
        l.encode(&mut e);
        let back = Launch::decode(&mut Decoder(&e.0)).unwrap();
        assert_eq!(
            (back.root, back.binds, back.cwd, back.umask, back.argv, back.env, back.ignored, back.blocked),
            (l.root, l.binds, l.cwd, l.umask, l.argv, l.env, l.ignored, l.blocked)
        );
        assert!(Launch::decode(&mut Decoder(&e.0[..e.0.len() - 1])).is_err());
    }
}
