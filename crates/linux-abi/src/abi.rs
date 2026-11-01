//! Linux arm64 userspace ABI: struct layouts and flag values the guest sees.

/// `struct stat` for arm64 (asm-generic/stat.h), 128 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: u64,
    pub st_mtime: i64,
    pub st_mtime_nsec: u64,
    pub st_ctime: i64,
    pub st_ctime_nsec: u64,
    pub __unused4: u32,
    pub __unused5: u32,
}

impl Stat {
    /// Convert from the host's `struct stat`. File-type and permission bits
    /// (S_IFMT and friends) have the same values on Linux and Darwin.
    pub fn from_darwin(s: &libc::stat) -> Self {
        Stat {
            st_dev: encode_dev(s.st_dev),
            st_ino: s.st_ino,
            st_mode: s.st_mode as u32,
            st_nlink: s.st_nlink as u32,
            st_uid: s.st_uid,
            st_gid: s.st_gid,
            st_rdev: encode_dev(s.st_rdev),
            st_size: s.st_size,
            st_blksize: s.st_blksize,
            st_blocks: s.st_blocks,
            st_atime: s.st_atime,
            st_atime_nsec: s.st_atime_nsec as u64,
            st_mtime: s.st_mtime,
            st_mtime_nsec: s.st_mtime_nsec as u64,
            st_ctime: s.st_ctime,
            st_ctime_nsec: s.st_ctime_nsec as u64,
            ..Default::default()
        }
    }
}

/// A Darwin `dev_t` (8-bit major, 24-bit minor) in Linux's encoding, so
/// device numbers look right (e.g. /dev/null as 1:3 would, if Darwin agreed).
fn encode_dev(d: libc::dev_t) -> u64 {
    let d = d as u32 as u64;
    let (major, minor) = ((d >> 24) & 0xff, d & 0xff_ffff);
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// `struct timespec` on arm64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// `struct iovec` on arm64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Iovec {
    pub base: u64,
    pub len: u64,
}

/// `struct utsname`: six NUL-padded 65-byte fields.
pub const UTS_FIELD: usize = 65;
pub const UTSNAME_SIZE: usize = UTS_FIELD * 6;

/// openat() flags as numbered on arm64 Linux (some differ from x86-64).
pub mod o {
    pub const ACCMODE: u64 = 0o3;
    pub const CREAT: u64 = 0o100;
    pub const EXCL: u64 = 0o200;
    pub const NOCTTY: u64 = 0o400;
    pub const TRUNC: u64 = 0o1000;
    pub const APPEND: u64 = 0o2000;
    pub const NONBLOCK: u64 = 0o4000;
    pub const DIRECTORY: u64 = 0o40000;
    pub const NOFOLLOW: u64 = 0o100000;
    pub const DIRECT: u64 = 0o200000;
    pub const LARGEFILE: u64 = 0o400000;
    pub const CLOEXEC: u64 = 0o2000000;
    pub const PATH: u64 = 0o10000000;
}

/// Translate Linux open flags to Darwin's. Unknown/irrelevant bits (LARGEFILE,
/// DIRECT) are dropped; O_PATH becomes a read-only open for now.
pub fn open_flags_to_darwin(flags: u64) -> libc::c_int {
    let mut out = match flags & o::ACCMODE {
        0 => libc::O_RDONLY,
        1 => libc::O_WRONLY,
        _ => libc::O_RDWR,
    };
    for (linux, darwin) in [
        (o::CREAT, libc::O_CREAT),
        (o::EXCL, libc::O_EXCL),
        (o::NOCTTY, libc::O_NOCTTY),
        (o::TRUNC, libc::O_TRUNC),
        (o::APPEND, libc::O_APPEND),
        (o::NONBLOCK, libc::O_NONBLOCK),
        (o::DIRECTORY, libc::O_DIRECTORY),
        (o::NOFOLLOW, libc::O_NOFOLLOW),
        (o::CLOEXEC, libc::O_CLOEXEC),
    ] {
        if flags & linux != 0 {
            out |= darwin;
        }
    }
    out
}

/// Translate Darwin open-file flags (from `F_GETFL`) back to Linux's.
pub fn open_flags_from_darwin(flags: libc::c_int) -> u64 {
    let mut out = match flags & libc::O_ACCMODE {
        libc::O_RDONLY => 0,
        libc::O_WRONLY => 1,
        _ => 2,
    };
    for (linux, darwin) in [(o::APPEND, libc::O_APPEND), (o::NONBLOCK, libc::O_NONBLOCK)] {
        if flags & darwin != 0 {
            out |= linux;
        }
    }
    out | o::LARGEFILE // always set on 64-bit Linux
}

/// `AT_*` values (Linux numbering).
pub mod at {
    pub const FDCWD: i64 = -100;
    pub const SYMLINK_NOFOLLOW: u64 = 0x100;
    pub const REMOVEDIR: u64 = 0x200;
    /// faccessat's flag; shares REMOVEDIR's value.
    pub const EACCESS: u64 = 0x200;
    pub const SYMLINK_FOLLOW: u64 = 0x400;
    pub const NO_AUTOMOUNT: u64 = 0x800;
    pub const EMPTY_PATH: u64 = 0x1000;
}

/// `fcntl` commands and `utimensat` / `renameat2` / `memfd_create` flags.
pub mod fcntl {
    pub const DUPFD: u64 = 0;
    pub const GETFD: u64 = 1;
    pub const SETFD: u64 = 2;
    pub const GETFL: u64 = 3;
    pub const SETFL: u64 = 4;
    pub const GETLK: u64 = 5;
    pub const SETLK: u64 = 6;
    pub const SETLKW: u64 = 7;
    pub const OFD_GETLK: u64 = 36;
    pub const OFD_SETLK: u64 = 37;
    pub const OFD_SETLKW: u64 = 38;
    pub const DUPFD_CLOEXEC: u64 = 1030;
    pub const ADD_SEALS: u64 = 1033;
    pub const GET_SEALS: u64 = 1034;
    pub const FD_CLOEXEC: u64 = 1;

    pub const UTIME_NOW: i64 = (1 << 30) - 1;
    pub const UTIME_OMIT: i64 = (1 << 30) - 2;

    pub const RENAME_NOREPLACE: u64 = 1;
    pub const RENAME_EXCHANGE: u64 = 2;

    pub const MFD_CLOEXEC: u64 = 1;

    pub const SEEK_DATA: u64 = 3;
    pub const SEEK_HOLE: u64 = 4;
}

/// `ioctl` requests (asm-generic/ioctls.h).
pub mod ioctl {
    /// Share another file's blocks (reflink).
    pub const FICLONE: u64 = 0x4004_9409;
    pub const FICLONERANGE: u64 = 0x4020_940d;
    pub const TIOCGPGRP: u64 = 0x540f;
    pub const TIOCSPGRP: u64 = 0x5410;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const FIONREAD: u64 = 0x541b;
    pub const FIONBIO: u64 = 0x5421;
    pub const FIONCLEX: u64 = 0x5450;
    pub const FIOCLEX: u64 = 0x5451;
}

/// `struct pollfd` (same layout and event bits on Darwin, except POLLRDHUP).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub const POLLRDHUP: i16 = 0x2000;

/// `struct linux_dirent64` header; the NUL-terminated name follows, and each
/// record is padded to 8 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Dirent64 {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
}

/// Size of the fixed part of `linux_dirent64` (before `d_name`).
pub const DIRENT64_NAME_OFFSET: usize = 19;

/// Sockets: address families, types, message flags, options (Linux numbering).
pub mod net {
    pub const AF_UNIX: u16 = 1;
    pub const AF_INET: u16 = 2;
    pub const AF_INET6: u16 = 10;

    pub const SOCK_TYPE_MASK: u64 = 0xf;
    pub const SOCK_NONBLOCK: u64 = 0o4000;
    pub const SOCK_CLOEXEC: u64 = 0o2000000;

    pub const MSG_OOB: u64 = 0x1;
    pub const MSG_PEEK: u64 = 0x2;
    pub const MSG_DONTROUTE: u64 = 0x4;
    pub const MSG_CTRUNC: u64 = 0x8;
    pub const MSG_TRUNC: u64 = 0x20;
    pub const MSG_DONTWAIT: u64 = 0x40;
    pub const MSG_EOR: u64 = 0x80;
    pub const MSG_WAITALL: u64 = 0x100;
    pub const MSG_NOSIGNAL: u64 = 0x4000;
    pub const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;

    pub const SOL_SOCKET: i32 = 1;
    pub const SCM_RIGHTS: i32 = 1;

    pub const SO_REUSEADDR: u64 = 2;
    pub const SO_TYPE: u64 = 3;
    pub const SO_ERROR: u64 = 4;
    pub const SO_BROADCAST: u64 = 6;
    pub const SO_SNDBUF: u64 = 7;
    pub const SO_RCVBUF: u64 = 8;
    pub const SO_KEEPALIVE: u64 = 9;
    pub const SO_PASSCRED: u64 = 16;
    pub const SO_PEERCRED: u64 = 17;
    pub const SO_RCVTIMEO: u64 = 20;
    pub const SO_SNDTIMEO: u64 = 21;

    pub const IPPROTO_IP: u64 = 0;
    pub const IPPROTO_TCP: u64 = 6;
    pub const IPPROTO_IPV6: u64 = 41;
    pub const IP_RECVERR: u64 = 11;
    pub const IPV6_RECVERR: u64 = 25;
    pub const TCP_NODELAY: u64 = 1;
    pub const IPV6_V6ONLY: u64 = 26;

    /// `struct msghdr` on arm64.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct MsgHdr {
        pub name: u64,
        pub namelen: u32,
        pub _pad1: u32,
        pub iov: u64,
        pub iovlen: u64,
        pub control: u64,
        pub controllen: u64,
        pub flags: i32,
        pub _pad2: u32,
    }

    /// `struct cmsghdr` on arm64: data follows at 16 bytes, records are padded
    /// to 8 bytes. (Darwin's is 12 bytes with 4-byte padding.)
    pub const CMSG_HDR: u64 = 16;

    /// `struct ucred`, for SO_PEERCRED.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Ucred {
        pub pid: i32,
        pub uid: u32,
        pub gid: u32,
    }
}

/// Clock ids.
pub mod clock {
    pub const REALTIME: u64 = 0;
    pub const MONOTONIC: u64 = 1;
    pub const PROCESS_CPUTIME_ID: u64 = 2;
    pub const THREAD_CPUTIME_ID: u64 = 3;
    pub const MONOTONIC_RAW: u64 = 4;
    pub const REALTIME_COARSE: u64 = 5;
    pub const MONOTONIC_COARSE: u64 = 6;
    pub const BOOTTIME: u64 = 7;
}

/// `mmap` flags (asm-generic/mman-common.h; arm64 uses the generic values).
pub mod mm {
    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;

    pub const MAP_SHARED: u64 = 0x01;
    pub const MAP_PRIVATE: u64 = 0x02;
    pub const MAP_SHARED_VALIDATE: u64 = 0x03;
    pub const MAP_TYPE: u64 = 0x0f;
    pub const MAP_FIXED: u64 = 0x10;
    pub const MAP_ANONYMOUS: u64 = 0x20;
    pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
}

/// Auxiliary vector keys (`include/uapi/linux/auxvec.h`), passed to a new
/// program on its initial stack after envp.
pub mod auxv {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const HWCAP: u64 = 16;
    pub const CLKTCK: u64 = 17;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const EXECFN: u64 = 31;

    /// arm64 `AT_HWCAP` bits (asm/hwcap.h).
    pub const HWCAP_FP: u64 = 1 << 0;
    pub const HWCAP_ASIMD: u64 = 1 << 1;
}

/// View a plain-old-data struct as bytes for copying into guest memory.
///
/// # Safety
/// `T` must be `repr(C)` with no padding the guest could misread as data.
pub unsafe fn as_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_is_128_bytes() {
        assert_eq!(std::mem::size_of::<Stat>(), 128);
        assert_eq!(std::mem::offset_of!(Stat, st_size), 48);
        assert_eq!(std::mem::offset_of!(Stat, st_mtime), 88);
    }

    #[test]
    fn open_flags() {
        let f = open_flags_to_darwin(2 | o::CREAT | o::TRUNC | o::CLOEXEC);
        assert_eq!(f & libc::O_ACCMODE, libc::O_RDWR);
        assert_ne!(f & libc::O_CREAT, 0);
        assert_ne!(f & libc::O_TRUNC, 0);
        assert_ne!(f & libc::O_CLOEXEC, 0);
        // arm64's O_DIRECTORY (0o40000) is not Darwin's.
        assert_ne!(o::DIRECTORY as i32, libc::O_DIRECTORY);
        assert_ne!(open_flags_to_darwin(o::DIRECTORY) & libc::O_DIRECTORY, 0);
    }
}
