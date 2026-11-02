//! What a VM shared by many processes costs to set up and keep
//! (docs/processes.md): creating a VM with a large IPA space, mapping big
//! reserved pools and small shared windows into it, and what a fork inside
//! the VM would cost: copying a process's page tables with every private
//! page made copy-on-write. Sign first:
//! scripts/sign-hv.sh target/release/examples/vm_cost

use std::{ffi::c_void, ptr, time::Instant};

use macwl_runtime::paging::{self, prot, PageTable, PhysMem, PAGE_SIZE};

type HvReturn = i32;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> HvReturn;
    fn hv_vm_destroy() -> HvReturn;
    fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> HvReturn;
    fn hv_vm_unmap(ipa: u64, size: usize) -> HvReturn;
    fn hv_vm_config_create() -> *mut c_void;
    fn hv_vm_config_set_ipa_size(config: *mut c_void, ipa_bit_length: u32) -> HvReturn;
    fn hv_vm_config_get_max_ipa_size(ipa_bit_length: *mut u32) -> HvReturn;
    fn hv_vm_config_get_default_ipa_size(ipa_bit_length: *mut u32) -> HvReturn;
}

const RWX: u64 = 7;

fn us(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e6
}

fn reserve(len: usize) -> *mut c_void {
    let p = unsafe { libc::mmap(ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE, -1, 0) };
    if p == libc::MAP_FAILED { ptr::null_mut() } else { p }
}

fn create(bits: Option<u32>) -> HvReturn {
    let config = match bits {
        None => ptr::null_mut(),
        Some(b) => {
            let c = unsafe { hv_vm_config_create() };
            assert_eq!(unsafe { hv_vm_config_set_ipa_size(c, b) }, 0, "IPA {b} bits");
            c
        }
    };
    unsafe { hv_vm_create(config) }
}

fn main() {
    let (mut max, mut default) = (0, 0);
    unsafe {
        hv_vm_config_get_max_ipa_size(&mut max);
        hv_vm_config_get_default_ipa_size(&mut default);
    }
    println!("IPA bits: default {default}, max {max}");

    // Creating and destroying a VM, by IPA size.
    for bits in [None, Some(40), Some(max)] {
        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            assert_eq!(create(bits), 0);
            assert_eq!(unsafe { hv_vm_destroy() }, 0);
        }
        println!("VM create + destroy, IPA {}: {:.0} µs", bits.map_or("default".into(), |b| b.to_string()), us(t) / n as f64);
    }

    // Mapping a reserved (untouched) pool, by size, in a 40-bit VM (or the
    // largest there is).
    let bits = max.min(40).max(default);
    assert_eq!(create(Some(bits)), 0);
    for gib in [8u64, 64, 256, 512, 1000] {
        let len = (gib << 30) as usize;
        if gib << 30 > 1u64 << bits {
            continue;
        }
        let t = Instant::now();
        let p = reserve(len);
        let reserved = us(t);
        if p.is_null() {
            println!("pool {gib} GiB: mmap failed");
            continue;
        }
        let t = Instant::now();
        let r = unsafe { hv_vm_map(p, 0, len, RWX) };
        let mapped = us(t);
        let t = Instant::now();
        let u = unsafe { hv_vm_unmap(0, len) };
        let unmapped = us(t);
        unsafe { libc::munmap(p, len) };
        println!("pool {gib} GiB: mmap {reserved:.0} µs, hv_vm_map {mapped:.0} µs ({r:#x}), hv_vm_unmap {unmapped:.0} µs ({u:#x})");
    }

    // Shared windows: map and unmap a shm mapping, by size (each process
    // does this for every MAP_SHARED mapping today; in one VM, once).
    for kib in [16usize, 1024, 64 << 10] {
        let len = kib << 10;
        let fd = unsafe { libc::shm_open(c"/macwl.vmcost".as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600) };
        unsafe {
            libc::shm_unlink(c"/macwl.vmcost".as_ptr());
            libc::ftruncate(fd, len as i64);
        }
        let host = unsafe { libc::mmap(ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        let n = 2000;
        let t = Instant::now();
        for _ in 0..n {
            assert_eq!(unsafe { hv_vm_map(host, 1 << 36, len, RWX) }, 0);
            assert_eq!(unsafe { hv_vm_unmap(1 << 36, len) }, 0);
        }
        println!("window {kib} KiB: hv_vm_map + hv_vm_unmap {:.1} µs", us(t) / n as f64);
        unsafe {
            libc::munmap(host, len);
            libc::close(fd);
        }
    }
    assert_eq!(unsafe { hv_vm_destroy() }, 0);

    // Fork inside the VM: copy the page tables of a process with `pages`
    // private pages mapped, making both copies read-only (copy on write) and
    // counting references to each frame.
    println!("fork in the VM (page tables copied, private pages made copy-on-write):");
    for mb in [4u64, 32, 200, 1024] {
        let pages = (mb << 20) / PAGE_SIZE;
        let mut phys = PhysMem::new().unwrap();
        let parent = PageTable::new(&mut phys).unwrap();
        let rw = paging::user_attrs(prot::READ | prot::WRITE);
        // As a heap and libraries would be: a few regions.
        let base = 0x5555_0000_0000u64;
        for i in 0..pages {
            let f = phys.alloc().unwrap();
            parent.map(&mut phys, base + (i / 4096) * (1 << 30) + (i % 4096) * PAGE_SIZE, f, rw).unwrap();
        }
        let mut refs = vec![0u16; (phys.frames_in_use() + 64) as usize];
        let t = Instant::now();
        let child = PageTable::new(&mut phys).unwrap();
        const ADDR: u64 = 0x0000_ffff_ffff_f000;
        const RO: u64 = 1 << 7;
        parent.for_each_mapped(&mut phys, 0, 1 << 48, &mut |phys, va, slot| {
            let e = phys.read_u64(slot) | RO;
            phys.write_u64(slot, e);
            child.map(phys, va, e & ADDR, e & !ADDR).unwrap();
            refs[((e & ADDR) / PAGE_SIZE) as usize] += 1;
        });
        let copy = us(t);
        // The copy-on-write fault's own work, after the VM exit: copy the
        // frame, point the entry at it.
        let n = 10_000.min(pages);
        let t = Instant::now();
        for i in 0..n {
            let va = base + (i / 4096) * (1 << 30) + (i % 4096) * PAGE_SIZE;
            let old = child.lookup(&phys, va).unwrap();
            let new = phys.alloc().unwrap();
            unsafe { ptr::copy_nonoverlapping(phys.ptr(old), phys.ptr(new), PAGE_SIZE as usize) };
            child.for_each_mapped(&mut phys, va, va + PAGE_SIZE, &mut |phys, _, slot| phys.write_u64(slot, new | rw));
            refs[(old / PAGE_SIZE) as usize] -= 1;
        }
        let fault = us(t) / n as f64;
        println!("  {mb:>5} MB ({pages} pages): copy {:.0} µs ({:.0} ns/page); a COW fault's host work {fault:.2} µs", copy, copy * 1e3 / pages as f64);
        std::hint::black_box(&refs);
    }
}
