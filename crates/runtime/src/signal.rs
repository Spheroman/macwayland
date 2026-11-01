//! Running guest signal handlers: Linux arm64 signal frames on the guest
//! stack, `rt_sigreturn`, and kicking the vCPU out when a host signal comes in.
//!
//! The frame is Linux's `struct rt_sigframe` — `siginfo`, then a `ucontext`
//! whose `sigcontext` holds the registers and, in its reserved area, an
//! FP/SIMD record — followed by a frame record (x29, x30) for unwinders.
//! Handlers that don't name their own return path (SA_RESTORER: musl does,
//! glibc relies on the vDSO) return to a two-instruction `rt_sigreturn`
//! trampoline the runtime maps in every address space.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use linux_abi::{
    syscall::{HandlerCall, StackT},
    GuestMemory,
};

use crate::{hv::UserContext, memory::Space};

/// Where the `rt_sigreturn` trampoline is: above the stack, below the top of
/// user memory.
pub const TRAMPOLINE_VA: u64 = crate::memory::USER_TOP - 0x8000;
/// `mov x8, #139` (rt_sigreturn); `svc #0`.
pub const TRAMPOLINE: [u32; 2] = [0xd280_1168, 0xd400_0001];

/// `sizeof(struct rt_sigframe)` on arm64.
const FRAME_SIZE: u64 = 4688;
const UC: usize = 128;
const UC_STACK: usize = UC + 16;
const UC_SIGMASK: usize = UC + 40;
const MCONTEXT: usize = UC + 176;
const MC_FAULT_ADDRESS: usize = MCONTEXT;
const MC_REGS: usize = MCONTEXT + 8;
const MC_SP: usize = MCONTEXT + 256;
const MC_PC: usize = MCONTEXT + 264;
const MC_PSTATE: usize = MCONTEXT + 272;
const MC_RESERVED: usize = MCONTEXT + 288;
const MC_RESERVED_SIZE: usize = 4096;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;
/// PSTATE bits a handler (or sigreturn) may set: the condition flags.
const PSTATE_USER: u64 = 0xf000_0000;

fn put(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

fn get64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn get32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// Set the guest up to run `call`'s handler, with a frame saving `ctx` (the
/// state it interrupted). `fault_address` is `sigcontext.fault_address`.
/// Fails if the frame can't be written (a bad stack): Linux then kills with
/// SIGSEGV.
pub(crate) fn deliver(vcpu: &mut crate::hv::Vcpu, space: &Space, call: &HandlerCall, altstack: StackT, ctx: &UserContext, fault_address: u64) -> Result<(), ()> {
    let on_altstack = altstack.contains(ctx.sp);
    let top = match call.altstack {
        Some(alt) if !on_altstack => alt.sp + alt.size,
        _ => ctx.sp,
    };
    let frame = (top & !15).checked_sub(16 + FRAME_SIZE).ok_or(())? & !15;
    let record = frame + FRAME_SIZE;

    let mut b = vec![0u8; FRAME_SIZE as usize + 16];
    put(&mut b, 0, &call.info.to_bytes());
    // uc_stack: the alternate stack as it was, and whether we were on it.
    let ss_flags = if on_altstack { SS_ONSTACK } else if altstack.enabled() { 0 } else { SS_DISABLE };
    put(&mut b, UC_STACK, &altstack.sp.to_le_bytes());
    put(&mut b, UC_STACK + 8, &ss_flags.to_le_bytes());
    put(&mut b, UC_STACK + 16, &altstack.size.to_le_bytes());
    put(&mut b, UC_SIGMASK, &call.saved_mask.to_le_bytes());
    put(&mut b, MC_FAULT_ADDRESS, &fault_address.to_le_bytes());
    for (i, r) in ctx.x.iter().enumerate() {
        put(&mut b, MC_REGS + i * 8, &r.to_le_bytes());
    }
    put(&mut b, MC_SP, &ctx.sp.to_le_bytes());
    put(&mut b, MC_PC, &ctx.pc.to_le_bytes());
    put(&mut b, MC_PSTATE, &(ctx.pstate & !0xf).to_le_bytes());
    // The FP/SIMD record, then the terminating empty record.
    put(&mut b, MC_RESERVED, &FPSIMD_MAGIC.to_le_bytes());
    put(&mut b, MC_RESERVED + 4, &FPSIMD_SIZE.to_le_bytes());
    put(&mut b, MC_RESERVED + 8, &(ctx.fpsr as u32).to_le_bytes());
    put(&mut b, MC_RESERVED + 12, &(ctx.fpcr as u32).to_le_bytes());
    for (i, v) in ctx.v.iter().enumerate() {
        put(&mut b, MC_RESERVED + 16 + i * 16, &v.to_le_bytes());
    }
    // The frame record: what the interrupted code's frame chain continues from.
    put(&mut b, FRAME_SIZE as usize, &ctx.x[29].to_le_bytes());
    put(&mut b, FRAME_SIZE as usize + 8, &ctx.x[30].to_le_bytes());
    space.write(frame, &b).map_err(|_| ())?;

    let mut h = ctx.clone();
    h.x[0] = call.info.signo;
    h.x[1] = frame;
    h.x[2] = frame + UC as u64;
    h.x[29] = record;
    h.x[30] = call.restorer.unwrap_or(TRAMPOLINE_VA);
    h.sp = frame;
    h.pc = call.handler;
    h.pstate = 0;
    h.restore(vcpu).map_err(|_| ())
}

/// `rt_sigreturn`: restore what the frame at the guest's stack pointer saved.
/// Returns the signal mask to go back to, or Err for a bad frame (Linux kills
/// with SIGSEGV).
pub(crate) fn sigreturn(vcpu: &mut crate::hv::Vcpu, space: &Space) -> Result<u64, ()> {
    let cur = UserContext::save(vcpu).map_err(|_| ())?;
    let frame = cur.sp;
    if frame % 16 != 0 {
        return Err(());
    }
    let mut b = vec![0u8; FRAME_SIZE as usize];
    space.read(frame, &mut b).map_err(|_| ())?;
    let mut ctx = cur.clone();
    for (i, r) in ctx.x.iter_mut().enumerate() {
        *r = get64(&b, MC_REGS + i * 8);
    }
    ctx.sp = get64(&b, MC_SP);
    ctx.pc = get64(&b, MC_PC);
    // Only the flags are the program's to set: it returns to EL0 regardless.
    ctx.pstate = get64(&b, MC_PSTATE) & PSTATE_USER;
    // Find the FP/SIMD record among the reserved area's records.
    let mut off = MC_RESERVED;
    while off + 8 <= MC_RESERVED + MC_RESERVED_SIZE {
        let (magic, size) = (get32(&b, off), get32(&b, off + 4) as usize);
        if magic == 0 || size < 8 {
            break;
        }
        if magic == FPSIMD_MAGIC && size >= FPSIMD_SIZE as usize {
            ctx.fpsr = get32(&b, off + 8) as u64;
            ctx.fpcr = get32(&b, off + 12) as u64;
            for (i, v) in ctx.v.iter_mut().enumerate() {
                *v = u128::from_le_bytes(b[off + 16 + i * 16..off + 32 + i * 16].try_into().unwrap());
            }
        }
        off += size;
    }
    ctx.restore(vcpu).map_err(|_| ())?;
    Ok(get64(&b, UC_SIGMASK))
}

/// The vCPUs a host signal should interrupt (u64::MAX: free slot). A fixed
/// table, so the signal handler can read it without locks.
static VCPUS: [AtomicU64; 64] = [const { AtomicU64::new(u64::MAX) }; 64];

/// The host thread running each vCPU in [`VCPUS`] (0: none).
static HOST_THREADS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];

/// Make vCPU `id`, run by this host thread, one that host signals kick out of
/// the guest. Returns its slot, for [`remove_vcpu`].
pub(crate) fn add_vcpu(id: u64) -> Option<usize> {
    linux_abi::syscall::set_kick(kick);
    linux_abi::syscall::set_interrupt(interrupt_all);
    let slot = VCPUS.iter().position(|slot| slot.compare_exchange(u64::MAX, id, Ordering::AcqRel, Ordering::Relaxed).is_ok())?;
    HOST_THREADS[slot].store(unsafe { libc::pthread_self() } as usize, Ordering::Release);
    Some(slot)
}

/// Interrupt the host call the CPU in `slot` is blocked in (not the guest).
pub(crate) fn interrupt_slot(slot: usize) {
    let t = HOST_THREADS[slot].load(Ordering::Acquire);
    if t != 0 {
        unsafe { libc::pthread_kill(t as libc::pthread_t, INTERRUPT) };
    }
}

/// Worker threads (host threads making guest system calls without a vCPU),
/// for [`interrupt_all`] (0: free slot). Past this many, a worker isn't
/// interrupted for a timer's signal (its call ends in its own time).
static WORKERS: [AtomicUsize; 512] = [const { AtomicUsize::new(0) }; 512];

/// This host thread is a worker. Returns its slot, for [`remove_worker`].
pub(crate) fn add_worker() -> Option<usize> {
    let me = unsafe { libc::pthread_self() } as usize;
    WORKERS.iter().position(|slot| slot.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_ok())
}

pub(crate) fn remove_worker(slot: usize) {
    WORKERS[slot].store(0, Ordering::Release);
}

/// Make the vCPU in `slot` exit the guest (not its host thread's call).
pub(crate) fn exit_slot(slot: usize) {
    let id = VCPUS[slot].load(Ordering::Acquire);
    if id != u64::MAX {
        crate::hv::exit_vcpu(id);
    }
}

/// Get the attention of what runs on the vCPU in `slot`: out of the guest,
/// and out of any host call its host thread is blocked in.
pub(crate) fn wake_slot(slot: usize) {
    let id = VCPUS[slot].load(Ordering::Acquire);
    if id != u64::MAX {
        crate::hv::exit_vcpu(id);
    }
    let t = HOST_THREADS[slot].load(Ordering::Acquire);
    if t != 0 {
        unsafe { libc::pthread_kill(t as libc::pthread_t, INTERRUPT) };
    }
}

pub(crate) fn remove_vcpu(slot: usize) {
    HOST_THREADS[slot].store(0, Ordering::Release);
    VCPUS[slot].store(u64::MAX, Ordering::Release);
}

/// Break every guest thread out of the host call it's blocked in (for a
/// timer's signal).
fn interrupt_all() {
    for t in HOST_THREADS.iter().chain(&WORKERS) {
        let t = t.load(Ordering::Acquire);
        if t != 0 {
            unsafe { libc::pthread_kill(t as libc::pthread_t, INTERRUPT) };
        }
    }
}

/// Called from host signal handlers: make every vCPU exit (if running, at
/// once; if not, its next run returns straight away), so each thread looks
/// at the signal.
fn kick() {
    for slot in &VCPUS {
        let id = slot.load(Ordering::Acquire);
        if id != u64::MAX {
            crate::hv::exit_vcpu(id);
        }
    }
}

/// The host signal one thread sends another to break it out of a blocking
/// host call (`exit_group`, `execve`, a signal for that thread): SIGEMT,
/// which no Linux signal maps to.
pub(crate) const INTERRUPT: libc::c_int = libc::SIGEMT;

/// Install [`INTERRUPT`]'s handler: nothing, but the call it lands in returns
/// EINTR (no SA_RESTART).
pub(crate) fn install_interrupt() {
    extern "C" fn nothing(_: libc::c_int) {}
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = nothing as *const () as libc::sighandler_t;
    unsafe { libc::sigaction(INTERRUPT, &sa, std::ptr::null_mut()) };
}
