//! What moving a guest thread between vCPUs costs: saving one thread's
//! registers from a vCPU and restoring another's (M:N threading's context
//! switch), and creating a vCPU (what reusing one saves). Then what a switch
//! between *processes* in one VM adds (docs/processes.md): a vCPU runs guest
//! code under two sets of page tables, switching TTBR0_EL1 between them, with
//! ASIDs or with a TLB flush instead. Sign first:
//! scripts/sign-hv.sh target/release/examples/switch_cost

use std::time::Instant;

use macwl_runtime::{
    hv::{self, reg, sys_reg, UserContext, Vcpu, Vm},
    paging::{self, prot, PageTable, PhysMem, PAGE_SIZE},
};

fn main() {
    let vm = Vm::new().expect("VM (signed with the hypervisor entitlement?)");
    let mut vcpu = Vcpu::new(&vm).unwrap();
    let mut other = UserContext::save(&vcpu).unwrap();
    other.x[0] = 1;
    const N: u32 = 20_000;
    let start = Instant::now();
    for _ in 0..N {
        let mine = UserContext::save(&vcpu).unwrap();
        other.restore(&mut vcpu).unwrap();
        other = mine;
    }
    let per = start.elapsed() / N;
    println!("save + restore: {:.2} µs", per.as_secs_f64() * 1e6);
    let start = Instant::now();
    let save = UserContext::save(&vcpu).unwrap();
    for _ in 0..N {
        std::hint::black_box(UserContext::save(&vcpu).unwrap());
    }
    println!("save alone: {:.2} µs", (start.elapsed() / N).as_secs_f64() * 1e6);
    let start = Instant::now();
    for _ in 0..N {
        save.restore(&mut vcpu).unwrap();
    }
    println!("restore alone: {:.2} µs", (start.elapsed() / N).as_secs_f64() * 1e6);
    drop(vcpu);
    let start = Instant::now();
    for _ in 0..200 {
        drop(Vcpu::new(&vm).unwrap());
    }
    println!("vCPU create + destroy: {:.1} µs", (start.elapsed() / 200).as_secs_f64() * 1e6);
    // A hand-off between host threads (a condvar), which a blocking call
    // moved to a worker costs twice.
    let pair = std::sync::Arc::new((std::sync::Mutex::new(0u32), std::sync::Condvar::new()));
    let p2 = pair.clone();
    let t = std::thread::spawn(move || {
        let (m, c) = &*p2;
        for i in 0..N {
            let mut g = c.wait_while(m.lock().unwrap(), |v| *v != 2 * i + 1).unwrap();
            *g += 1;
            c.notify_one();
        }
    });
    let (m, c) = &*pair;
    let start = Instant::now();
    for i in 0..N {
        let mut g = m.lock().unwrap();
        *g = 2 * i + 1;
        c.notify_one();
        drop(c.wait_while(g, |v| *v != 2 * i + 2).unwrap());
    }
    t.join().unwrap();
    println!("host thread round trip (condvar): {:.2} µs", (start.elapsed() / N).as_secs_f64() * 1e6);

    processes::run(&vm);
}

/// Two guest processes in one VM, one vCPU switching between them.
mod processes {
    use super::*;

    /// Each process's program and the pages it reads, at the same virtual
    /// addresses in both (in different frames): if a switch left the other
    /// process's translations in use, the program would report the wrong
    /// process.
    const CODE_VA: u64 = 0x10000;
    const DATA_VA: u64 = 0x100000;
    const MAX_PAGES: u64 = 2048;
    const VECTORS_VA: u64 = 0xffff_ffff_fff0_0000;
    /// The vector for a synchronous exception from EL0: `hvc #0; eret`, then
    /// a TLB-flushing return path.
    const EL0_SYNC: u64 = 0x400;
    const FLUSH_PATH: u64 = VECTORS_VA + EL0_SYNC + 8;
    /// Descriptor bit 11: not global (the entry is tagged with the ASID).
    const NG: u64 = 1 << 11;

    /// Loop: x0 = `marker`; read a word from each of x5 pages from DATA_VA
    /// (a TLB entry each); `svc`.
    fn program(marker: u32) -> [u32; 10] {
        [
            0xd280_0000 | (marker << 5),  // 0: movz x0, #marker
            0xd2a0_0202,                  // 1: movz x2, #0x10, lsl #16 (DATA_VA)
            0xaa05_03e3,                  // 2: mov x3, x5
            0xb400_0000 | (5 << 5) | 3,   // 3: cbz x3, 8
            0xf940_0044,                  // 4: ldr x4, [x2]
            0x9140_0442,                  // 5: add x2, x2, #4096
            0xd100_0463,                  // 6: sub x3, x3, #1
            0x1400_0000 | (0x3ff_fffc),   // 7: b 3
            0xd400_0001,                  // 8: svc #0
            0x1400_0000 | (0x3ff_fff7),   // 9: b 0
        ]
    }

    fn write_code(phys: &mut PhysMem, frame: u64, code: &[u32]) {
        for (i, insn) in code.iter().enumerate() {
            unsafe { phys.ptr(frame + i as u64 * 4).cast::<u32>().write(*insn) };
        }
    }

    struct Process {
        root: u64,
        asid: u64,
        ctx: UserContext,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Switch {
        /// The same task resumes (no switch).
        None,
        /// Another thread of the same process: save + restore.
        Thread,
        /// Another process, with its own ASID: save + restore + TTBR0.
        Asid,
        /// Another process, no ASIDs: save + restore + TTBR0 + `tlbi vmalle1`.
        Flush,
    }

    fn blank() -> UserContext {
        UserContext { x: [0; 31], sp: 0, pc: CODE_VA, pstate: 0, tpidr: 0, tpidrro: 0, fpcr: 0, fpsr: 0, v: [0; 32] }
    }

    fn configure(vcpu: &mut Vcpu, kernel_root: u64) {
        use sys_reg::*;
        vcpu.set_sys_reg(MAIR_EL1, 0xff).unwrap();
        let tcr = 16 | (0b01 << 8) | (0b01 << 10) | (0b11 << 12) // T0SZ 48 bits, WB, inner shareable, 4 KiB
            | (16 << 16) | (0b01 << 24) | (0b01 << 26) | (0b11 << 28) | (0b10 << 30) // TTBR1 likewise
            | (0b001 << 32) // IPS: 36 bits
            | (1 << 36)     // AS: 16-bit ASIDs (A1 = 0: the ASID is TTBR0's)
            | (1 << 37); // TBI0
        vcpu.set_sys_reg(TCR_EL1, tcr).unwrap();
        vcpu.set_sys_reg(TTBR1_EL1, kernel_root).unwrap();
        vcpu.set_sys_reg(CPACR_EL1, 0b11 << 20).unwrap();
        vcpu.set_sys_reg(VBAR_EL1, VECTORS_VA).unwrap();
        vcpu.set_sys_reg(SCTLR_EL1, 0x30d0_0800 | 1 | (1 << 2) | (1 << 12)).unwrap();
    }

    fn ttbr0(p: &Process, switch: Switch) -> u64 {
        // Without ASIDs every process is ASID 0.
        p.root | if switch == Switch::Flush { 0 } else { p.asid << 48 }
    }

    /// Run `rounds` system calls, switching as `switch` says after each;
    /// each run reads `pages` pages. Returns µs per round.
    fn rounds(vcpu: &mut Vcpu, procs: &mut [Process; 2], switch: Switch, pages: u64, rounds: u32) -> f64 {
        for p in procs.iter_mut() {
            p.ctx = blank();
            p.ctx.x[5] = pages;
        }
        let mut cur = 0;
        // A thread switch stays in process 0 (both tasks run its program).
        let task_process = |task: usize| if switch == Switch::Thread { 0 } else { task };
        vcpu.set_sys_reg(sys_reg::TTBR0_EL1, ttbr0(&procs[0], switch)).unwrap();
        procs[0].ctx.restore(vcpu).unwrap();
        // Start through the flush path: nothing stale from earlier runs.
        vcpu.set_reg(reg::CPSR, 0x3c5).unwrap();
        vcpu.set_reg(reg::PC, FLUSH_PATH).unwrap();
        let mut saved: [Option<UserContext>; 2] = [None, None];
        let start = Instant::now();
        for _ in 0..rounds {
            let exit = vcpu.run().unwrap();
            if exit.reason != hv::EXIT_REASON_EXCEPTION || exit.exception.syndrome >> 26 != 0x16 {
                panic!("unexpected exit {} {:#x}", exit.reason, exit.exception.syndrome);
            }
            let want = task_process(cur) as u64 + 1;
            let got = vcpu.reg(reg::X0).unwrap();
            assert_eq!(got, want, "ran the other process's code (switch {}, {pages} pages)", switch as u8);
            if switch == Switch::None {
                continue; // resumes at the vector's eret
            }
            saved[cur] = Some(UserContext::save(vcpu).unwrap());
            cur ^= 1;
            let next = saved[cur].take().unwrap_or_else(|| procs[task_process(cur)].ctx.clone());
            if switch != Switch::Thread {
                vcpu.set_sys_reg(sys_reg::TTBR0_EL1, ttbr0(&procs[cur], switch)).unwrap();
            }
            next.restore(vcpu).unwrap();
            if switch == Switch::Flush {
                vcpu.set_reg(reg::CPSR, 0x3c5).unwrap();
                vcpu.set_reg(reg::PC, FLUSH_PATH).unwrap();
            }
        }
        start.elapsed().as_secs_f64() * 1e6 / rounds as f64
    }

    pub fn run(vm: &Vm) {
        let mut phys = PhysMem::new().unwrap();
        let kernel = PageTable::new(&mut phys).unwrap();
        let vectors = phys.alloc().unwrap();
        // hvc #0 at each vector; at EL0 sync, eret and a flush path too.
        for i in 0..16u64 {
            write_code(&mut phys, vectors + i * 0x80, &[0xd400_0002, 0xd69f_03e0]);
        }
        let flush = [0xd400_0002, 0xd69f_03e0, 0xd503_3a9f, 0xd508_871f, 0xd503_3b9f, 0xd503_3fdf, 0xd69f_03e0];
        write_code(&mut phys, vectors + EL0_SYNC, &flush); // hvc; eret; dsb ishst; tlbi vmalle1; dsb ish; isb; eret
        kernel.map(&mut phys, VECTORS_VA, vectors, paging::kernel_code_attrs()).unwrap();
        let mut make = |marker: u32, asid: u64| {
            let user = PageTable::new(&mut phys).unwrap();
            let code = phys.alloc().unwrap();
            write_code(&mut phys, code, &program(marker));
            user.map(&mut phys, CODE_VA, code, paging::user_attrs(prot::READ | prot::EXEC) | NG).unwrap();
            for i in 0..MAX_PAGES {
                let f = phys.alloc().unwrap();
                user.map(&mut phys, DATA_VA + i * PAGE_SIZE, f, paging::user_attrs(prot::READ | prot::WRITE) | NG).unwrap();
            }
            Process { root: user.root, asid, ctx: blank() }
        };
        let mut procs = [make(1, 1), make(2, 2)];
        phys.map_into(vm).unwrap();
        let mut vcpu = Vcpu::new(vm).unwrap();
        configure(&mut vcpu, kernel.root);

        const R: u32 = 20_000;
        let t0 = Instant::now();
        for i in 0..R {
            vcpu.set_sys_reg(sys_reg::TTBR0_EL1, procs[i as usize & 1].root | ((i as u64 & 1) + 1) << 48).unwrap();
        }
        println!("TTBR0_EL1 write alone: {:.3} µs", t0.elapsed().as_secs_f64() * 1e6 / R as f64);
        println!("one vCPU, a system call per round; µs per round (switch cost = round minus 'none'):");
        println!("{:>6} {:>8} {:>8} {:>8} {:>8}", "pages", "none", "thread", "asid", "flush");
        for pages in [0, 16, 256, 2048] {
            // Warm up, then measure each twice and keep the faster.
            let mut row = Vec::new();
            for switch in [Switch::None, Switch::Thread, Switch::Asid, Switch::Flush] {
                let r = if pages >= 2048 { R / 4 } else { R };
                rounds(&mut vcpu, &mut procs, switch, pages, 2000);
                let t = rounds(&mut vcpu, &mut procs, switch, pages, r).min(rounds(&mut vcpu, &mut procs, switch, pages, r));
                row.push(t);
            }
            println!("{:>6} {:>8.2} {:>8.2} {:>8.2} {:>8.2}", pages, row[0], row[1], row[2], row[3]);
        }
        drop(vcpu);
        hv::unmap(0, paging::PHYS_SIZE as usize).unwrap();
    }
}
