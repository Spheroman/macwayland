//! End-to-end: hand-assembled static arm64 ELFs run under the runtime.
//!
//! Hypervisor.framework allows one VM per process, so tests take `VM_LOCK`.
//! They need the hypervisor entitlement; `cargo test` signs via the runner in
//! .cargo/config.toml.

use std::{io::Read, os::{fd::OwnedFd, unix::ffi::OsStrExt}, sync::Mutex};

use linux_abi::{fs::FdEntry, Process};
use macwl_runtime::Exit;

static VM_LOCK: Mutex<()> = Mutex::new(());

const BASE: u64 = 0x40_0000;
const CODE_OFFSET: u64 = 64 + 56; // after the ELF header and one program header

// A few arm64 instructions.
fn movz(rd: u32, imm: u16, hw: u32) -> u32 {
    0xd280_0000 | (hw << 21) | ((imm as u32) << 5) | rd
}
fn movk(rd: u32, imm: u16, hw: u32) -> u32 {
    0xf280_0000 | (hw << 21) | ((imm as u32) << 5) | rd
}
/// `mov xd, #imm` for a 32-bit immediate.
fn mov(rd: u32, imm: u64) -> [u32; 2] {
    [movz(rd, imm as u16, 0), movk(rd, (imm >> 16) as u16, 1)]
}
const SVC: u32 = 0xd400_0001;
/// `ldr x0, [x1]`
const LDR_X0_X1: u32 = 0xf940_0020;

/// A static ELF with one RWX segment: `code`, then `data` at the address
/// `data_addr(code_len)` returns.
fn elf(code: &[u32], data: &[u8]) -> Vec<u8> {
    let size = CODE_OFFSET + code.len() as u64 * 4 + data.len() as u64;
    let mut f = Vec::new();
    f.extend(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    f.extend(2u16.to_le_bytes()); // ET_EXEC
    f.extend(183u16.to_le_bytes()); // EM_AARCH64
    f.extend(1u32.to_le_bytes());
    f.extend((BASE + CODE_OFFSET).to_le_bytes()); // e_entry
    f.extend(64u64.to_le_bytes()); // e_phoff
    f.extend(0u64.to_le_bytes()); // e_shoff
    f.extend(0u32.to_le_bytes()); // e_flags
    f.extend(64u16.to_le_bytes()); // e_ehsize
    f.extend(56u16.to_le_bytes()); // e_phentsize
    f.extend(1u16.to_le_bytes()); // e_phnum
    f.extend([0u8; 6]); // no section headers
    f.extend(1u32.to_le_bytes()); // PT_LOAD
    f.extend(7u32.to_le_bytes()); // R | W | X
    f.extend(0u64.to_le_bytes()); // p_offset
    f.extend(BASE.to_le_bytes()); // p_vaddr
    f.extend(BASE.to_le_bytes()); // p_paddr
    f.extend(size.to_le_bytes()); // p_filesz
    f.extend(size.to_le_bytes()); // p_memsz
    f.extend(0x1000u64.to_le_bytes()); // p_align
    assert_eq!(f.len() as u64, CODE_OFFSET);
    f.extend(code.iter().flat_map(|i| i.to_le_bytes()));
    f.extend(data);
    f
}

fn data_addr(code_len: usize) -> u64 {
    BASE + CODE_OFFSET + code_len as u64 * 4
}

/// Run `elf` with guest stdout captured; return the exit and the output.
fn run(elf: &[u8]) -> (Exit, Vec<u8>) {
    run_args(elf, &[b"test"])
}

fn run_args(elf: &[u8], argv: &[&[u8]]) -> (Exit, Vec<u8>) {
    run_with(elf, argv, Process::new("/"))
}

/// Run `elf` in `process`, with guest stdout captured.
fn run_with(elf: &[u8], argv: &[&[u8]], process: Process) -> (Exit, Vec<u8>) {
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut reader, writer) = std::io::pipe().unwrap();
    process.fds.insert_at(1, FdEntry::new(OwnedFd::from(writer)));
    // The process (and the write end) goes when the run ends, so the read
    // below ends.
    let exit = macwl_runtime::run(elf, argv, &[], process).unwrap();
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    (exit, out)
}

#[test]
fn hello_world() {
    let msg = b"hello from Linux\n";
    let mut code = Vec::new();
    code.extend(mov(0, 1)); // fd
    code.extend(mov(1, 0)); // buf, patched below
    code.extend(mov(2, msg.len() as u64));
    code.extend(mov(8, 64)); // write
    code.push(SVC);
    code.extend(mov(0, 42));
    code.extend(mov(8, 94)); // exit_group
    code.push(SVC);
    let [lo, hi] = mov(1, data_addr(code.len()));
    code[2] = lo;
    code[3] = hi;

    let (exit, out) = run(&elf(&code, msg));
    assert_eq!(out, msg);
    assert_eq!(exit, Exit::Code(42));
}

#[test]
fn unknown_syscall_is_enosys() {
    let mut code = Vec::new();
    code.extend(mov(8, 999));
    code.push(SVC);
    code.extend(mov(8, 94)); // exit_group(x0), x0 = -ENOSYS
    code.push(SVC);
    let (exit, _) = run(&elf(&code, &[]));
    assert_eq!(exit, Exit::Code(-38 & 0xff));
}

#[test]
fn null_load_faults() {
    let mut code = Vec::new();
    code.extend(mov(1, 0));
    code.push(LDR_X0_X1);
    let (exit, _) = run(&elf(&code, &[]));
    let Exit::Fault { sig: 11, esr, far, pc } = exit else { panic!("expected a fault, got {exit:?}") };
    assert_eq!(esr >> 26, 0x24, "data abort from EL0"); // EC_DABT_LOWER
    assert_eq!(far, 0);
    assert_eq!(pc, BASE + CODE_OFFSET + 8);
}

/// SIMD registers round-trip through the vCPU (the setter is called through
/// inline assembly, since it takes its value in a vector register).
#[test]
fn simd_registers_round_trip() {
    use macwl_runtime::hv::{Vcpu, Vm};
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vm = Vm::new().unwrap();
    let mut vcpu = Vcpu::new(&vm).unwrap();
    for q in 0..32u32 {
        let v = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128.rotate_left(q * 4) ^ q as u128;
        vcpu.set_simd_reg(q, v).unwrap();
    }
    for q in 0..32u32 {
        let v = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128.rotate_left(q * 4) ^ q as u128;
        assert_eq!(vcpu.simd_reg(q).unwrap(), v, "q{q}");
    }
}

/// A guest program built by scripts/build-guest.sh.
fn guest(name: &str) -> Vec<u8> {
    let path = format!("{}/../../target/guest/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e} (run scripts/build-guest.sh first)"))
}

/// A real musl program: libc startup, stdio, exit status from `main`.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn musl_hello() {
    let (exit, out) = run(&guest("hello"));
    assert_eq!(String::from_utf8_lossy(&out), "hello from musl, argc=1 argv[0]=test\n");
    assert_eq!(exit, Exit::Code(7));
}

/// Dynamically linked, the way Nix builds programs: the dynamic linker is
/// named by its /nix/store path, which exists on the Mac too.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn dynamic_musl_hello_from_nix() {
    let (exit, out) = run(&guest("hello-dyn"));
    assert_eq!(String::from_utf8_lossy(&out), "hello from musl, argc=1 argv[0]=test\n");
    assert_eq!(exit, Exit::Code(7));
}

/// Dynamically linked the way a distribution builds programs: the dynamic
/// linker is /lib/ld-musl-aarch64.so.1, found in a guest root that isn't the
/// Mac's.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn dynamic_musl_hello_in_a_guest_root() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("lib")).unwrap();
    std::fs::write(root.path().join("lib/ld-musl-aarch64.so.1"), guest("ld-musl-aarch64.so.1")).unwrap();
    let (exit, out) = run_with(&guest("hello-distro"), &[b"test"], Process::new(root.path()));
    assert_eq!(String::from_utf8_lossy(&out), "hello from musl, argc=1 argv[0]=test\n");
    assert_eq!(exit, Exit::Code(7));

    // Without the loader in the root, loading fails cleanly.
    std::fs::remove_file(root.path().join("lib/ld-musl-aarch64.so.1")).unwrap();
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let err = macwl_runtime::run(&guest("hello-distro"), &[b"test"], &[], Process::new(root.path())).unwrap_err();
    assert!(err.to_string().contains("/lib/ld-musl-aarch64.so.1"), "{err}");
}

#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn musl_memory() {
    let (exit, out) = run(&guest("mm"));
    assert_eq!(String::from_utf8_lossy(&out), "mm ok\n");
    assert_eq!(exit, Exit::Code(0));
}

/// Accesses that mprotect/munmap revoked must fault even though the page was
/// just in the TLB.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn revoked_access_faults() {
    for mode in ["mprotect", "munmap"] {
        let (exit, out) = run_args(&guest("mm"), &[b"mm", mode.as_bytes()]);
        assert!(matches!(exit, Exit::Fault { .. }), "{mode}: {exit:?}, output {:?}", String::from_utf8_lossy(&out));
    }
}

/// An 8 KiB host file (a page of 'h', a page of '.') handed to the guest as
/// fd 3, the way a Wayland compositor and client share a buffer.
fn shm_process() -> (Process, tempfile::NamedTempFile) {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&[b'h'; 4096]).unwrap();
    f.write_all(&[b'.'; 4096]).unwrap();
    let process = Process::new("/");
    process.fds.insert_at(3, FdEntry::new(OwnedFd::from(f.reopen().unwrap())));
    (process, f)
}

#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn shared_file_mapping() {
    let (process, file) = shm_process();
    let (exit, out) = run_with(&guest("shm"), &[b"shm"], process);
    assert_eq!(String::from_utf8_lossy(&out), "shm ok\n");
    assert_eq!(exit, Exit::Code(0));
    let data = std::fs::read(file.path()).unwrap();
    assert!(data[..4096].iter().all(|&b| b == b'h'));
    assert!(data[4096..].iter().all(|&b| b == b'g'), "the guest's writes reached the host file");
}

/// A host program's POSIX shm object (say, a compositor's keymap) mapped
/// privately by the guest: macOS can't read such objects, only map them.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn private_mapping_of_a_host_shm_object() {
    use std::os::fd::FromRawFd;
    let name = std::ffi::CString::new(format!("/macwl-test-{}", std::process::id())).unwrap();
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
    assert!(fd >= 0);
    unsafe { libc::shm_unlink(name.as_ptr()) };
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 16384) }, 0);
    let p = unsafe { libc::mmap(std::ptr::null_mut(), 16384, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    unsafe { std::ptr::write_bytes(p.cast::<u8>(), b'k', 16384) };
    let process = Process::new("/");
    process.fds.insert_at(3, FdEntry::new(fd));
    let (exit, out) = run_with(&guest("shm"), &[b"shm", b"private"], process);
    assert_eq!(String::from_utf8_lossy(&out), "private ok\n");
    assert_eq!(exit, Exit::Code(0));
}

#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn shared_mapping_past_eof_is_a_bus_error() {
    let (process, _file) = shm_process();
    let (exit, out) = run_with(&guest("shm"), &[b"shm", b"eof"], process);
    assert!(matches!(exit, Exit::BusError { .. }), "{exit:?}, output {:?}", String::from_utf8_lossy(&out));
}

#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn guest_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap(); // no /var -> /private/var link
    let process = Process::new("/");
    process.set_exe(b"/bin/fs".to_vec());
    let (exit, out) = run_with(&guest("fs"), &[b"fs", dir.as_os_str().as_encoded_bytes()], process);
    assert_eq!(String::from_utf8_lossy(&out), "fs ok\n");
    assert_eq!(exit, Exit::Code(0));
}

/// The Wayland data path under the hypervisor: the guest sends a memfd over a
/// host Unix socket; the host maps it, sees the guest's frame, and writes into
/// the same memory, which the guest then sees.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn guest_shares_a_buffer_with_a_host_compositor() {
    use std::{io::Write, os::fd::AsRawFd};
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap().join("wayland-0");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let compositor = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut data = [0u8; 16];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        let mut control = [0u64; 8];
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = 64;
        let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut hdr, 0) };
        assert_eq!(&data[..n as usize], b"pool");
        let c = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
        assert!(!c.is_null(), "no fd received");
        let fd = unsafe { *libc::CMSG_DATA(c).cast::<i32>() };
        let px = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        assert_ne!(px, libc::MAP_FAILED);
        let px = unsafe { std::slice::from_raw_parts_mut(px.cast::<u8>(), 4096) };
        assert_eq!(&px[..8], b"frame 1\0", "the guest's frame");
        px[100..113].copy_from_slice(b"seen by host\0");
        conn.write_all(b"ok").unwrap();
    });
    let (exit, out) = run_args(&guest("net"), &[b"net", path.as_os_str().as_encoded_bytes()]);
    compositor.join().unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "net ok\n");
    assert_eq!(exit, Exit::Code(0));
}

/// B2: the unmodified Rust Wayland client, built for Linux, opens a window on
/// macwl-compositor (running here on a thread) and its frames arrive intact.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn b2_linux_wayland_client_draws_on_the_mac_compositor() {
    use std::{sync::mpsc, time::Duration};
    let elf = guest("hello-client");
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = std::fs::canonicalize(dir.path()).unwrap();
    // Only this test touches XDG_RUNTIME_DIR, which the compositor reads.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir) };
    let (tx, frames) = mpsc::channel();
    std::thread::spawn(move || {
        // Runs until the test process exits.
        let _ = macwl_compositor::run(Box::new(move |pixels, data, _damage| {
            let at = |x: usize, y: usize| {
                let p = data.offset as usize + y * data.stride as usize + x * 4;
                u32::from_le_bytes(pixels[p..p + 4].try_into().unwrap())
            };
            let w = data.width as usize - 1;
            let h = data.height as usize - 1;
            tx.send((data.width, data.height, [at(0, 0), at(w, 0), at(0, h), at(w, h)])).ok();
        }));
    });
    let socket = runtime_dir.join("wayland-mac");
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let env = [
        format!("XDG_RUNTIME_DIR={}", runtime_dir.display()).into_bytes(),
        b"WAYLAND_DISPLAY=wayland-mac".to_vec(),
    ];
    let envp: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();
    let _vm = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut reader, writer) = std::io::pipe().unwrap();
    let process = Process::new("/");
    process.fds.insert_at(1, FdEntry::new(OwnedFd::from(writer)));
    let exit = macwl_runtime::run(&elf, &[b"hello-client", b"3"], &envp, process).unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    assert_eq!(out, "configured, drawing first frame\nrendered 3 frames, exiting\n");
    assert_eq!(exit, Exit::Code(0));

    let (w, h, corners) = frames.recv_timeout(Duration::from_secs(5)).expect("no frame reached the compositor");
    assert_eq!((w, h), (640, 480));
    // The client's first frame: blue, red, cyan, yellow corners (XRGB8888).
    assert_eq!(corners, [0xff0000ff, 0xfffe0001, 0xff00feff, 0xfffefe01]); // 254 = 639*255/640, 479*255/480
    assert_eq!(frames.try_iter().count(), 2, "three frames in all");
}

/// The `macwl-run` binary, signed: forked guests re-launch it, so process
/// tests run it as a subprocess rather than in this test binary.
fn macwl_run() -> std::process::Command {
    static SIGNED: std::sync::Once = std::sync::Once::new();
    let bin = env!("CARGO_BIN_EXE_macwl-run");
    SIGNED.call_once(|| {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/sign-hv.sh");
        let out = std::process::Command::new(script).arg(bin).output().unwrap();
        assert!(out.status.success(), "signing failed: {}", String::from_utf8_lossy(&out.stderr));
    });
    std::process::Command::new(bin)
}

/// fork, copy-on-write both ways, MAP_SHARED and file offsets shared with
/// the child, wait statuses (exit codes and translated signals), execve,
/// posix_spawn (vfork), and a fork from a forked child: see guest/fork.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn processes() {
    let path = format!("{}/../../target/guest/fork", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).arg(&path).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "exec'd with 6\nfork ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Signal handlers: frames and rt_sigreturn, masks and pending signals, the
/// alternate stack, SIGSEGV handled, a timer interrupting a busy loop (a host
/// signal kicking the vCPU out), SA_RESTART, signals from another process,
/// SIGPIPE: see guest/signals.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn signals() {
    let path = format!("{}/../../target/guest/signals", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "signals ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// More threads than CPUs (150 at once, past the 64 vCPUs a process can
/// have), preemption sharing a CPU among spinning threads, a signal to one
/// thread: see guest/manythreads.c. Run with the default CPUs and with two.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn manythreads() {
    let path = format!("{}/../../target/guest/manythreads", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    for cpus in [None, Some("2")] {
        let mut cmd = macwl_run();
        if let Some(n) = cpus {
            cmd.env("MACWL_CPUS", n);
        }
        let out = cmd.arg(&path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "manythreads ok\n", "cpus {cpus:?}, stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success());
    }
}

/// signalfd (signals the process sends itself, and host signals from
/// another process, polled and blocking) and close_range: see
/// guest/signalfd.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn signalfd() {
    let path = format!("{}/../../target/guest/signalfd", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "signalfd ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Sockets passed while the socket carrying them is in flight, and the end
/// of the file once they arrive: see guest/passfd.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn passfd() {
    let path = format!("{}/../../target/guest/passfd", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "passfd ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// A private file mapping follows the file's later changes in the pages it
/// hasn't written, as Wine's session object needs: see guest/privmap.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn private_file_mapping_sees_changes() {
    let path = format!("{}/../../target/guest/privmap", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "privmap ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Threads: mutexes and condition variables (futex), joins (CLEARTID), TLS,
/// thread ids, a signal to one thread blocked in read(), 48 threads at once,
/// fork and exec from a threaded process, exit() from a thread: see
/// guest/threads.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn threads() {
    let path = format!("{}/../../target/guest/threads", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "threads ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Live (not zombie) processes running the executable at `path`.
fn live_processes_of(path: &std::path::Path) -> Vec<i32> {
    let mut pids = vec![0i32; 1 << 16];
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    pids.truncate(n.max(0) as usize);
    pids.retain(|&pid| {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) } != size {
            return false;
        }
        const SZOMB: u32 = 5;
        let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        info.pbi_status != SZOMB && len > 0 && std::path::Path::new(std::ffi::OsStr::from_bytes(&buf[..len as usize])) == path
    });
    pids
}

/// Every Mac process (and so its VM) goes as soon as its Linux process has
/// ended: hundreds of children that exit, exit_group from a thread while
/// others are blocked, are killed (with and without a spare and a child of
/// their own), exec, daemonize (fork, setsid, fork) or outlive their parent,
/// none of them waited for: see guest/procstress.c. Runs a copy of
/// `macwl-run`, so that its processes, spares included, can be told from
/// other tests'.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn processes_go_when_they_end() {
    use std::io::{BufRead, BufReader};
    let path = format!("{}/../../target/guest/procstress", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let bin = macwl_run().get_program().to_owned();
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("macwl-run");
    std::fs::copy(bin, &copy).unwrap();
    let copy = std::fs::canonicalize(copy).unwrap();
    let mut child = std::process::Command::new(&copy)
        .arg(&path)
        .arg(&path)
        .arg("300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let main = child.id() as i32;
    let mut announced = Vec::new();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    for line in lines.by_ref() {
        let line = line.unwrap();
        if line == "check" {
            break;
        }
        announced.push(line.strip_prefix("pid ").and_then(|p| p.parse::<i32>().ok()).unwrap_or_else(|| panic!("unexpected output {line:?}")));
    }
    assert_eq!(announced.len(), 300 + 300 / 10 * 4, "every child and grandchild announced itself");
    // Daemons may still be finishing; give them a moment. What's left is
    // the parent, with its zombies, and its spare.
    let wait_until = |what: &str, ok: &dyn Fn(&[i32]) -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let live = live_processes_of(&copy);
            if ok(&live) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}: still live: {live:?}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    // (With MACWL_ONE_VM=1 the processes are the host's: it's the one Mac
    // process, with no spare.)
    let mac_processes = if macwl_runtime::one_vm() { 1 } else { 2 };
    wait_until("ended children", &|live| live.len() == mac_processes && live.contains(&main) && !announced.iter().any(|p| live.contains(p)));
    drop(child.stdin.take());
    assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("procstress ok"));
    assert!(child.wait().unwrap().success());
    wait_until("after the parent", &|live| live.is_empty());
}

/// `guest/procs` TEST under one VM (`MACWL_ONE_VM=1`, whatever the tests run
/// with): its stdout, after checking it ended well.
fn procs_in_one_vm(test: &str) -> String {
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().env("MACWL_ONE_VM", "1").arg(&path).arg(test).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success() && stdout.ends_with(&format!("procs {test} ok\n")), "{test}: {stdout}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    stdout
}

/// Copy-on-write fork in one VM: each side's writes its own, after a second
/// fork while the first child still shares frames, and while threads write
/// as the process forks; MAP_SHARED and a vfork child's memory shared.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_copy_on_write() {
    procs_in_one_vm("cow");
}

/// Zombies and `wait` in one VM: a zombie until waited for, WNOHANG,
/// WNOWAIT, WUNTRACED and WCONTINUED, SIGCHLD's si_code for stops and
/// exits, SIGCHLD ignored reaping at once, a stopped child killed, an orphan
/// reparented.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_zombies_and_wait() {
    procs_in_one_vm("wait");
}

/// Signals, process groups and sessions across the processes of one host:
/// kill to a group and to 0, setpgid from both sides, setsid's rules,
/// EACCES on an exec'd child, the sender's pid in siginfo, tgkill of
/// another process's thread.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_signals_and_groups() {
    procs_in_one_vm("groups");
}

/// A process's timers in one VM (setitimer, timer_create, alarm) signal it
/// and no other process: another process of the host, meanwhile sleeping,
/// polling and waiting for a signal in short steps, sees none of them end
/// early.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_timers_are_their_process_alone() {
    procs_in_one_vm("timers");
}

/// The rest of the process model in one VM: CLONE_VM without vfork, a
/// child subreaper (orphans and an orphaned zombie adopted), pidfds (poll
/// for the end, pidfd_send_signal, pidfd_getfd, waitid(P_PIDFD)), CPU time
/// per process with user and system apart (a sleeping child's CPU clock
/// stays near zero while its parent spins; a spinning thread counts as it
/// goes; system calls are system time; times()), capget, PR_CAPBSET_READ,
/// PR_CAP_AMBIENT, get_mempolicy, SO_REUSEPORT.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_process_model() {
    procs_in_one_vm("model");
}

/// `/proc` in one VM, from the process table and each process: the
/// listing, another process's stat, status, statm, cmdline (read from its
/// memory), environ, comm, maps, fd/ and its links, exe, cwd, task/; a
/// zombie's state; /proc/self; /proc/stat.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_proc_of_every_process() {
    procs_in_one_vm("procfs");
}

/// Containment in one VM: a runtime error (a panic, here from a call debug
/// builds of the runtime answer with one) in one process's system call ends
/// that process with SIGSYS, whether its main thread or another made the
/// call; its parent, a sibling and later forks carry on in the same host.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_contains_a_runtime_error() {
    procs_in_one_vm("contain");
}

/// More live Linux processes than a Mac has VMs (128): 300 at once in one
/// host, which stays one Mac process meanwhile.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_more_processes_than_vms() {
    use std::io::{BufRead, BufReader};
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    // A copy of macwl-run, so its processes can be told from other tests'.
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("macwl-run");
    std::fs::copy(macwl_run().get_program(), &copy).unwrap();
    let copy = std::fs::canonicalize(copy).unwrap();
    let mut child = std::process::Command::new(&copy)
        .env("MACWL_ONE_VM", "1")
        .args([path.as_os_str(), "many".as_ref(), "300".as_ref(), "hold".as_ref()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("300 alive"));
    assert_eq!(live_processes_of(&copy), vec![child.id() as i32], "one Mac process for 301 Linux ones");
    drop(child.stdin.take());
    assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("procs many ok"));
    assert!(child.wait().unwrap().success());
}

/// The host process never execs: in one VM an execve of a Mac program
/// fails with ENOEXEC (step 6 brings them back as processes of their own),
/// even with Mac programs turned on.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_refuses_mac_programs() {
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().env("MACWL_ONE_VM", "1").env("MACWL_MAC_PROGRAMS", "1").arg(&path).arg("execmac").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "execve: ENOEXEC\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}
