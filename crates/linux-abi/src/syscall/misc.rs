//! Process identity, clocks, sleeping, `uname`, `getrandom`, resource limits.

use std::sync::atomic::Ordering;

use super::{ok, Process, Thread};
use crate::{
    abi::{clock, Timespec, UTSNAME_SIZE, UTS_FIELD},
    errno,
    memory::{GuestMemory, GuestMemoryExt},
};

/// A Linux clock id as Darwin's. Linux's MONOTONIC stops during suspend, like
/// Darwin's UPTIME_RAW; BOOTTIME doesn't, like Darwin's MONOTONIC.
fn host_clock(id: u64) -> Result<libc::clockid_t, i64> {
    Ok(match id {
        clock::REALTIME | clock::REALTIME_COARSE => libc::CLOCK_REALTIME,
        clock::MONOTONIC | clock::MONOTONIC_COARSE => libc::CLOCK_UPTIME_RAW,
        clock::MONOTONIC_RAW => libc::CLOCK_UPTIME_RAW,
        clock::BOOTTIME => libc::CLOCK_MONOTONIC,
        clock::PROCESS_CPUTIME_ID => libc::CLOCK_PROCESS_CPUTIME_ID,
        clock::THREAD_CPUTIME_ID => libc::CLOCK_THREAD_CPUTIME_ID,
        _ => return Err(errno::EINVAL),
    })
}

/// The time on Linux clock `id`, since its epoch.
pub(super) fn clock_now(id: u64) -> Result<std::time::Duration, i64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    ok(unsafe { libc::clock_gettime(host_clock(id)?, &mut ts) } as i64)?;
    Ok(std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

pub(super) fn clock_gettime<M: GuestMemory + ?Sized>(mem: &M, id: u64, out: u64, res: bool) -> Result<i64, i64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let f = if res { libc::clock_getres } else { libc::clock_gettime };
    ok(unsafe { f(host_clock(id)?, &mut ts) } as i64)?;
    if out != 0 {
        mem.write_pod(out, &Timespec { tv_sec: ts.tv_sec, tv_nsec: ts.tv_nsec })?;
    }
    Ok(0)
}

/// `gettimeofday`. The timezone argument is obsolete; Linux fills it with
/// zeros unless the clock was set with one, which nothing does any more.
pub(super) fn gettimeofday<M: GuestMemory + ?Sized>(mem: &M, tv: u64, tz: u64) -> Result<i64, i64> {
    if tv != 0 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        ok(unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } as i64)?;
        mem.write_pod(tv, &[ts.tv_sec, ts.tv_nsec / 1000])?;
    }
    if tz != 0 {
        mem.write_pod(tz, &[0i32; 2])?;
    }
    Ok(0)
}

/// `nanosleep` and relative `clock_nanosleep`. Interrupted (EINTR), it
/// says how much of the sleep was left in `rem`, if given, as Linux does.
pub(super) fn nanosleep<M: GuestMemory + ?Sized>(mem: &M, req: u64, rem: u64) -> Result<i64, i64> {
    let t: Timespec = mem.read_pod(req)?;
    if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
        return Err(errno::EINVAL);
    }
    let ts = libc::timespec { tv_sec: t.tv_sec, tv_nsec: t.tv_nsec };
    let mut left = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let r = ok(unsafe { libc::nanosleep(&ts, &mut left) } as i64);
    if r == Err(errno::EINTR) && rem != 0 {
        mem.write_pod(rem, &Timespec { tv_sec: left.tv_sec, tv_nsec: left.tv_nsec })?;
    }
    r.map(|_| 0)
}

pub(super) fn clock_nanosleep<M: GuestMemory + ?Sized>(mem: &M, id: u64, flags: u64, req: u64, rem: u64) -> Result<i64, i64> {
    const TIMER_ABSTIME: u64 = 1;
    let host = host_clock(id)?;
    if flags & TIMER_ABSTIME == 0 {
        return nanosleep(mem, req, rem);
    }
    let t: Timespec = mem.read_pod(req)?;
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    ok(unsafe { libc::clock_gettime(host, &mut now) } as i64)?;
    let ns = |s: i64, n: i64| s as i128 * 1_000_000_000 + n as i128;
    let wait = ns(t.tv_sec, t.tv_nsec) - ns(now.tv_sec, now.tv_nsec);
    if wait > 0 {
        let ts = libc::timespec { tv_sec: (wait / 1_000_000_000) as i64, tv_nsec: (wait % 1_000_000_000) as i64 };
        ok(unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) } as i64)?;
    }
    Ok(0)
}

pub(super) fn uname<M: GuestMemory + ?Sized>(mem: &M, buf: u64) -> Result<i64, i64> {
    let mut host = [0u8; 256];
    unsafe { libc::gethostname(host.as_mut_ptr().cast(), host.len()) };
    let host_len = host.iter().position(|&b| b == 0).unwrap_or(0);
    let fields: [&[u8]; 6] = [b"Linux", &host[..host_len], b"6.6.0-macwayland", b"#1 SMP", b"aarch64", b"(none)"];
    let mut out = [0u8; UTSNAME_SIZE];
    for (i, f) in fields.iter().enumerate() {
        let n = f.len().min(UTS_FIELD - 1);
        out[i * UTS_FIELD..i * UTS_FIELD + n].copy_from_slice(&f[..n]);
    }
    mem.write(buf, &out)?;
    Ok(0)
}

pub(super) fn getrandom<M: GuestMemory + ?Sized>(mem: &M, buf: u64, len: u64) -> Result<i64, i64> {
    let mut data = vec![0u8; len.min(1 << 20) as usize];
    unsafe { libc::arc4random_buf(data.as_mut_ptr().cast(), data.len()) };
    mem.write(buf, &data)?;
    Ok(data.len() as i64)
}

/// `prlimit64` on this process. Limits are reported, not enforced; setting
/// one is accepted and remembered.
pub(super) fn prlimit64<M: GuestMemory + ?Sized>(p: &Process, mem: &M, pid: u64, resource: u64, new: u64, old: u64) -> Result<i64, i64> {
    const RLIM_NLIMITS: u64 = 16;
    if pid != 0 && pid as i64 != p.pid {
        return Err(errno::ESRCH);
    }
    if resource >= RLIM_NLIMITS {
        return Err(errno::EINVAL);
    }
    let new = if new != 0 { Some(mem.read_pod::<[u64; 2]>(new)?) } else { None };
    let mut limits = p.rlimits.lock().unwrap_or_else(|e| e.into_inner());
    if old != 0 {
        mem.write_pod(old, &limits[resource as usize])?;
    }
    if let Some(lim) = new {
        if lim[0] > lim[1] {
            return Err(errno::EINVAL);
        }
        limits[resource as usize] = lim;
    }
    Ok(0)
}

/// `getrusage`: the process's, its children's, or the thread's, from the
/// kernel's accounting.
pub(super) fn getrusage<M: GuestMemory + ?Sized>(p: &Process, mem: &M, who: u64, out: u64) -> Result<i64, i64> {
    mem.write(out, &p.kernel.rusage(p, who as i64)?)?;
    Ok(0)
}

fn sysctl_u64(name: &std::ffi::CStr) -> u64 {
    let (mut v, mut len) = (0u64, 8usize);
    unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    // Some are 32-bit; the low bytes landed first either way (little-endian).
    if len == 4 { v & 0xffff_ffff } else { v }
}

/// `sysinfo`: uptime, load and memory from the host.
pub(super) fn sysinfo<M: GuestMemory + ?Sized>(mem: &M, out: u64) -> Result<i64, i64> {
    let mut boot = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = size_of::<libc::timeval>();
    unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), (&mut boot as *mut libc::timeval).cast(), &mut len, std::ptr::null_mut(), 0) };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut loads = [0f64; 3];
    unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    let page = sysctl_u64(c"hw.pagesize");
    let free = (sysctl_u64(c"vm.page_free_count") + sysctl_u64(c"vm.page_speculative_count")) * page;
    let mut b = [0u8; 112];
    let mut put = |off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put(0, (now - boot.tv_sec).max(0) as u64);
    for (i, l) in loads.iter().enumerate() {
        put(8 + i * 8, (l * 65536.0) as u64); // SI_LOAD_SHIFT = 16
    }
    put(32, sysctl_u64(c"hw.memsize"));
    put(40, free);
    b[80..82].copy_from_slice(&1u16.to_le_bytes()); // procs: not worth counting
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit: bytes
    mem.write(out, &b)?;
    Ok(0)
}

/// `sched_getaffinity`: every CPU (pinning isn't supported).
pub(super) fn sched_getaffinity<M: GuestMemory + ?Sized>(mem: &M, len: u64, mask: u64) -> Result<i64, i64> {
    let ncpu = sysctl_u64(c"hw.logicalcpu").max(1) as usize;
    let bytes = ncpu.div_ceil(64) * 8;
    if (len as usize) < bytes || !len.is_multiple_of(8) {
        return Err(errno::EINVAL);
    }
    let mut out = vec![0u8; bytes];
    for cpu in 0..ncpu {
        out[cpu / 8] |= 1 << (cpu % 8);
    }
    mem.write(mask, &out)?;
    Ok(bytes as i64)
}

/// The `set*id` calls, for an unprivileged process: "changing" to ids it
/// already has succeeds (programs drop privileges they never had at startup),
/// anything else is EPERM. `ids` are the new values, -1 meaning unchanged.
pub(super) fn setids(ids: &[u64], group: bool) -> Result<i64, i64> {
    let (real, effective) = unsafe {
        if group { (libc::getgid(), libc::getegid()) } else { (libc::getuid(), libc::geteuid()) }
    };
    let ok = ids.iter().all(|&id| id as u32 == u32::MAX || id as u32 == real || id as u32 == effective);
    if ok { Ok(0) } else { Err(errno::EPERM) }
}

/// `getresuid`/`getresgid`: real, effective and saved ids.
pub(super) fn getresids<M: GuestMemory + ?Sized>(mem: &M, ptrs: [u64; 3], group: bool) -> Result<i64, i64> {
    let (real, effective) = unsafe {
        if group { (libc::getgid(), libc::getegid()) } else { (libc::getuid(), libc::geteuid()) }
    };
    for (ptr, id) in ptrs.into_iter().zip([real, effective, effective]) {
        mem.write_pod(ptr, &id)?;
    }
    Ok(0)
}

/// `getgroups`: the host's supplementary groups (guest ids are host ids).
pub(super) fn getgroups<M: GuestMemory + ?Sized>(mem: &M, size: u64, list: u64) -> Result<i64, i64> {
    let n = ok(unsafe { libc::getgroups(0, std::ptr::null_mut()) } as i64)?;
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let n = ok(unsafe { libc::getgroups(n as i32, groups.as_mut_ptr()) } as i64)? as usize;
    groups.truncate(n);
    if size == 0 {
        return Ok(n as i64);
    }
    if (size as usize) < n {
        return Err(errno::EINVAL);
    }
    let bytes: Vec<u8> = groups.iter().flat_map(|g| g.to_le_bytes()).collect();
    mem.write(list, &bytes)?;
    Ok(n as i64)
}

/// `prctl`: the options programs commonly use at startup.
pub(super) fn prctl<M: GuestMemory + ?Sized>(p: &Process, t: &mut Thread, mem: &M, option: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_PDEATHSIG: u64 = 2;
    const PR_GET_DUMPABLE: u64 = 3;
    const PR_SET_DUMPABLE: u64 = 4;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_CAPBSET_READ: u64 = 23;
    const PR_SET_MM: u64 = 35;
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    const PR_SET_VMA: u64 = 0x5356_4d41;
    const TASK_COMM_LEN: usize = 16;
    match option {
        PR_SET_NAME => {
            let mut name = mem.read_cstr(a2)?;
            name.truncate(TASK_COMM_LEN - 1);
            t.comm = name;
            Ok(0)
        }
        PR_GET_NAME => {
            if t.comm.is_empty() {
                let exe = p.exe();
                let exe = exe.rsplit(|&b| b == b'/').next().unwrap_or_default();
                t.comm = exe[..exe.len().min(TASK_COMM_LEN - 1)].to_vec();
            }
            let mut out = [0u8; TASK_COMM_LEN];
            out[..t.comm.len()].copy_from_slice(&t.comm);
            mem.write(a2, &out)?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(p.dumpable.load(Ordering::Relaxed) as i64),
        PR_SET_DUMPABLE if a2 <= 1 => {
            p.dumpable.store(a2 == 1, Ordering::Relaxed);
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS if a2 == 1 && a3 == 0 => {
            p.no_new_privs.store(true, Ordering::Relaxed);
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => Ok(p.no_new_privs.load(Ordering::Relaxed) as i64),
        // There's no parent death to be told about until processes exist (item 6).
        PR_SET_PDEATHSIG => Ok(0),
        PR_GET_PDEATHSIG => {
            mem.write_pod(a2, &0i32)?;
            Ok(0)
        }
        // No capabilities: an unprivileged process's bounding set still
        // lists them all, but nothing here grants any.
        PR_CAPBSET_READ if a2 <= 40 => Ok(1),
        // Rewriting the process's memory layout fields (glibc offers it the
        // stack's address) needs CAP_SYS_RESOURCE, which nothing has here.
        PR_SET_MM => Err(errno::EPERM),
        // Naming anonymous memory (glibc does, for malloc arenas): a kernel
        // built without CONFIG_ANON_VMA_NAME says EINVAL.
        PR_SET_VMA => Err(errno::EINVAL),
        _ => {
            p.unimplemented(format!("prctl option {option:#x}"));
            Err(errno::EINVAL)
        }
    }
}

/// Linux's defaults: [soft, hard] for each `RLIMIT_*`.
pub(super) fn default_rlimits() -> [[u64; 2]; 16] {
    const INF: u64 = u64::MAX;
    let mut l = [[INF, INF]; 16];
    l[3] = [8 << 20, INF]; // STACK (matches the runtime's stack)
    l[4] = [0, INF]; // CORE
    l[7] = [1024, 1024 * 1024]; // NOFILE
    l[8] = [8 << 20, 8 << 20]; // MEMLOCK
    l[13] = [0, 0]; // NICE
    l[14] = [0, 0]; // RTPRIO
    l
}

