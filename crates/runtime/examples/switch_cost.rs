//! What moving a guest thread between vCPUs costs: saving one thread's
//! registers from a vCPU and restoring another's (M:N threading's context
//! switch), and creating a vCPU (what reusing one saves). Sign first:
//! scripts/sign-hv.sh target/release/examples/switch_cost

use std::time::Instant;

use macwl_runtime::hv::{UserContext, Vcpu, Vm};

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
}
