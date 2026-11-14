//! Linux arm64 system call numbers.
//!
//! arm64 uses the "generic" table from `include/uapi/asm-generic/unistd.h`, which
//! differs from x86-64 (and so from Noah): there is no `open`, `stat` or `fork`,
//! only `openat`, `newfstatat` and `clone`.

pub const SETXATTR: u64 = 5;
pub const FREMOVEXATTR: u64 = 16;
pub const GETCWD: u64 = 17;
pub const EVENTFD2: u64 = 19;
pub const EPOLL_CREATE1: u64 = 20;
pub const EPOLL_CTL: u64 = 21;
pub const EPOLL_PWAIT: u64 = 22;
pub const DUP: u64 = 23;
pub const DUP3: u64 = 24;
pub const FCNTL: u64 = 25;
pub const INOTIFY_INIT1: u64 = 26;
pub const INOTIFY_ADD_WATCH: u64 = 27;
pub const INOTIFY_RM_WATCH: u64 = 28;
pub const IOCTL: u64 = 29;
pub const FLOCK: u64 = 32;
pub const MKDIRAT: u64 = 34;
pub const UNLINKAT: u64 = 35;
pub const FACCESSAT: u64 = 48;
pub const CHDIR: u64 = 49;
pub const OPENAT: u64 = 56;
pub const CLOSE: u64 = 57;
pub const PIPE2: u64 = 59;
pub const GETDENTS64: u64 = 61;
pub const LSEEK: u64 = 62;
pub const READ: u64 = 63;
pub const WRITE: u64 = 64;
pub const READV: u64 = 65;
pub const WRITEV: u64 = 66;
pub const PREAD64: u64 = 67;
pub const PWRITE64: u64 = 68;
pub const READLINKAT: u64 = 78;
pub const NEWFSTATAT: u64 = 79;
pub const FSTAT: u64 = 80;
pub const EXIT: u64 = 93;
pub const EXIT_GROUP: u64 = 94;
pub const SET_TID_ADDRESS: u64 = 96;
pub const FUTEX: u64 = 98;
pub const SET_ROBUST_LIST: u64 = 99;
pub const GET_ROBUST_LIST: u64 = 100;
pub const NANOSLEEP: u64 = 101;
pub const CLOCK_GETTIME: u64 = 113;
pub const CLOCK_GETRES: u64 = 114;
pub const SCHED_GETSCHEDULER: u64 = 120;
pub const SCHED_GETPARAM: u64 = 121;
pub const SCHED_GETAFFINITY: u64 = 123;
pub const SCHED_GET_PRIORITY_MAX: u64 = 125;
pub const SCHED_GET_PRIORITY_MIN: u64 = 126;
pub const SETPRIORITY: u64 = 140;
pub const GETPRIORITY: u64 = 141;
pub const SCHED_SETAFFINITY: u64 = 122;
pub const USERFAULTFD: u64 = 282;
pub const READAHEAD: u64 = 213;
pub const SCHED_YIELD: u64 = 124;
pub const GETITIMER: u64 = 102;
pub const SETITIMER: u64 = 103;
pub const TIMER_CREATE: u64 = 107;
pub const TIMER_GETTIME: u64 = 108;
pub const TIMER_GETOVERRUN: u64 = 109;
pub const TIMER_SETTIME: u64 = 110;
pub const TIMER_DELETE: u64 = 111;
pub const KILL: u64 = 129;
pub const TGKILL: u64 = 131;
pub const RT_SIGSUSPEND: u64 = 133;
pub const RT_SIGACTION: u64 = 134;
pub const RT_SIGPROCMASK: u64 = 135;
pub const RT_SIGPENDING: u64 = 136;
pub const RT_SIGTIMEDWAIT: u64 = 137;
pub const RT_SIGRETURN: u64 = 139;
pub const SETREGID: u64 = 143;
pub const SETGID: u64 = 144;
pub const SETREUID: u64 = 145;
pub const SETUID: u64 = 146;
pub const SETRESUID: u64 = 147;
pub const GETRESUID: u64 = 148;
pub const SETRESGID: u64 = 149;
pub const GETRESGID: u64 = 150;
pub const SETPGID: u64 = 154;
pub const GETPGID: u64 = 155;
pub const GETSID: u64 = 156;
pub const SETSID: u64 = 157;
pub const GETGROUPS: u64 = 158;
pub const UNAME: u64 = 160;
pub const GETPID: u64 = 172;
pub const GETPPID: u64 = 173;
pub const GETUID: u64 = 174;
pub const GETEUID: u64 = 175;
pub const GETGID: u64 = 176;
pub const GETEGID: u64 = 177;
pub const GETTID: u64 = 178;
pub const SOCKET: u64 = 198;
pub const CONNECT: u64 = 203;
pub const SENDMSG: u64 = 211;
pub const RECVMSG: u64 = 212;
pub const RECVMMSG: u64 = 243;
pub const SENDMMSG: u64 = 269;
pub const BRK: u64 = 214;
pub const MUNMAP: u64 = 215;
pub const MMAP: u64 = 222;
pub const MPROTECT: u64 = 226;
pub const PRLIMIT64: u64 = 261;
pub const GETRANDOM: u64 = 278;
pub const WAITID: u64 = 95;
pub const GETRUSAGE: u64 = 165;
pub const PRCTL: u64 = 167;
pub const CLONE: u64 = 220;
pub const EXECVE: u64 = 221;
pub const WAIT4: u64 = 260;
pub const CLONE3: u64 = 435;
pub const CLOSE_RANGE: u64 = 436;
pub const EPOLL_PWAIT2: u64 = 441;
pub const FUTEX_WAITV: u64 = 449;
pub const FUTEX_WAKE: u64 = 454;
pub const FUTEX_WAIT: u64 = 455;
pub const FUTEX_REQUEUE: u64 = 456;
pub const FADVISE64: u64 = 223;
pub const STATX: u64 = 291;
pub const RSEQ: u64 = 293;

pub const SYMLINKAT: u64 = 36;
pub const LINKAT: u64 = 37;
pub const RENAMEAT: u64 = 38;
pub const STATFS: u64 = 43;
pub const FSTATFS: u64 = 44;
pub const TRUNCATE: u64 = 45;
pub const FTRUNCATE: u64 = 46;
pub const FALLOCATE: u64 = 47;
pub const FCHDIR: u64 = 50;
pub const FCHMOD: u64 = 52;
pub const FCHMODAT: u64 = 53;
pub const FCHOWNAT: u64 = 54;
pub const FCHOWN: u64 = 55;
pub const SENDFILE: u64 = 71;
pub const COPY_FILE_RANGE: u64 = 285;
pub const PSELECT6: u64 = 72;
pub const PPOLL: u64 = 73;
pub const SIGNALFD4: u64 = 74;
pub const FSYNC: u64 = 82;
pub const FDATASYNC: u64 = 83;
pub const TIMERFD_CREATE: u64 = 85;
pub const TIMERFD_SETTIME: u64 = 86;
pub const TIMERFD_GETTIME: u64 = 87;
pub const UTIMENSAT: u64 = 88;
pub const UMASK: u64 = 166;
pub const SOCKETPAIR: u64 = 199;
pub const BIND: u64 = 200;
pub const LISTEN: u64 = 201;
pub const ACCEPT: u64 = 202;
pub const GETSOCKNAME: u64 = 204;
pub const GETPEERNAME: u64 = 205;
pub const SENDTO: u64 = 206;
pub const RECVFROM: u64 = 207;
pub const SETSOCKOPT: u64 = 208;
pub const GETSOCKOPT: u64 = 209;
pub const SHUTDOWN: u64 = 210;
pub const ACCEPT4: u64 = 242;
pub const RENAMEAT2: u64 = 276;
pub const MEMFD_CREATE: u64 = 279;
pub const FACCESSAT2: u64 = 439;

pub const TKILL: u64 = 130;
pub const SIGALTSTACK: u64 = 132;
pub const GETTIMEOFDAY: u64 = 169;
pub const SYSINFO: u64 = 179;
pub const MADVISE: u64 = 233;
pub const CLOCK_NANOSLEEP: u64 = 115;
pub const CAPGET: u64 = 90;
pub const TIMES: u64 = 153;
pub const CAPSET: u64 = 91;
pub const GET_MEMPOLICY: u64 = 236;
pub const PIDFD_SEND_SIGNAL: u64 = 424;
pub const PIDFD_OPEN: u64 = 434;
pub const PIDFD_GETFD: u64 = 438;
/// Not Linux's: a call debug builds answer by panicking, to test that a
/// runtime error in one process's call ends that process alone.
pub const MACWL_TEST_PANIC: u64 = 0x4d57_0001;

/// Names of system calls 0–243 (the generic table; 244–259 are reserved for
/// architecture-specific calls, which arm64 doesn't have).
const NAMES_0: [&str; 244] = [
    "io_setup", "io_destroy", "io_submit", "io_cancel", "io_getevents", "setxattr", "lsetxattr",
    "fsetxattr", "getxattr", "lgetxattr", "fgetxattr", "listxattr", "llistxattr", "flistxattr",
    "removexattr", "lremovexattr", "fremovexattr", "getcwd", "lookup_dcookie", "eventfd2",
    "epoll_create1", "epoll_ctl", "epoll_pwait", "dup", "dup3", "fcntl", "inotify_init1",
    "inotify_add_watch", "inotify_rm_watch", "ioctl", "ioprio_set", "ioprio_get", "flock",
    "mknodat", "mkdirat", "unlinkat", "symlinkat", "linkat", "renameat", "umount2", "mount",
    "pivot_root", "nfsservctl", "statfs", "fstatfs", "truncate", "ftruncate", "fallocate",
    "faccessat", "chdir", "fchdir", "chroot", "fchmod", "fchmodat", "fchownat", "fchown", "openat",
    "close", "vhangup", "pipe2", "quotactl", "getdents64", "lseek", "read", "write", "readv",
    "writev", "pread64", "pwrite64", "preadv", "pwritev", "sendfile", "pselect6", "ppoll",
    "signalfd4", "vmsplice", "splice", "tee", "readlinkat", "newfstatat", "fstat", "sync", "fsync",
    "fdatasync", "sync_file_range", "timerfd_create", "timerfd_settime", "timerfd_gettime",
    "utimensat", "acct", "capget", "capset", "personality", "exit", "exit_group", "waitid",
    "set_tid_address", "unshare", "futex", "set_robust_list", "get_robust_list", "nanosleep",
    "getitimer", "setitimer", "kexec_load", "init_module", "delete_module", "timer_create",
    "timer_gettime", "timer_getoverrun", "timer_settime", "timer_delete", "clock_settime",
    "clock_gettime", "clock_getres", "clock_nanosleep", "syslog", "ptrace", "sched_setparam",
    "sched_setscheduler", "sched_getscheduler", "sched_getparam", "sched_setaffinity",
    "sched_getaffinity", "sched_yield", "sched_get_priority_max", "sched_get_priority_min",
    "sched_rr_get_interval", "restart_syscall", "kill", "tkill", "tgkill", "sigaltstack",
    "rt_sigsuspend", "rt_sigaction", "rt_sigprocmask", "rt_sigpending", "rt_sigtimedwait",
    "rt_sigqueueinfo", "rt_sigreturn", "setpriority", "getpriority", "reboot", "setregid", "setgid",
    "setreuid", "setuid", "setresuid", "getresuid", "setresgid", "getresgid", "setfsuid",
    "setfsgid", "times", "setpgid", "getpgid", "getsid", "setsid", "getgroups", "setgroups",
    "uname", "sethostname", "setdomainname", "getrlimit", "setrlimit", "getrusage", "umask",
    "prctl", "getcpu", "gettimeofday", "settimeofday", "adjtimex", "getpid", "getppid", "getuid",
    "geteuid", "getgid", "getegid", "gettid", "sysinfo", "mq_open", "mq_unlink", "mq_timedsend",
    "mq_timedreceive", "mq_notify", "mq_getsetattr", "msgget", "msgctl", "msgrcv", "msgsnd",
    "semget", "semctl", "semtimedop", "semop", "shmget", "shmctl", "shmat", "shmdt", "socket",
    "socketpair", "bind", "listen", "accept", "connect", "getsockname", "getpeername", "sendto",
    "recvfrom", "setsockopt", "getsockopt", "shutdown", "sendmsg", "recvmsg", "readahead", "brk",
    "munmap", "mremap", "add_key", "request_key", "keyctl", "clone", "execve", "mmap", "fadvise64",
    "swapon", "swapoff", "mprotect", "msync", "mlock", "munlock", "mlockall", "munlockall",
    "mincore", "madvise", "remap_file_pages", "mbind", "get_mempolicy", "set_mempolicy",
    "migrate_pages", "move_pages", "rt_tgsigqueueinfo", "perf_event_open", "accept4", "recvmmsg",
];
/// 260–294.
const NAMES_260: [&str; 35] = [
    "wait4", "prlimit64", "fanotify_init", "fanotify_mark", "name_to_handle_at",
    "open_by_handle_at", "clock_adjtime", "syncfs", "setns", "sendmmsg", "process_vm_readv",
    "process_vm_writev", "kcmp", "finit_module", "sched_setattr", "sched_getattr", "renameat2",
    "seccomp", "getrandom", "memfd_create", "bpf", "execveat", "userfaultfd", "membarrier",
    "mlock2", "copy_file_range", "preadv2", "pwritev2", "pkey_mprotect", "pkey_alloc", "pkey_free",
    "statx", "io_pgetevents", "rseq", "kexec_file_load",
];
/// 424–462 (295–423 are only used on 32-bit architectures).
const NAMES_424: [&str; 39] = [
    "pidfd_send_signal", "io_uring_setup", "io_uring_enter", "io_uring_register", "open_tree",
    "move_mount", "fsopen", "fsconfig", "fsmount", "fspick", "pidfd_open", "clone3", "close_range",
    "openat2", "pidfd_getfd", "faccessat2", "process_madvise", "epoll_pwait2", "mount_setattr",
    "quotactl_fd", "landlock_create_ruleset", "landlock_add_rule", "landlock_restrict_self",
    "memfd_secret", "process_mrelease", "futex_waitv", "set_mempolicy_home_node", "cachestat",
    "fchmodat2", "map_shadow_stack", "futex_wake", "futex_wait", "futex_requeue", "statmount",
    "listmount", "lsm_get_self_attr", "lsm_set_self_attr", "lsm_list_modules", "mseal",
];

/// Human-readable name for logging unimplemented calls.
pub fn name(nr: u64) -> &'static str {
    let nr = nr as usize;
    let found = match nr {
        0..244 => NAMES_0.get(nr),
        260..295 => NAMES_260.get(nr - 260),
        424..463 => NAMES_424.get(nr - 424),
        _ => None,
    };
    found.copied().unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_constants() {
        for (nr, want) in [
            (SETXATTR, "setxattr"),
            (FREMOVEXATTR, "fremovexattr"),
            (GETCWD, "getcwd"),
            (EVENTFD2, "eventfd2"),
            (EPOLL_CREATE1, "epoll_create1"),
            (EPOLL_CTL, "epoll_ctl"),
            (EPOLL_PWAIT, "epoll_pwait"),
            (DUP, "dup"),
            (DUP3, "dup3"),
            (FCNTL, "fcntl"),
            (INOTIFY_INIT1, "inotify_init1"),
            (INOTIFY_ADD_WATCH, "inotify_add_watch"),
            (INOTIFY_RM_WATCH, "inotify_rm_watch"),
            (IOCTL, "ioctl"),
            (FLOCK, "flock"),
            (MKDIRAT, "mkdirat"),
            (UNLINKAT, "unlinkat"),
            (FACCESSAT, "faccessat"),
            (CHDIR, "chdir"),
            (OPENAT, "openat"),
            (CLOSE, "close"),
            (PIPE2, "pipe2"),
            (GETDENTS64, "getdents64"),
            (LSEEK, "lseek"),
            (READ, "read"),
            (WRITE, "write"),
            (READV, "readv"),
            (WRITEV, "writev"),
            (PREAD64, "pread64"),
            (PWRITE64, "pwrite64"),
            (READLINKAT, "readlinkat"),
            (NEWFSTATAT, "newfstatat"),
            (FSTAT, "fstat"),
            (EXIT, "exit"),
            (EXIT_GROUP, "exit_group"),
            (SET_TID_ADDRESS, "set_tid_address"),
            (FUTEX, "futex"),
            (SET_ROBUST_LIST, "set_robust_list"),
            (GET_ROBUST_LIST, "get_robust_list"),
            (FUTEX_WAITV, "futex_waitv"),
            (FUTEX_WAKE, "futex_wake"),
            (FUTEX_WAIT, "futex_wait"),
            (FUTEX_REQUEUE, "futex_requeue"),
            (NANOSLEEP, "nanosleep"),
            (CLOCK_GETTIME, "clock_gettime"),
            (CLOCK_GETRES, "clock_getres"),
            (SCHED_GETAFFINITY, "sched_getaffinity"),
            (SCHED_YIELD, "sched_yield"),
            (GETITIMER, "getitimer"),
            (SETITIMER, "setitimer"),
            (TIMER_CREATE, "timer_create"),
            (TIMER_GETTIME, "timer_gettime"),
            (TIMER_GETOVERRUN, "timer_getoverrun"),
            (TIMER_SETTIME, "timer_settime"),
            (TIMER_DELETE, "timer_delete"),
            (KILL, "kill"),
            (TGKILL, "tgkill"),
            (RT_SIGSUSPEND, "rt_sigsuspend"),
            (RT_SIGACTION, "rt_sigaction"),
            (RT_SIGPROCMASK, "rt_sigprocmask"),
            (RT_SIGPENDING, "rt_sigpending"),
            (RT_SIGTIMEDWAIT, "rt_sigtimedwait"),
            (RT_SIGRETURN, "rt_sigreturn"),
            (SETREGID, "setregid"),
            (SETGID, "setgid"),
            (SETREUID, "setreuid"),
            (SETUID, "setuid"),
            (SETRESUID, "setresuid"),
            (GETRESUID, "getresuid"),
            (SETRESGID, "setresgid"),
            (GETRESGID, "getresgid"),
            (SETPGID, "setpgid"),
            (GETPGID, "getpgid"),
            (GETSID, "getsid"),
            (SETSID, "setsid"),
            (GETGROUPS, "getgroups"),
            (UNAME, "uname"),
            (GETPID, "getpid"),
            (GETPPID, "getppid"),
            (GETUID, "getuid"),
            (GETEUID, "geteuid"),
            (GETGID, "getgid"),
            (GETEGID, "getegid"),
            (GETTID, "gettid"),
            (SOCKET, "socket"),
            (CONNECT, "connect"),
            (SENDMSG, "sendmsg"),
            (RECVMSG, "recvmsg"),
            (BRK, "brk"),
            (MUNMAP, "munmap"),
            (MMAP, "mmap"),
            (MPROTECT, "mprotect"),
            (PRLIMIT64, "prlimit64"),
            (GETRANDOM, "getrandom"),
            (WAITID, "waitid"),
            (GETRUSAGE, "getrusage"),
            (PRCTL, "prctl"),
            (CLONE, "clone"),
            (EXECVE, "execve"),
            (WAIT4, "wait4"),
            (CLONE3, "clone3"),
            (CLOSE_RANGE, "close_range"),
            (EPOLL_PWAIT2, "epoll_pwait2"),
            (FADVISE64, "fadvise64"),
            (STATX, "statx"),
            (RSEQ, "rseq"),
            (SYMLINKAT, "symlinkat"),
            (LINKAT, "linkat"),
            (RENAMEAT, "renameat"),
            (STATFS, "statfs"),
            (FSTATFS, "fstatfs"),
            (TRUNCATE, "truncate"),
            (FTRUNCATE, "ftruncate"),
            (FALLOCATE, "fallocate"),
            (FCHDIR, "fchdir"),
            (FCHMOD, "fchmod"),
            (FCHMODAT, "fchmodat"),
            (FCHOWNAT, "fchownat"),
            (FCHOWN, "fchown"),
            (SENDFILE, "sendfile"),
            (COPY_FILE_RANGE, "copy_file_range"),
            (PSELECT6, "pselect6"),
            (PPOLL, "ppoll"),
            (SIGNALFD4, "signalfd4"),
            (FSYNC, "fsync"),
            (FDATASYNC, "fdatasync"),
            (TIMERFD_CREATE, "timerfd_create"),
            (TIMERFD_SETTIME, "timerfd_settime"),
            (TIMERFD_GETTIME, "timerfd_gettime"),
            (UTIMENSAT, "utimensat"),
            (UMASK, "umask"),
            (SOCKETPAIR, "socketpair"),
            (BIND, "bind"),
            (LISTEN, "listen"),
            (ACCEPT, "accept"),
            (GETSOCKNAME, "getsockname"),
            (GETPEERNAME, "getpeername"),
            (SENDTO, "sendto"),
            (RECVFROM, "recvfrom"),
            (SETSOCKOPT, "setsockopt"),
            (GETSOCKOPT, "getsockopt"),
            (SHUTDOWN, "shutdown"),
            (ACCEPT4, "accept4"),
            (RENAMEAT2, "renameat2"),
            (MEMFD_CREATE, "memfd_create"),
            (FACCESSAT2, "faccessat2"),
            (TKILL, "tkill"),
            (SIGALTSTACK, "sigaltstack"),
            (GETTIMEOFDAY, "gettimeofday"),
            (SYSINFO, "sysinfo"),
            (MADVISE, "madvise"),
            (CLOCK_NANOSLEEP, "clock_nanosleep"),
        ] {
            assert_eq!(name(nr), want, "system call {nr}");
        }
        assert_eq!(name(250), "?");
        assert_eq!(name(463), "?");
    }
}
