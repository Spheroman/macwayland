//! B1: how many vCPUs can one Hypervisor.framework VM have?
//!
//! Track B maps each Linux thread onto a vCPU, so this cap decides whether we
//! can do 1:1 threads or need to multiplex. Prints the advertised limit, then
//! verifies it by creating vCPUs (one per host thread, as HVF requires) until
//! creation fails.
//!
//! Build, then sign with scripts/sign-hv.sh (needs the hypervisor entitlement).

use std::{
    ffi::c_void,
    ptr,
    sync::mpsc,
};

type HvReturn = i32;
const HV_SUCCESS: HvReturn = 0;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> HvReturn;
    fn hv_vm_get_max_vcpu_count(max_vcpu_count: *mut u32) -> HvReturn;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *mut c_void, config: *mut c_void) -> HvReturn;
    fn hv_vcpu_destroy(vcpu: u64) -> HvReturn;
}

fn main() {
    let mut advertised = 0u32;
    let ret = unsafe { hv_vm_get_max_vcpu_count(&mut advertised) };
    if ret == HV_SUCCESS {
        println!("advertised max vCPUs per VM: {advertised}");
    } else {
        println!("hv_vm_get_max_vcpu_count failed: {ret:#x}");
    }

    let ret = unsafe { hv_vm_create(ptr::null_mut()) };
    if ret != HV_SUCCESS {
        eprintln!("hv_vm_create failed: {ret:#x} (is the binary signed with the hypervisor entitlement?)");
        std::process::exit(1);
    }

    // Each vCPU belongs to the thread that created it, so spawn threads that each
    // create one and park until we've found the limit.
    let attempts = 1024;
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    let mut created = 0;
    for i in 0..attempts {
        let tx = tx.clone();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut vcpu = 0u64;
            let mut exit: *mut c_void = ptr::null_mut();
            let ret = unsafe { hv_vcpu_create(&mut vcpu, &mut exit, ptr::null_mut()) };
            tx.send(ret).unwrap();
            if ret == HV_SUCCESS {
                go_rx.recv().ok();
                unsafe { hv_vcpu_destroy(vcpu) };
            }
        });
        let ret = rx.recv().unwrap();
        if ret != HV_SUCCESS {
            println!("vCPU #{} failed: {ret:#x}", i + 1);
            handle.join().unwrap();
            break;
        }
        created += 1;
        handles.push((handle, go_tx));
    }
    println!("created {created} vCPUs concurrently in one VM");

    for (handle, go) in handles {
        go.send(()).ok();
        handle.join().unwrap();
    }
}
