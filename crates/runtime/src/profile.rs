//! `MACWL_PROFILE=1`: timestamped marks on stderr for the phases of starting,
//! forking and exec'ing, on a clock shared by all processes (so a parent's
//! and its child's marks line up).

use std::sync::OnceLock;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MACWL_PROFILE").is_some_and(|v| v != "0"))
}

/// Log that `what` just happened.
pub fn mark(what: &str) {
    if enabled() {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        let us = ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000;
        eprintln!("[profile] {us} {} {what}", std::process::id());
    }
}
