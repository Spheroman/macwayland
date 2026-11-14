//! B1: what Hypervisor.framework allows.
//!
//! - `hv-limits` (no arguments): how many vCPUs can one VM have? Track B maps
//!   each Linux thread onto a vCPU, so this cap decides whether we can do 1:1
//!   threads or need to multiplex. Prints the advertised limit, then verifies
//!   it by creating vCPUs (one per host thread, as HVF requires) until
//!   creation fails.
//! - `hv-limits vms [--max N] [--map GiB] [--vcpus K] [--ipa BITS]`: how many
//!   VMs can exist at once, system-wide? Track B runs one VM per Linux
//!   process. Spawns child processes that each create a VM (with an IPA size
//!   of BITS, if given), map GiB of untouched host memory into it, and create
//!   K vCPUs, until one fails or N exist; then releases them all.
//!
//! Build, then sign with scripts/sign-hv.sh (needs the hypervisor entitlement).

use std::{
    ffi::c_void,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    ptr,
    sync::mpsc,
    time::Instant,
};

type HvReturn = i32;
const HV_SUCCESS: HvReturn = 0;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> HvReturn;
    fn hv_vm_get_max_vcpu_count(max_vcpu_count: *mut u32) -> HvReturn;
    fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> HvReturn;
    fn hv_vm_config_create() -> *mut c_void;
    fn hv_vm_config_set_ipa_size(config: *mut c_void, ipa_bit_length: u32) -> HvReturn;
    fn hv_vm_config_get_max_ipa_size(ipa_bit_length: *mut u32) -> HvReturn;
    fn hv_vm_config_get_default_ipa_size(ipa_bit_length: *mut u32) -> HvReturn;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *mut c_void, config: *mut c_void) -> HvReturn;
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => vcpus(),
        Some("vms") => vms(&args[1..]),
        Some("--vm-child") => vm_child(&args[1..]),
        Some(other) => {
            eprintln!("unknown mode {other:?}; usage: hv-limits [vms [--max N] [--map GiB] [--vcpus K] [--ipa BITS]]");
            std::process::exit(2);
        }
    }
}

fn vcpus() {
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

    let (created, err) = create_vcpus(1024);
    if let Some(err) = err {
        println!("vCPU #{} failed: {err:#x}", created + 1);
    }
    println!("created {created} vCPUs concurrently in one VM");
}

/// Create up to `n` vCPUs, each on its own thread (a vCPU belongs to the
/// thread that created it), and leave them parked for the life of the
/// process. Returns how many were created, and the error that stopped it.
fn create_vcpus(n: u32) -> (u32, Option<HvReturn>) {
    let (tx, rx) = mpsc::channel();
    for i in 0..n {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut vcpu = 0u64;
            let mut exit: *mut c_void = ptr::null_mut();
            let ret = unsafe { hv_vcpu_create(&mut vcpu, &mut exit, ptr::null_mut()) };
            tx.send(ret).unwrap();
            if ret == HV_SUCCESS {
                loop {
                    std::thread::park();
                }
            }
        });
        let ret = rx.recv().unwrap();
        if ret != HV_SUCCESS {
            return (i, Some(ret));
        }
    }
    (n, None)
}

struct Options {
    max: u32,
    map_gib: u64,
    vcpus: u32,
    ipa: Option<u32>,
}

fn parse(args: &[String]) -> Options {
    let mut o = Options { max: 2000, map_gib: 0, vcpus: 0, ipa: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| {
            eprintln!("{a} needs a number");
            std::process::exit(2);
        });
        match a.as_str() {
            "--max" => o.max = v as u32,
            "--map" => o.map_gib = v,
            "--vcpus" => o.vcpus = v as u32,
            "--ipa" => o.ipa = Some(v as u32),
            _ => {
                eprintln!("unknown option {a}");
                std::process::exit(2);
            }
        }
    }
    o
}

/// One VM, set up as asked; reports "ok" or the failure on stdout, then holds
/// everything until stdin closes.
fn vm_child(args: &[String]) {
    let o = parse(args);
    let fail = |what: &str, ret: HvReturn| -> ! {
        println!("{what} {:#x}", ret as u32);
        std::process::exit(1);
    };
    let config = match o.ipa {
        None => ptr::null_mut(),
        Some(bits) => {
            let c = unsafe { hv_vm_config_create() };
            let ret = unsafe { hv_vm_config_set_ipa_size(c, bits) };
            if ret != HV_SUCCESS {
                fail("hv_vm_config_set_ipa_size", ret);
            }
            c
        }
    };
    let ret = unsafe { hv_vm_create(config) };
    if ret != HV_SUCCESS {
        fail("hv_vm_create", ret);
    }
    if o.map_gib > 0 {
        let size = (o.map_gib << 30) as usize;
        // As the runtime's pool: reserved, never touched.
        let p = unsafe {
            libc_mmap(ptr::null_mut(), size, 3 /* RW */, 0x1000 | 0x0002 | 0x0040 /* ANON|PRIVATE|NORESERVE */, -1, 0)
        };
        if p as isize == -1 {
            println!("mmap failed");
            std::process::exit(1);
        }
        let ret = unsafe { hv_vm_map(p, 0, size, 7) };
        if ret != HV_SUCCESS {
            fail("hv_vm_map", ret);
        }
    }
    let (created, err) = create_vcpus(o.vcpus);
    if let Some(err) = err {
        fail(&format!("hv_vcpu_create #{}", created + 1), err);
    }
    println!("ok");
    std::io::stdout().flush().ok();
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);
    // Exiting tears the VM down.
}

unsafe extern "C" {
    #[link_name = "mmap"]
    fn libc_mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
}

fn vms(args: &[String]) {
    let o = parse(args);
    let (mut max_ipa, mut default_ipa) = (0, 0);
    unsafe {
        hv_vm_config_get_max_ipa_size(&mut max_ipa);
        hv_vm_config_get_default_ipa_size(&mut default_ipa);
    }
    println!(
        "IPA bits: default {default_ipa}, max {max_ipa}; each VM: map {} GiB, {} vCPUs, IPA {}",
        o.map_gib,
        o.vcpus,
        o.ipa.map_or("default".into(), |b| b.to_string())
    );
    let me = std::env::current_exe().unwrap();
    let mut children: Vec<Child> = Vec::new();
    let start = Instant::now();
    let mut failure = None;
    while children.len() < o.max as usize {
        let mut child = match Command::new(&me)
            .arg("--vm-child")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                failure = Some(format!("spawn: {e}"));
                break;
            }
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).ok();
        if line.trim() != "ok" {
            failure = Some(line.trim().to_string());
            drop(child.stdin.take());
            child.wait().ok();
            break;
        }
        children.push(child);
    }
    let held = start.elapsed();
    println!(
        "{} VMs live at once{} ({:.1}s)",
        children.len(),
        failure.map_or(String::new(), |f| format!("; next failed: {f}")),
        held.as_secs_f64()
    );
    for c in &mut children {
        drop(c.stdin.take());
    }
    for c in &mut children {
        c.wait().ok();
    }
}
