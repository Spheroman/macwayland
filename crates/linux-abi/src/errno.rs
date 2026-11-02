//! Linux errno values and translation from Darwin errno.
//!
//! The two agree for 1–34 except EAGAIN (Darwin 35, Linux 11; Darwin 11 is
//! EDEADLK). Above 34 they diverge completely, so every host error goes through
//! [`from_darwin`] before it is handed back to the guest.

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const ENXIO: i64 = 6;
pub const E2BIG: i64 = 7;
pub const ENOEXEC: i64 = 8;
pub const EBADF: i64 = 9;
pub const ECHILD: i64 = 10;
pub const EAGAIN: i64 = 11;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EBUSY: i64 = 16;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENODEV: i64 = 19;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENFILE: i64 = 23;
pub const EMFILE: i64 = 24;
pub const ENOTTY: i64 = 25;
pub const ETXTBSY: i64 = 26;
pub const EFBIG: i64 = 27;
pub const ENOSPC: i64 = 28;
pub const ESPIPE: i64 = 29;
pub const EROFS: i64 = 30;
pub const EMLINK: i64 = 31;
pub const EPIPE: i64 = 32;
pub const EDOM: i64 = 33;
pub const ERANGE: i64 = 34;
pub const EDEADLK: i64 = 35;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOLCK: i64 = 37;
pub const ENOSYS: i64 = 38;
pub const ENOTEMPTY: i64 = 39;
pub const ELOOP: i64 = 40;
pub const EOVERFLOW: i64 = 75;
pub const ENOTSOCK: i64 = 88;
pub const EDESTADDRREQ: i64 = 89;
pub const EMSGSIZE: i64 = 90;
pub const EPROTOTYPE: i64 = 91;
pub const ENOPROTOOPT: i64 = 92;
pub const EPROTONOSUPPORT: i64 = 93;
pub const EOPNOTSUPP: i64 = 95;
pub const EAFNOSUPPORT: i64 = 97;
pub const EADDRINUSE: i64 = 98;
pub const EADDRNOTAVAIL: i64 = 99;
pub const ENETDOWN: i64 = 100;
pub const ENETUNREACH: i64 = 101;
pub const ECONNABORTED: i64 = 103;
pub const ECONNRESET: i64 = 104;
pub const ENOBUFS: i64 = 105;
pub const EISCONN: i64 = 106;
pub const ENOTCONN: i64 = 107;
pub const ETIMEDOUT: i64 = 110;
pub const ECONNREFUSED: i64 = 111;
pub const EHOSTUNREACH: i64 = 113;
pub const EALREADY: i64 = 114;
pub const EINPROGRESS: i64 = 115;
pub const ECANCELED: i64 = 125;

/// Not errnos the guest ever sees: what a call that would block tells the
/// core instead, on a CPU that's wanted for another thread (see
/// `syscall::block`). PARK: the thread waits off its CPU for what
/// `Thread::take_park` says, and the call is made again once that comes.
/// OFFLOAD: the wait can't be watched that way (a file kqueue can't
/// watch); a worker makes the call. (Linux keeps 512 and up for its own
/// such values, ERESTARTSYS and friends.)
pub const PARK: i64 = 600;
pub const OFFLOAD: i64 = 601;

/// Translate a Darwin errno (as from `io::Error::raw_os_error`) to Linux.
pub fn from_darwin(e: i32) -> i64 {
    match e {
        // Identical numbering.
        libc::EPERM..=libc::ERANGE if e != libc::EDEADLK => e as i64,
        libc::EDEADLK => EDEADLK,
        libc::EAGAIN => EAGAIN,
        libc::EINPROGRESS => EINPROGRESS,
        libc::EALREADY => EALREADY,
        libc::ENOTSOCK => ENOTSOCK,
        libc::EDESTADDRREQ => EDESTADDRREQ,
        libc::EMSGSIZE => EMSGSIZE,
        libc::EPROTOTYPE => EPROTOTYPE,
        libc::ENOPROTOOPT => ENOPROTOOPT,
        libc::EPROTONOSUPPORT => EPROTONOSUPPORT,
        libc::ENOTSUP | libc::EOPNOTSUPP => EOPNOTSUPP,
        libc::EAFNOSUPPORT => EAFNOSUPPORT,
        libc::EADDRINUSE => EADDRINUSE,
        libc::EADDRNOTAVAIL => EADDRNOTAVAIL,
        libc::ENETDOWN => ENETDOWN,
        libc::ENETUNREACH => ENETUNREACH,
        libc::ECONNABORTED => ECONNABORTED,
        libc::ECONNRESET => ECONNRESET,
        libc::ENOBUFS => ENOBUFS,
        libc::EISCONN => EISCONN,
        libc::ENOTCONN => ENOTCONN,
        libc::ETIMEDOUT => ETIMEDOUT,
        libc::ECONNREFUSED => ECONNREFUSED,
        libc::ELOOP => ELOOP,
        libc::ENAMETOOLONG => ENAMETOOLONG,
        libc::EHOSTUNREACH => EHOSTUNREACH,
        libc::ENOTEMPTY => ENOTEMPTY,
        libc::ENOLCK => ENOLCK,
        libc::ENOSYS => ENOSYS,
        libc::EOVERFLOW => EOVERFLOW,
        libc::ECANCELED => ECANCELED,
        _ => EIO,
    }
}

/// Linux errno for an `io::Error` from a host call.
pub fn from_io(err: &std::io::Error) -> i64 {
    err.raw_os_error().map(from_darwin).unwrap_or(EIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_numbers_pass_through() {
        assert_eq!(from_darwin(libc::ENOENT), ENOENT);
        assert_eq!(from_darwin(libc::EBADF), EBADF);
        assert_eq!(from_darwin(libc::ERANGE), ERANGE);
    }

    #[test]
    fn eagain_and_edeadlk_swap() {
        assert_eq!(libc::EAGAIN, 35);
        assert_eq!(from_darwin(libc::EAGAIN), EAGAIN);
        assert_eq!(libc::EDEADLK, 11);
        assert_eq!(from_darwin(libc::EDEADLK), EDEADLK);
    }

    #[test]
    fn high_numbers_translate() {
        assert_eq!(from_darwin(libc::ENOSYS), 38);
        assert_eq!(from_darwin(libc::ECONNREFUSED), 111);
        assert_eq!(from_darwin(libc::ENOTEMPTY), 39);
    }
}
