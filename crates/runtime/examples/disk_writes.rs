//! Measure whether dirtying memory makes macOS write to disk: rewrites 1 GiB
//! of anonymous memory (`anon`), of a shared mapping of an unlinked file
//! (`file`), or of a POSIX shm object (`shm`, what memfd and /dev/shm use)
//! for N seconds: `disk_writes anon|file|shm [seconds]`. Compare `ioreg -c IOBlockStorageDriver` "Bytes
//! (Write)" before and after: the kernel's writeback isn't charged to the
//! process, so its own counter (printed) stays at 0 either way. Measured:
//! ~4 GB/min for `file`, nothing beyond the idle baseline for `anon` or `shm`.
use std::time::{Duration, Instant};

fn disk_written() -> u64 {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4, (&mut ri as *mut libc::rusage_info_v4).cast()) };
    assert_eq!(r, 0);
    ri.ri_diskio_byteswritten
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "anon".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(70);
    const LEN: usize = 1 << 30;
    let (flags, fd) = if mode == "shm" {
        let fd = linux_abi::shm::create().unwrap();
        (libc::MAP_SHARED, std::os::fd::IntoRawFd::into_raw_fd(fd))
    } else if mode == "file" {
        let mut tmpl = std::env::temp_dir().join("wbprobe-XXXXXX").into_os_string().into_encoded_bytes();
        tmpl.push(0);
        let fd = unsafe { libc::mkstemp(tmpl.as_mut_ptr().cast()) };
        assert!(fd >= 0);
        unsafe { libc::unlink(tmpl.as_ptr().cast()) };
        assert_eq!(unsafe { libc::ftruncate(fd, LEN as i64) }, 0);
        (libc::MAP_SHARED, fd)
    } else {
        (libc::MAP_ANON | libc::MAP_PRIVATE, -1)
    };
    let p = unsafe { libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ | libc::PROT_WRITE, flags, fd, 0) } as *mut u64;
    assert_ne!(p as isize, -1);
    let before = disk_written();
    let start = Instant::now();
    let mut round = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        // Rewrite every 16 KiB page once per round.
        for i in (0..LEN / 8).step_by(2048) {
            unsafe { *p.add(i) = round ^ i as u64 };
        }
        round += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("{}: {round} rounds over 1 GiB in {:?}: process disk writes {} MiB", mode, start.elapsed(), (disk_written() - before) >> 20);
}
