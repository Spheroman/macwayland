//! What Linux programs learn about the machine from `/proc` and
//! `/etc/os-release`, made from macOS's own figures: the CPUs, memory and
//! swap, uptime and load, and (for a root without one) what system this is.

use std::ffi::CStr;

/// The kernel release `uname` reports.
pub const KERNEL_RELEASE: &str = "6.6.0-macwayland";

fn sysctl_u64(name: &CStr) -> u64 {
    let (mut v, mut len) = (0u64, 8usize);
    unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    // Some are 32-bit; the low bytes landed first either way (little-endian).
    if len == 4 { v & 0xffff_ffff } else { v }
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    if unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf[..len]).ok()?.to_string_lossy().into_owned();
    (!s.is_empty()).then_some(s)
}

pub fn logical_cpus() -> u64 {
    sysctl_u64(c"hw.logicalcpu").max(1)
}

/// Seconds since boot.
pub fn uptime() -> f64 {
    let mut boot = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = size_of::<libc::timeval>();
    unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), (&mut boot as *mut libc::timeval).cast(), &mut len, std::ptr::null_mut(), 0) };
    let mut now = libc::timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { libc::gettimeofday(&mut now, std::ptr::null_mut()) };
    ((now.tv_sec - boot.tv_sec) as f64 + (now.tv_usec - boot.tv_usec) as f64 / 1e6).max(0.0)
}

pub fn loadavg() -> [f64; 3] {
    let mut loads = [0f64; 3];
    unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    loads
}

/// Memory, in bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    /// What could be had without swapping: everything but what apps, the
    /// kernel (wired) and the compressor hold — as Activity Monitor counts.
    pub available: u64,
    /// File-backed pages.
    pub cached: u64,
    pub active: u64,
    pub inactive: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn memory() -> Memory {
    let total = sysctl_u64(c"hw.memsize");
    let page = sysctl_u64(c"hw.pagesize");
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = (size_of::<libc::vm_statistics64>() / size_of::<libc::integer_t>()) as libc::mach_msg_type_number_t;
    #[allow(deprecated)] // libc's mach_host_self: the mach2 crate isn't worth adding for it
    let host = unsafe { libc::mach_host_self() };
    let r = unsafe { libc::host_statistics64(host, libc::HOST_VM_INFO64, (&mut vm as *mut libc::vm_statistics64).cast(), &mut count) };
    let mut m = Memory { total, ..Default::default() };
    if r == 0 {
        let pages = |n: u32| n as u64 * page;
        m.free = pages(vm.free_count) + pages(vm.speculative_count);
        m.cached = pages(vm.external_page_count);
        m.active = pages(vm.active_count);
        m.inactive = pages(vm.inactive_count);
        let used = pages(vm.internal_page_count).saturating_sub(pages(vm.purgeable_count)) + pages(vm.wire_count) + pages(vm.compressor_page_count);
        m.available = total.saturating_sub(used);
    }
    let mut swap: libc::xsw_usage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::xsw_usage>();
    if unsafe { libc::sysctlbyname(c"vm.swapusage".as_ptr(), (&mut swap as *mut libc::xsw_usage).cast(), &mut len, std::ptr::null_mut(), 0) } == 0 {
        m.swap_total = swap.xsu_total;
        m.swap_free = swap.xsu_avail;
    }
    m
}

/// `/proc/cpuinfo`, arm64's format. Linux has no model name line on arm64;
/// it's added (programs that show the CPU look for one), since the part
/// numbers alone don't say which Apple chip this is.
pub fn cpuinfo() -> String {
    let model = sysctl_string(c"machdep.cpu.brand_string");
    let mut out = String::new();
    for n in 0..logical_cpus() {
        out.push_str(&format!("processor\t: {n}\n"));
        if let Some(model) = &model {
            out.push_str(&format!("model name\t: {model}\n"));
        }
        // The timer runs at 24 MHz; Linux reports twice that.
        out.push_str("BogoMIPS\t: 48.00\nFeatures\t: fp asimd\nCPU implementer\t: 0x61\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0x000\nCPU revision\t: 0\n\n");
    }
    out
}

/// `/proc/meminfo`: the fields `free`, `top` and friends read (in kB).
pub fn meminfo() -> String {
    let m = memory();
    let kb = |b: u64| b / 1024;
    let fields = [
        ("MemTotal", kb(m.total)),
        ("MemFree", kb(m.free)),
        ("MemAvailable", kb(m.available)),
        ("Buffers", 0),
        ("Cached", kb(m.cached)),
        ("SwapCached", 0),
        ("Active", kb(m.active)),
        ("Inactive", kb(m.inactive)),
        ("SwapTotal", kb(m.swap_total)),
        ("SwapFree", kb(m.swap_free)),
        ("Shmem", 0),
        ("SReclaimable", 0),
    ];
    fields.iter().map(|(name, v)| format!("{:<16}{:>8} kB\n", format!("{name}:"), v)).collect()
}

/// `/proc/uptime`: seconds up, and idle (summed over CPUs; not tracked).
pub fn proc_uptime() -> String {
    format!("{:.2} 0.00\n", uptime())
}

/// `/proc/loadavg`. The process counts aren't tracked.
pub fn proc_loadavg(last_pid: i64) -> String {
    let [a, b, c] = loadavg();
    format!("{a:.2} {b:.2} {c:.2} 1/1 {last_pid}\n")
}

pub fn proc_version() -> String {
    format!("Linux version {KERNEL_RELEASE} (macwayland) #1 SMP\n")
}

/// `/etc/machine-id` for a root that has none: 32 hex digits, the same each
/// time on this Mac. Made from its hardware UUID, hashed, as the id is any
/// program's to read and the UUID better kept to the Mac.
pub fn machine_id() -> Option<String> {
    let mut uuid = [0u8; 16];
    let wait = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &wait) } != 0 {
        return None;
    }
    // FNV-1a, twice (different offsets) for 128 bits: stable, unlike std's
    // hasher, whose algorithm may change.
    let fnv = |offset: u64| {
        b"macwayland machine-id".iter().chain(&uuid).fold(offset, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
    };
    Some(format!("{:016x}{:016x}\n", fnv(0xcbf2_9ce4_8422_2325), fnv(0x6c62_272e_07bb_0142)))
}

/// `/etc/os-release` for a root that has none (the Mac's own `/`): Linux
/// programs running on macOS, through macwayland.
pub fn os_release() -> String {
    let version = sysctl_string(c"kern.osproductversion").unwrap_or_default();
    format!(
        "NAME=\"macwayland\"\nID=macwayland\nID_LIKE=macos\nVERSION_ID=\"{version}\"\n\
         PRETTY_NAME=\"macwayland (macOS {version})\"\nHOME_URL=\"https://github.com/Spheroman/macwayland\"\n"
    )
}
