//! Minimal Hypervisor.framework (arm64) bindings: one VM, vCPUs, registers.

use std::{ffi::c_void, fmt, marker::PhantomData, ptr};

type HvReturn = i32;
const HV_SUCCESS: HvReturn = 0;
const HV_DENIED: HvReturn = 0xfae94007_u32 as i32;

pub const MEMORY_READ: u64 = 1 << 0;
pub const MEMORY_WRITE: u64 = 1 << 1;
pub const MEMORY_EXEC: u64 = 1 << 2;

pub const EXIT_REASON_CANCELED: u32 = 0;
pub const EXIT_REASON_EXCEPTION: u32 = 1;
pub const EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;

/// General registers (`hv_reg_t`): X0–X30 are 0–30.
pub mod reg {
    pub const X0: u32 = 0;
    pub const X8: u32 = 8;
    pub const PC: u32 = 31;
    pub const FPCR: u32 = 32;
    pub const FPSR: u32 = 33;
    pub const CPSR: u32 = 34;
}

/// System registers (`hv_sys_reg_t`), encoded op0:op1:CRn:CRm:op2.
pub mod sys_reg {
    pub const SCTLR_EL1: u16 = 0xc080;
    pub const CPACR_EL1: u16 = 0xc082;
    pub const TTBR0_EL1: u16 = 0xc100;
    pub const TTBR1_EL1: u16 = 0xc101;
    pub const TCR_EL1: u16 = 0xc102;
    pub const SPSR_EL1: u16 = 0xc200;
    pub const ELR_EL1: u16 = 0xc201;
    pub const SP_EL0: u16 = 0xc208;
    pub const ESR_EL1: u16 = 0xc290;
    pub const FAR_EL1: u16 = 0xc300;
    pub const MAIR_EL1: u16 = 0xc510;
    pub const VBAR_EL1: u16 = 0xc600;
    pub const SP_EL1: u16 = 0xe208;
    pub const TPIDR_EL0: u16 = 0xde82;
    pub const TPIDRRO_EL0: u16 = 0xde83;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExitException {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

/// `hv_vcpu_exit_t`, filled in by every `hv_vcpu_run`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Exit {
    pub reason: u32,
    pub exception: ExitException,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> HvReturn;
    fn hv_vm_destroy() -> HvReturn;
    fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> HvReturn;
    fn hv_vm_unmap(ipa: u64, size: usize) -> HvReturn;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const Exit, config: *mut c_void) -> HvReturn;
    fn hv_vcpu_destroy(vcpu: u64) -> HvReturn;
    fn hv_vcpu_run(vcpu: u64) -> HvReturn;
    fn hv_vcpus_exit(vcpus: *const u64, count: u32) -> HvReturn;
    fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> HvReturn;
    fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> HvReturn;
    fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> HvReturn;
    fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> HvReturn;
    /// `value` is a 16-byte vector; u128 has its size and alignment.
    fn hv_vcpu_get_simd_fp_reg(vcpu: u64, reg: u32, value: *mut u128) -> HvReturn;
    /// Takes the value by vector register (v0), which Rust can't declare
    /// for FFI: called through `asm!` in [`Vcpu::set_simd_reg`].
    fn hv_vcpu_set_simd_fp_reg();
}

/// A failed Hypervisor.framework call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HvError {
    pub call: &'static str,
    pub code: i32,
}

impl fmt::Display for HvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {:#x}", self.call, self.code as u32)?;
        if self.code == HV_DENIED {
            write!(f, " (missing the hypervisor entitlement? sign with scripts/sign-hv.sh)")?;
        }
        Ok(())
    }
}

impl std::error::Error for HvError {}

fn check(call: &'static str, code: HvReturn) -> Result<(), HvError> {
    if code == HV_SUCCESS { Ok(()) } else { Err(HvError { call, code }) }
}

/// The process's VM. Hypervisor.framework allows one per process.
pub struct Vm(());

impl Vm {
    pub fn new() -> Result<Self, HvError> {
        check("hv_vm_create", unsafe { hv_vm_create(ptr::null_mut()) })?;
        Ok(Vm(()))
    }

    /// Map host memory into the guest's physical address space at `ipa`.
    ///
    /// # Safety
    /// `host` must stay mapped and valid for `size` bytes until the VM is
    /// dropped. Both must be host-page (16 KiB) aligned.
    pub unsafe fn map(&self, host: *mut u8, ipa: u64, size: usize, flags: u64) -> Result<(), HvError> {
        unsafe { map(host, ipa, size, flags) }
    }

    /// Destroy the VM, saying whether that worked (dropping it can't). It
    /// fails while any vCPU still exists.
    pub fn destroy(self) -> Result<(), HvError> {
        std::mem::forget(self);
        check("hv_vm_destroy", unsafe { hv_vm_destroy() })
    }
}

/// [`Vm::map`] without the handle. Hypervisor.framework's VM is per-process
/// state, so this works whenever a [`Vm`] exists (and fails otherwise).
///
/// # Safety
/// As for [`Vm::map`]; the mapping must also be removed with [`unmap`] (or
/// the VM destroyed) before the host memory goes away.
pub unsafe fn map(host: *mut u8, ipa: u64, size: usize, flags: u64) -> Result<(), HvError> {
    check("hv_vm_map", unsafe { hv_vm_map(host.cast(), ipa, size, flags) })
}

/// Remove a guest physical range mapped with [`map`].
pub fn unmap(ipa: u64, size: usize) -> Result<(), HvError> {
    check("hv_vm_unmap", unsafe { hv_vm_unmap(ipa, size) })
}

impl Drop for Vm {
    fn drop(&mut self) {
        unsafe { hv_vm_destroy() };
    }
}

/// Make vCPU `id` exit to the host as soon as possible (any thread; also from
/// a signal handler).
pub fn exit_vcpu(id: u64) {
    unsafe { hv_vcpus_exit(&id, 1) };
}

/// A vCPU. Hypervisor.framework ties it to the creating thread, so it is
/// neither `Send` nor `Sync`.
pub struct Vcpu<'vm> {
    id: u64,
    exit: *const Exit,
    _vm: PhantomData<&'vm Vm>,
}

impl<'vm> Vcpu<'vm> {
    pub fn new(_vm: &'vm Vm) -> Result<Self, HvError> {
        let mut id = 0;
        let mut exit = ptr::null();
        check("hv_vcpu_create", unsafe { hv_vcpu_create(&mut id, &mut exit, ptr::null_mut()) })?;
        Ok(Vcpu { id, exit, _vm: PhantomData })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Run the guest until it exits to the host, and return why.
    pub fn run(&mut self) -> Result<Exit, HvError> {
        check("hv_vcpu_run", unsafe { hv_vcpu_run(self.id) })?;
        Ok(unsafe { *self.exit })
    }

    pub fn reg(&self, reg: u32) -> Result<u64, HvError> {
        let mut v = 0;
        check("hv_vcpu_get_reg", unsafe { hv_vcpu_get_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    pub fn set_reg(&mut self, reg: u32, value: u64) -> Result<(), HvError> {
        check("hv_vcpu_set_reg", unsafe { hv_vcpu_set_reg(self.id, reg, value) })
    }

    pub fn sys_reg(&self, reg: u16) -> Result<u64, HvError> {
        let mut v = 0;
        check("hv_vcpu_get_sys_reg", unsafe { hv_vcpu_get_sys_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    pub fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<(), HvError> {
        check("hv_vcpu_set_sys_reg", unsafe { hv_vcpu_set_sys_reg(self.id, reg, value) })
    }

    /// SIMD/FP register Q`reg` (0–31).
    pub fn simd_reg(&self, reg: u32) -> Result<u128, HvError> {
        let mut v = 0u128;
        check("hv_vcpu_get_simd_fp_reg", unsafe { hv_vcpu_get_simd_fp_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    pub fn set_simd_reg(&mut self, reg: u32, value: u128) -> Result<(), HvError> {
        let ret: i32;
        // hv_vcpu_set_simd_fp_reg(vcpu: x0, reg: w1, value: v0) per AAPCS64.
        unsafe {
            std::arch::asm!(
                "ldr q0, [{value}]",
                "bl {f}",
                value = in(reg) &value,
                f = sym hv_vcpu_set_simd_fp_reg,
                in("x0") self.id,
                in("w1") reg,
                lateout("w0") ret,
                clobber_abi("C"),
            );
        }
        check("hv_vcpu_set_simd_fp_reg", ret)
    }
}

/// Everything the guest program can see of a CPU: what a forked child
/// resumes with (and, later, what a signal frame saves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserContext {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    /// PSTATE as EL0 had it (SPSR_EL1's view): flags, and EL0t.
    pub pstate: u64,
    pub tpidr: u64,
    pub tpidrro: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub v: [u128; 32],
}

impl UserContext {
    pub fn encode(&self, e: &mut linux_abi::wire::Encoder) {
        for &r in &self.x {
            e.u64(r);
        }
        e.u64(self.sp).u64(self.pc).u64(self.pstate).u64(self.tpidr).u64(self.tpidrro).u64(self.fpcr).u64(self.fpsr);
        for &v in &self.v {
            e.u128(v);
        }
    }

    pub fn decode(d: &mut linux_abi::wire::Decoder) -> Result<Self, linux_abi::wire::Truncated> {
        let mut x = [0; 31];
        for r in &mut x {
            *r = d.u64()?;
        }
        let (sp, pc, pstate, tpidr, tpidrro, fpcr, fpsr) = (d.u64()?, d.u64()?, d.u64()?, d.u64()?, d.u64()?, d.u64()?, d.u64()?);
        let mut v = [0; 32];
        for r in &mut v {
            *r = d.u128()?;
        }
        Ok(UserContext { x, sp, pc, pstate, tpidr, tpidrro, fpcr, fpsr, v })
    }

    /// The guest's state: if the vCPU is in our EL1 vector (a system call or
    /// fault), EL0's PC and PSTATE are in ELR_EL1 and SPSR_EL1; if it was
    /// stopped in the program itself, they're the live PC and CPSR.
    pub fn save(vcpu: &Vcpu) -> Result<Self, HvError> {
        let in_vector = vcpu.reg(reg::CPSR)? & 0xf != 0;
        let (pc, pstate) = if in_vector {
            (vcpu.sys_reg(sys_reg::ELR_EL1)?, vcpu.sys_reg(sys_reg::SPSR_EL1)?)
        } else {
            (vcpu.reg(reg::PC)?, vcpu.reg(reg::CPSR)?)
        };
        let mut x = [0; 31];
        for (i, r) in x.iter_mut().enumerate() {
            *r = vcpu.reg(reg::X0 + i as u32)?;
        }
        let mut v = [0; 32];
        for (i, r) in v.iter_mut().enumerate() {
            *r = vcpu.simd_reg(i as u32)?;
        }
        Ok(UserContext {
            x,
            sp: vcpu.sys_reg(sys_reg::SP_EL0)?,
            pc,
            pstate,
            tpidr: vcpu.sys_reg(sys_reg::TPIDR_EL0)?,
            tpidrro: vcpu.sys_reg(sys_reg::TPIDRRO_EL0)?,
            fpcr: vcpu.reg(reg::FPCR)?,
            fpsr: vcpu.reg(reg::FPSR)?,
            v,
        })
    }

    /// Set a (configured) vCPU up to resume the guest at EL0 with this state:
    /// directly, or (with ELR_EL1/SPSR_EL1 set to match) through the vector's
    /// TLB-flushing return path.
    pub fn restore(&self, vcpu: &mut Vcpu) -> Result<(), HvError> {
        vcpu.set_sys_reg(sys_reg::ELR_EL1, self.pc)?;
        vcpu.set_sys_reg(sys_reg::SPSR_EL1, self.pstate & !0xf)?;
        for (i, &r) in self.x.iter().enumerate() {
            vcpu.set_reg(reg::X0 + i as u32, r)?;
        }
        for (i, &r) in self.v.iter().enumerate() {
            vcpu.set_simd_reg(i as u32, r)?;
        }
        vcpu.set_sys_reg(sys_reg::SP_EL0, self.sp)?;
        vcpu.set_sys_reg(sys_reg::TPIDR_EL0, self.tpidr)?;
        vcpu.set_sys_reg(sys_reg::TPIDRRO_EL0, self.tpidrro)?;
        vcpu.set_reg(reg::FPCR, self.fpcr)?;
        vcpu.set_reg(reg::FPSR, self.fpsr)?;
        vcpu.set_reg(reg::PC, self.pc)?;
        // Mode bits 0: EL0t, so the vCPU starts straight in the program.
        vcpu.set_reg(reg::CPSR, self.pstate & !0xf)
    }
}

impl Drop for Vcpu<'_> {
    fn drop(&mut self) {
        unsafe { hv_vcpu_destroy(self.id) };
    }
}
