//! Where a GPU submission's time goes (docs/gpu.md, step 3), with
//! `MACWL_GPU_TRACE` set (in macwl-run's environment): each fence is
//! stamped as its execbuffer enters, has submitted the command, has made
//! the fence, returns; as virglrenderer's callback signals it; and as a
//! guest wait on it starts and returns. Completed samples are kept, and
//! the segments' percentiles printed as a context goes.

use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

use super::sync::Fence;

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    Entered,
    Calling,
    Submitted,
    Fenced,
    Returned,
    Signaled,
    WaitStarted,
    WaitReturned,
}

const STAGES: usize = 8;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stamps([Option<Instant>; STAGES]);

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MACWL_GPU_TRACE").is_some())
}

/// Now, if tracing (a stamp to give `stamp_at` later).
pub fn now() -> Option<Instant> {
    on().then(Instant::now)
}

pub fn stamp(f: &Fence, stage: Stage) {
    if on() {
        stamp_at(f, stage, Some(Instant::now()));
    }
}

pub fn stamp_at(f: &Fence, stage: Stage, at: Option<Instant>) {
    if let Some(at) = at {
        let mut s = f.stamps.lock().unwrap_or_else(|e| e.into_inner());
        s.0[stage as usize].get_or_insert(at);
    }
}

/// A guest wait on `f`, started at `from`, has returned.
pub fn waited(f: &Fence, from: Option<Instant>) {
    let Some(from) = from else { return };
    let s = {
        let mut s = f.stamps.lock().unwrap_or_else(|e| e.into_inner());
        if s.0[Stage::WaitReturned as usize].is_some() {
            return;
        }
        s.0[Stage::WaitStarted as usize] = Some(from);
        s.0[Stage::WaitReturned as usize] = Some(Instant::now());
        *s
    };
    if s.0.iter().all(Option::is_some) {
        samples().lock().unwrap_or_else(|e| e.into_inner()).push(s);
    }
}

fn samples() -> &'static Mutex<Vec<Stamps>> {
    static S: OnceLock<Mutex<Vec<Stamps>>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// Print the segments' percentiles (µs), and start over.
pub fn report() {
    if !on() {
        return;
    }
    let samples = std::mem::take(&mut *samples().lock().unwrap_or_else(|e| e.into_inner()));
    if samples.is_empty() {
        return;
    }
    let at = |s: &Stamps, st: Stage| s.0[st as usize].unwrap();
    let us = |a: Instant, b: Instant| b.saturating_duration_since(a).as_secs_f64() * 1e6;
    let segments: [(&str, Stage, Stage); 7] = [
        ("execbuffer: before virglrenderer", Stage::Entered, Stage::Calling),
        ("execbuffer: virgl_renderer_submit_cmd", Stage::Calling, Stage::Submitted),
        ("execbuffer: make the fence", Stage::Submitted, Stage::Fenced),
        ("execbuffer: return", Stage::Fenced, Stage::Returned),
        ("fence made -> signalled (worker, GPU, proxy)", Stage::Fenced, Stage::Signaled),
        ("signalled -> guest wait returns", Stage::Signaled, Stage::WaitReturned),
        ("execbuffer entered -> wait returns", Stage::Entered, Stage::WaitReturned),
    ];
    eprintln!("macwl-run: GPU trace, {} submissions (µs: p50 / p90 / p99)", samples.len());
    for (name, a, b) in segments {
        let mut v: Vec<f64> = samples.iter().map(|s| us(at(s, a), at(s, b))).collect();
        v.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        eprintln!("  {name:48} {:8.1} {:8.1} {:8.1}", p(0.5), p(0.9), p(0.99));
    }
    // A wait that starts after the fence signalled doesn't wait.
    let late = samples.iter().filter(|s| at(s, Stage::WaitStarted) >= at(s, Stage::Signaled)).count();
    eprintln!("  (waits starting after the signal: {late})");
}

/// With `MACWL_GPU_TRACE=2`, each execbuffer, briefly (the first 40).
pub fn execbuffer(ctx: u32, flags: u32, len: u32, ring: u32, n_out: u32, cmd: &[u8]) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static VERBOSE: OnceLock<bool> = OnceLock::new();
    if !*VERBOSE.get_or_init(|| std::env::var_os("MACWL_GPU_TRACE").is_some_and(|v| v == "2")) || N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 40 {
        return;
    }
    let head: Vec<String> = cmd.chunks(4).take(6).map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().unwrap_or([0; 4])))).collect();
    eprintln!("execbuffer ctx {ctx} flags {flags:#x} ring {ring} out {n_out} len {len}: {}", head.join(" "));
}
