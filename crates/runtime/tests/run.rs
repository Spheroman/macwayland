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
/// signal kicking the vCPU out), SA_RESTART, calls that don't wait never
/// failing with EINTR (or skipped) while another thread signals all along,
/// signals from another process, SIGPIPE: see guest/signals.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn signals() {
    let path = format!("{}/../../target/guest/signals", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "signals ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// tgkill to a thread of another process: never ESRCH while the thread
/// lives, and the signal arrives (see guest/tgkill.c). When each process
/// was a Mac process of its own it was ESRCH, which wineserver took to mean
/// a client's thread had died; it ended the thread on its side while it
/// still ran, and its heap was soon corrupt.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn tgkill_reaches_another_process_thread() {
    let path = format!("{}/../../target/guest/tgkill", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tgkill ok\n", "stderr: {stderr}");
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

/// The host is the one Mac process (and VM) however many Linux processes it
/// runs, and it goes once they have: hundreds of children that exit,
/// exit_group from a thread while others are blocked, are killed (with and
/// without a child of their own), exec, daemonize (fork, setsid, fork) or
/// outlive their parent, none of them waited for: see guest/procstress.c.
/// Runs a copy of `macwl-run`, so that its processes can be told from other
/// tests'.
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
    // the host.
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
    wait_until("ended children", &|live| live == [main] && !announced.iter().any(|p| live.contains(p)));
    drop(child.stdin.take());
    assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("procstress ok"));
    assert!(child.wait().unwrap().success());
    wait_until("after the parent", &|live| live.is_empty());
}

/// `guest/procs` TEST: its stdout, after checking it ended well.
fn procs_in_one_vm(test: &str) -> String {
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().arg(&path).arg(test).output().unwrap();
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

/// With Mac programs off, an execve of one fails with ENOEXEC.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_refuses_mac_programs_when_off() {
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().env_remove("MACWL_MAC_PROGRAMS").arg(&path).arg("execmac").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "execve: ENOEXEC\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Mac programs from Linux bash in one VM (docs/processes.md, section 7;
/// ROADMAP.md item 13's list, scripted in tests/mac_programs.sh): Mac
/// `uname`, `sw_vers`, `false`, `env`; pipes mixing Mac and Linux programs
/// both ways; exit codes and signals through `wait` (TERM, KILL, USR1, in
/// Linux numbers); fds by guest number, redirects, cwd; a Mac binary that
/// can't run failing execve (bash: 126); SIGSTOP, SIGCONT, SIGTSTP and
/// `bg`, SIGINT to a Mac job; the stand-in in `/proc` and procps' `ps`; Mac
/// `git`; `pbcopy`/`pbpaste`.
#[test]
#[ignore = "needs nix"]
fn one_vm_mac_programs() {
    let path = ["coreutils", "gnugrep", "procps"].map(|p| format!("{}/bin", linux_package(p).to_string_lossy())).join(":");
    let bash = std::path::Path::new(&linux_package("bash")).join("bin/bash");
    let dir = tempfile::tempdir().unwrap();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mac_programs.sh");
    let out = macwl_run()
        .env("MACWL_MAC_PROGRAMS", "1")
        .arg(&bash)
        .arg(script)
        .arg(&path)
        .arg(dir.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.ends_with("mac programs ok\n"), "{stdout}, stderr: {}", String::from_utf8_lossy(&out.stderr));
}

/// musl's `posix_spawn` (vfork) of Mac programs in one VM, 20 in a row,
/// output through a pipe, exit statuses; a Mac binary macOS won't run
/// fails the spawn with ENOEXEC (guest/procs.c, spawnmac); and an execve of
/// one replaces the program as far as the guest can tell (execmac: `true`,
/// nothing printed).
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_vm_spawns_mac_programs() {
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("badmac");
    let true_bin = std::fs::read("/usr/bin/true").unwrap();
    std::fs::write(&bad, &true_bin[..20000.min(true_bin.len())]).unwrap();
    std::fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let run = |args: &[&std::ffi::OsStr]| macwl_run().env("MACWL_MAC_PROGRAMS", "1").arg(&path).args(args).output().unwrap();
    let out = run(&["spawnmac".as_ref(), bad.as_os_str()]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "bad mac binary: ENOEXEC\nprocs spawnmac ok\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let out = run(&["execmac".as_ref()]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Mac programs on a terminal in one VM: Linux bash (interactive, its
/// terminal the host's pty relayed from this one) runs Mac programs that
/// have that pty as their controlling terminal (through a tty agent), and
/// job control works on them as on Linux ones: Ctrl-Z stops Mac `sleep`
/// (`jobs` says Stopped), `bg`, `fg` and Ctrl-C; a `/bin/sleep | tr`
/// pipeline of two Mac programs stopped and continued; a background Mac
/// `cat` reading the terminal stopped by the Mac driver (SIGTTIN); Mac
/// `vim` suspended, `fg`, redrawn and saving; Mac `git log` through Mac
/// `less` as its pager, quit with `q`.
#[test]
#[ignore = "needs nix"]
fn one_vm_mac_programs_on_a_terminal() {
    use std::os::fd::AsRawFd;
    let bash = std::path::Path::new(&linux_package("bash")).join("bin/bash");
    let dir = tempfile::tempdir().unwrap();
    let dir_s = dir.path().to_str().unwrap().to_string();
    let type_in = |master: &OwnedFd, s: &str| {
        assert_eq!(unsafe { libc::write(master.as_raw_fd(), s.as_ptr().cast(), s.len()) }, s.len() as isize);
        std::thread::sleep(std::time::Duration::from_millis(150));
    };
    let env: [(&str, &std::ffi::OsStr); 3] =
        [("MACWL_MAC_PROGRAMS", "1".as_ref()), ("MACWL_ENV_PS1", "$ ".as_ref()), ("MACWL_ENV_TERM", "xterm-256color".as_ref())];
    let (mut child, master) = in_a_terminal(&[bash.as_os_str(), "--norc".as_ref(), "-i".as_ref()], &env, 24, 80);
    let mut seen = Vec::new();
    let mut at = 0;
    // A marker the typed line doesn't contain (its echo can't match).
    let mark = |master: &OwnedFd, seen: &mut Vec<u8>, at: &mut usize, what: &str| {
        type_in(master, &format!("echo {what}-$((6*7))\n"));
        *at = read_until(master, seen, *at, &format!("{what}-42"));
    };
    mark(&master, &mut seen, &mut at, "ready");
    type_in(&master, &format!("cd {dir_s}\n"));

    // The terminal is the Mac program's controlling terminal.
    type_in(&master, "/bin/sh -c 'tty; exec 3</dev/tty && echo has-$((1+1))-tty'\n");
    at = read_until(&master, &mut seen, at, "has-2-tty");

    // Ctrl-Z, jobs, bg, fg, Ctrl-C.
    type_in(&master, "/bin/sleep 100\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "\x1a");
    at = read_until(&master, &mut seen, at, "Stopped");
    type_in(&master, "jobs\n");
    at = read_until(&master, &mut seen, at, "Stopped");
    type_in(&master, "bg\n");
    type_in(&master, "jobs\n");
    at = read_until(&master, &mut seen, at, "Running");
    type_in(&master, "fg\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "\x03");
    type_in(&master, "echo status-$?\n");
    at = read_until(&master, &mut seen, at, "status-130");

    // A pipeline of two Mac programs: both stop, both continue.
    type_in(&master, "/bin/sleep 100 | /usr/bin/tr a b\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "\x1a");
    at = read_until(&master, &mut seen, at, "Stopped");
    type_in(&master, "fg\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "\x03");
    mark(&master, &mut seen, &mut at, "pipeline");

    // A background Mac program reading the terminal: stopped (SIGTTIN).
    type_in(&master, "/bin/cat &\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "jobs\n");
    at = read_until(&master, &mut seen, at, "Stopped");
    type_in(&master, "kill -9 %1\n");
    mark(&master, &mut seen, &mut at, "ttin");

    // Mac vim: suspended, continued (it redraws), and saving.
    type_in(&master, "/usr/bin/vim -u NONE -N edited\n");
    std::thread::sleep(std::time::Duration::from_millis(800));
    type_in(&master, "ihello from vim\x1b");
    type_in(&master, "\x1a");
    at = read_until(&master, &mut seen, at, "Stopped");
    type_in(&master, "jobs\n");
    at = read_until(&master, &mut seen, at, "vim");
    type_in(&master, "fg\n");
    at = read_until(&master, &mut seen, at, "hello from vim");
    type_in(&master, ":wq\n");
    mark(&master, &mut seen, &mut at, "vim");
    assert_eq!(std::fs::read_to_string(dir.path().join("edited")).unwrap(), "hello from vim\n");

    // Mac git, with Mac less as its pager on the terminal.
    type_in(&master, "/usr/bin/git init -q repo && /usr/bin/git -C repo -c user.name=t -c user.email=t@t commit -q --allow-empty -m paged-commit\n");
    // LESS without F: it waits for a key (from /dev/tty: its stdin is git's
    // pipe) even with one screen to show.
    type_in(&master, "LESS=R PAGER=/usr/bin/less /usr/bin/git -C repo log\n");
    at = read_until(&master, &mut seen, at, "paged-commit");
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "q");
    mark(&master, &mut seen, &mut at, "git");

    type_in(&master, "exit\n");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let st = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(std::time::Instant::now() < end, "bash didn't exit: {}", String::from_utf8_lossy(&seen));
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(st.success(), "{st:?}: {}", String::from_utf8_lossy(&seen));
}

/// Blocked guest threads cost no host thread each (docs/processes.md, step
/// 3a): 300 threads blocked in `read` and futex waits under a host thread
/// budget of 32, so the host's watchdog would end it at 48 host threads. See
/// guest/blockmany.c. (Never with thousands: linux_abi::host_threads.)
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn blocked_threads_cost_no_host_thread_each() {
    use std::io::Write;
    let path = format!("{}/../../target/guest/blockmany", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let mut child = macwl_run()
        .env("MACWL_MAX_HOST_THREADS", "32")
        .arg(&path)
        .arg("300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // It waits for a byte once they're all blocked; it's there already.
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("blocked 300") && stdout.ends_with("blockmany ok\n"), "stdout: {stdout}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// Calls that need a worker (100 threads waiting in `flock`) and 200 armed
/// timers under a host thread budget of 32: workers are bounded by the
/// budget and calls wait their turn; timers share one host thread. See
/// guest/hostbudget.c.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn locks_and_timers_stay_within_the_host_thread_budget() {
    let path = format!("{}/../../target/guest/hostbudget", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let out = macwl_run().env("MACWL_MAX_HOST_THREADS", "32").arg(&path).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with("hostbudget ok\n"), "stdout: {stdout}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}

/// `guest/tty ARGS`, checked to end well.
fn tty_guest(args: &[&std::ffi::OsStr]) {
    let path = format!("{}/../../target/guest/tty", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let mut cmd = macwl_run();
    // Its waits traced to stderr, which a failure (or two minutes without
    // an end) shows.
    let mut child = cmd
        .env("MACWL_ENV_TTY_TRACE", "1")
        .arg(&path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |mut r: Box<dyn std::io::Read + Send>| std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = r.read_to_end(&mut v);
        String::from_utf8_lossy(&v).into_owned()
    });
    let (out, err) = (drain(Box::new(child.stdout.take().unwrap())), drain(Box::new(child.stderr.take().unwrap())));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if std::time::Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let (stdout, stderr) = (out.join().unwrap(), err.join().unwrap());
    assert!(status.is_some_and(|s| s.success()) && stdout.ends_with("tty ok\n"), "tty {args:?} ({status:?}): {stdout}, stderr: {stderr}");
}

/// An arm64 Linux package from nixpkgs (`MACWL_TEST_<NAME>` overrides): its
/// store path.
fn linux_package(name: &str) -> std::ffi::OsString {
    if let Some(path) = std::env::var_os(format!("MACWL_TEST_{}", name.to_uppercase())) {
        return path;
    }
    let out = std::process::Command::new("nix")
        .args(["build", &format!("nixpkgs#legacyPackages.aarch64-linux.{name}.out"), "--max-jobs", "0", "--no-link", "--print-out-paths"])
        .output()
        .expect("nix (or MACWL_TEST_<NAME> with the package's store path)");
    assert!(out.status.success(), "nix build {name}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().into()
}

/// Terminals and job control on a pty the guest opens (guest/tty.c): a
/// session leader's controlling terminal (TIOCSCTTY, /dev/tty, TIOCGSID), a
/// background read stopped by SIGTTIN and continued in the foreground, EIO
/// with SIGTTIN ignored and in an orphaned group, TOSTOP and SIGTTOU,
/// tcsetattr and tcsetpgrp from the background (SIGTTOU blocked: allowed),
/// Ctrl-C, Ctrl-\ and Ctrl-Z for the foreground job alone, Ctrl-V quoting,
/// SIGWINCH on a resize, TIOCNOTTY, and hangups (the master closing; the
/// session leader ending). The host keeps terminals' sessions and
/// foreground groups itself (docs/processes.md, section 6).
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn terminals_and_job_control() {
    tty_guest(&["rules".as_ref()]);
}

/// Bash's job control on a pty, typed at through the master: `cat &`
/// stopped by SIGTTIN and `fg`, Ctrl-Z, `jobs`, `bg`, Ctrl-C ending the
/// foreground job and not a background one, and a hangup that ends bash
/// (by SIGHUP) and its jobs. nixpkgs' arm64 bash and coreutils.
#[test]
#[ignore = "needs scripts/build-guest.sh and nix"]
fn bash_job_control() {
    let (bash, coreutils) = (linux_package("bash"), linux_package("coreutils"));
    let bash = std::path::Path::new(&bash).join("bin/bash");
    tty_guest(&["bash".as_ref(), bash.as_os_str(), &coreutils]);
}

/// `macwl-run ARGS` in a terminal of its own (a pty, `rows` by
/// `cols`, as a Mac terminal's): the child and the terminal's master.
fn in_a_terminal(args: &[&std::ffi::OsStr], env: &[(&str, &std::ffi::OsStr)], rows: u16, cols: u16) -> (std::process::Child, OwnedFd) {
    use std::os::{fd::FromRawFd, unix::process::CommandExt};
    let (mut master, mut slave) = (0, 0);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) }, 0);
    // The master isn't the child's: only this end closing hangs it up.
    unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) };
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut cmd = macwl_run();
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let stdio = |fd: &OwnedFd| std::process::Stdio::from(fd.try_clone().unwrap());
    cmd.stdin(stdio(&slave)).stdout(stdio(&slave)).stderr(stdio(&slave));
    // A session of its own with the pty as its terminal, as a Mac terminal
    // starts a shell (whose job macwl-run then is).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as u64, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The keyboard's signals as a terminal's shell has them, even
            // if the tests run where they're ignored (a script's `&`): a
            // client passes ignored signals on to its process.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    drop(slave);
    (child, master)
}

/// Read `master` until `want` has come (after `seen`'s first `from` bytes),
/// for up to 20 s: where it ends.
fn read_until(master: &OwnedFd, seen: &mut Vec<u8>, from: usize, want: &str) -> usize {
    use std::os::fd::AsRawFd;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(i) = seen[from..].windows(want.len()).position(|w| w == want.as_bytes()) {
            return from + i + want.len();
        }
        assert!(std::time::Instant::now() < end, "no {want:?} in {:?}", String::from_utf8_lossy(seen));
        let mut p = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 50) } > 0 {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                seen.extend_from_slice(&buf[..n as usize]);
            }
        }
    }
}

/// `macwl-run` in one VM started in a terminal relays it (docs/processes.md,
/// "Its terminal"): its program gets a pty of the host's, with the
/// terminal's size; Ctrl-Z stops the program and, with it, `macwl-run` (as
/// the Mac shell expects of its job), with the terminal's settings given
/// back; continued, the program reads again; Ctrl-C ends a program, which
/// macwl-run reports as 128 + SIGINT; the terminal going hangs it up.
#[test]
#[ignore = "needs nix"]
fn one_vm_relays_its_terminal() {
    use std::os::fd::AsRawFd;
    let coreutils = std::path::PathBuf::from(linux_package("coreutils")).join("bin");
    let tool = |name: &str| coreutils.join(name).into_os_string();
    let type_in = |master: &OwnedFd, s: &str| assert_eq!(unsafe { libc::write(master.as_raw_fd(), s.as_ptr().cast(), s.len()) }, s.len() as isize);
    let wait = |child: &std::process::Child, options: i32| {
        let mut st = 0;
        assert_eq!(unsafe { libc::waitpid(child.id() as i32, &mut st, options) }, child.id() as i32);
        st
    };

    // Its size.
    let (child, master) = in_a_terminal(&[&tool("stty"), "size".as_ref()], &[], 33, 101);
    let mut seen = Vec::new();
    read_until(&master, &mut seen, 0, "33 101");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");

    // Ctrl-Z, and `fg`.
    let (child, master) = in_a_terminal(&[&tool("cat")], &[], 24, 80);
    let mut seen = Vec::new();
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "one\n");
    let at = read_until(&master, &mut seen, 0, "one");
    let at = read_until(&master, &mut seen, at, "one");
    type_in(&master, "\x1a");
    let st = wait(&child, libc::WUNTRACED);
    assert!(libc::WIFSTOPPED(st), "{st:#x}");
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(master.as_raw_fd(), &mut t) };
    assert!(t.c_lflag & libc::ICANON != 0, "the terminal's own settings back while stopped");
    unsafe { libc::kill(child.id() as i32, libc::SIGCONT) };
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "two\n");
    let at = read_until(&master, &mut seen, at, "two");
    read_until(&master, &mut seen, at, "two");
    type_in(&master, "\x04");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");

    // Ctrl-C.
    let (child, master) = in_a_terminal(&[&tool("sleep"), "100".as_ref()], &[], 24, 80);
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "\x03");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 128 + 2, "{st:#x}");

    // The terminal goes: the program gets SIGHUP.
    let (child, master) = in_a_terminal(&[&tool("sleep"), "100".as_ref()], &[], 24, 80);
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(master);
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 128 + 1, "{st:#x}");
}

/// A host serving clients (`macwl-run --serve PATH`, docs/processes.md step
/// 5) at a socket in a directory of its own; on drop, given a few seconds
/// to go by itself, then killed.
struct Served {
    host: std::process::Child,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Served {
    fn start() -> Served {
        Served::start_with(&[])
    }

    /// The same, with `env` in the host's environment.
    fn start_with(env: &[(&str, &str)]) -> Served {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host");
        let mut cmd = macwl_run();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let host = cmd.arg("--serve").arg(&path).stdin(std::process::Stdio::null()).spawn().unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(std::time::Instant::now() < end, "no socket at {}", path.display());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Served { host, path, _dir: dir }
    }

    /// `macwl-run` as a client of this host.
    fn client(&self) -> std::process::Command {
        let mut cmd = macwl_run();
        cmd.env("MACWL_HOST", &self.path);
        cmd
    }

    /// When the host process ends, if it does within `limit`.
    fn ends_within(&mut self, limit: std::time::Duration) -> Option<std::time::Instant> {
        let end = std::time::Instant::now() + limit;
        while std::time::Instant::now() < end {
            if self.host.try_wait().unwrap().is_some() {
                return Some(std::time::Instant::now());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if self.ends_within(std::time::Duration::from_secs(5)).is_none() {
            let _ = self.host.kill();
            let _ = self.host.wait();
        }
    }
}

/// `guest/client`'s path.
fn client_guest() -> std::path::PathBuf {
    let path = format!("{}/../../target/guest/client", env!("CARGO_MANIFEST_DIR"));
    std::fs::canonicalize(path).expect("run scripts/build-guest.sh first")
}

/// A client running `guest/client trap` (stdout piped): the child, its
/// first line ("ready PID PGID SID PPID"), and a reader for the rest.
fn trapping_client(host: &Served) -> (std::process::Child, String, std::io::BufReader<std::process::ChildStdout>) {
    use std::io::BufRead;
    let mut child = host.client().arg(client_guest()).arg("trap").stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.starts_with("ready "), "{line:?}");
    (child, line, out)
}

/// All of `out`, read on a thread of its own.
fn rest_of_in_background(mut out: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        String::from_utf8_lossy(&v).into_owned()
    })
}

/// The rest of `out`, read on a thread of its own, for up to 20 s.
fn rest_of(mut out: impl Read + Send + 'static) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the output to end")
}

/// A client's exit status is its process's: the code, or the signal, which
/// the client raises on itself (one that would dump core is 128 + it); its
/// fds 0–2 are the process's own (a pipe stays a pipe). A host a client
/// starts because none serves the path goes 2 s after it's done.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn client_exit_status() {
    use std::os::unix::process::ExitStatusExt;
    let host = Served::start();
    let run = |args: &[&str]| host.client().arg(client_guest()).args(args).output().unwrap();
    assert_eq!(run(&["exit", "3"]).status.code(), Some(3));
    assert_eq!(run(&["exit", "0"]).status.code(), Some(0));
    let st = run(&["raise", "15"]).status;
    assert_eq!(st.signal(), Some(libc::SIGTERM), "{st:?}");
    let st = run(&["raise", "9"]).status;
    assert_eq!(st.signal(), Some(libc::SIGKILL), "{st:?}");
    // SIGSEGV would dump core: a status instead.
    assert_eq!(run(&["raise", "11"]).status.code(), Some(128 + 11));
    let out = host.client().arg(client_guest()).arg("stdio").stdin(std::process::Stdio::null()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0 chr\n1 fifo\n2 fifo\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // A program that isn't there: the client says so, as `macwl-run` does.
    let out = host.client().arg("/nonexistent/program").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/program"), "{}", String::from_utf8_lossy(&out.stderr));

    // No host at the path: the client starts one, which goes once idle.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("host");
    let out = macwl_run().env("MACWL_HOST", &path).arg(client_guest()).args(["exit", "5"]).output().unwrap();
    assert_eq!(out.status.code(), Some(5), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700, "the user's alone");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while path.exists() {
        assert!(std::time::Instant::now() < end, "the started host stayed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Signals sent to a client from the Mac side (`kill -TERM`, `-INT`,
/// `-HUP`, `-USR1` of its pid) reach its Linux process, whose end is then
/// the client's.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn client_forwards_mac_signals() {
    let host = Served::start();
    for (mac, linux) in [(libc::SIGTERM, 15), (libc::SIGINT, 2), (libc::SIGHUP, 1), (libc::SIGUSR1, 10)] {
        let (mut child, _, out) = trapping_client(&host);
        assert_eq!(unsafe { libc::kill(child.id() as i32, mac) }, 0);
        let st = child.wait().unwrap();
        assert_eq!(st.code(), Some(100 + linux), "signal {mac}: {st:?}");
        assert_eq!(rest_of(out), format!("got {linux}\n"));
    }
}

/// A client killed with SIGKILL hangs its process up: SIGHUP (and SIGCONT)
/// reach it, as a terminal's going does a session's.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn client_killed_hangs_its_process_up() {
    let host = Served::start();
    let (mut child, _, out) = trapping_client(&host);
    child.kill().unwrap();
    child.wait().unwrap();
    // Its stdout is the process's own: what it says still comes.
    assert_eq!(rest_of(out), "got 1\n");
}

/// Two clients in one host see each other's processes in `/proc`: procps'
/// `ps` in one lists the other. And `macwl-run --ps` lists them from the
/// Mac, with the Mac pid of each one's client.
#[test]
#[ignore = "needs scripts/build-guest.sh and nix"]
fn clients_share_a_host() {
    use std::io::BufRead;
    let procps = std::path::PathBuf::from(linux_package("procps")).join("bin/ps");
    let host = Served::start();
    let mut a = host.client().arg(client_guest()).arg("wait").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(a.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let pid: i64 = line.trim().strip_prefix("ready ").unwrap().parse().unwrap();
    let out = host.client().arg(&procps).args(["-e", "-o", "pid=,comm="]).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{listing}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(listing.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == [pid.to_string().as_str(), "client"]), "{pid} in {listing}");
    assert!(listing.lines().any(|l| l.ends_with(" ps")), "ps itself in {listing}");

    let out = macwl_run().args(["--ps", "--host"]).arg(&host.path).output().unwrap();
    let ps = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{ps}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(ps.starts_with("    PID    PPID"), "{ps}");
    let row: Vec<&str> = ps.lines().find(|l| l.split_whitespace().next() == Some(&pid.to_string())).expect("its row").split_whitespace().collect();
    // PID PPID PGID SID TTY STAT TIME RSS CLIENT CMD...
    let me = pid.to_string();
    assert_eq!((row[1], row[2], row[3], row[8]), ("1", me.as_str(), me.as_str(), a.id().to_string().as_str()), "{ps}");
    assert_eq!(row[9..].join(" "), format!("{} wait", client_guest().display()));
    drop(a.stdin.take());
    assert!(a.wait().unwrap().success());
    // No host at a path: --ps says so.
    let out = macwl_run().args(["--ps", "--host", "/nonexistent/host"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

/// A host lives while it has processes or clients: it goes 2 s after its
/// last client's process ends, and stays for a daemon that process left
/// behind until 2 s after that has gone too.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn host_lifetime() {
    let mut host = Served::start();
    // No client yet: it waits for one.
    assert!(host.ends_within(std::time::Duration::from_millis(2500)).is_none(), "a --serve host waits for its first client");
    let st = host.client().arg(client_guest()).args(["exit", "0"]).status().unwrap();
    assert!(st.success());
    let done = std::time::Instant::now();
    let ended = host.ends_within(std::time::Duration::from_secs(6)).expect("the host to go");
    let after = ended - done;
    assert!(after >= std::time::Duration::from_millis(1900) && after < std::time::Duration::from_millis(3500), "{after:?}");
    assert!(!host.path.exists(), "its socket gone with it");

    let mut host = Served::start();
    let out = host.client().arg(client_guest()).args(["daemon", "3"]).output().unwrap();
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).starts_with("daemon "));
    let done = std::time::Instant::now();
    let ended = host.ends_within(std::time::Duration::from_secs(10)).expect("the host to go");
    let after = ended - done;
    assert!(after >= std::time::Duration::from_millis(4800) && after < std::time::Duration::from_millis(6500), "{after:?}");
}

/// A client started in a terminal relays it (docs/processes.md, "Its
/// terminal"): its process gets a pty of the host's with the terminal's
/// size; Ctrl-Z stops the process and the client with it, the terminal's
/// settings given back; continued, the process reads again; Ctrl-C ends a
/// program, and the client with SIGINT; a resize reaches the process; the
/// terminal going hangs it up.
#[test]
#[ignore = "needs nix"]
fn client_relays_its_terminal() {
    use std::os::fd::AsRawFd;
    // (Found first: the host would go while nix looks.)
    let bash = std::path::PathBuf::from(linux_package("bash")).join("bin/bash");
    let coreutils = std::path::PathBuf::from(linux_package("coreutils")).join("bin");
    let tool = |name: &str| coreutils.join(name).into_os_string();
    let type_in = |master: &OwnedFd, s: &str| assert_eq!(unsafe { libc::write(master.as_raw_fd(), s.as_ptr().cast(), s.len()) }, s.len() as isize);
    let wait = |child: &std::process::Child, options: i32| {
        let mut st = 0;
        assert_eq!(unsafe { libc::waitpid(child.id() as i32, &mut st, options) }, child.id() as i32);
        st
    };
    let host = Served::start();
    let env = [("MACWL_HOST", host.path.as_os_str())];

    // Its size.
    let (child, master) = in_a_terminal(&[&tool("stty"), "size".as_ref()], &env, 33, 101);
    let mut seen = Vec::new();
    read_until(&master, &mut seen, 0, "33 101");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");

    // Ctrl-Z, and `fg`.
    let (child, master) = in_a_terminal(&[&tool("cat")], &env, 24, 80);
    let mut seen = Vec::new();
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "one\n");
    let at = read_until(&master, &mut seen, 0, "one");
    let at = read_until(&master, &mut seen, at, "one");
    type_in(&master, "\x1a");
    let st = wait(&child, libc::WUNTRACED);
    assert!(libc::WIFSTOPPED(st), "{st:#x}");
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(master.as_raw_fd(), &mut t) };
    assert!(t.c_lflag & libc::ICANON != 0, "the terminal's own settings back while stopped");
    unsafe { libc::kill(child.id() as i32, libc::SIGCONT) };
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "two\n");
    let at = read_until(&master, &mut seen, at, "two");
    read_until(&master, &mut seen, at, "two");
    type_in(&master, "\x04");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");

    // Ctrl-C: the client ends as its process did, by SIGINT.
    let (child, master) = in_a_terminal(&[&tool("sleep"), "100".as_ref()], &env, 24, 80);
    std::thread::sleep(std::time::Duration::from_millis(500));
    type_in(&master, "\x03");
    let st = wait(&child, 0);
    assert!(libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGINT, "{st:#x}");

    // A resize: the pty's size follows.
    let env_bash = [("MACWL_HOST", host.path.as_os_str()), ("MACWL_ENV_PATH", coreutils.as_os_str())];
    let (child, master) = in_a_terminal(&[bash.as_os_str(), "-c".as_ref(), "read x; stty size".as_ref()], &env_bash, 24, 80);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let ws = libc::winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    unsafe { libc::kill(child.id() as i32, libc::SIGWINCH) };
    std::thread::sleep(std::time::Duration::from_millis(300));
    type_in(&master, "\n");
    let mut seen = Vec::new();
    read_until(&master, &mut seen, 0, "40 120");
    let st = wait(&child, 0);
    assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");

    // The terminal goes: the process gets SIGHUP, and so the client ends.
    let (child, master) = in_a_terminal(&[&tool("sleep"), "100".as_ref()], &env, 24, 80);
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(master);
    let st = wait(&child, 0);
    assert!(libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGHUP, "{st:#x}");
}

/// One `scripts/session.sh` at a time: each builds and signs the release
/// binaries, which another session would be running.
fn one_session() -> std::sync::MutexGuard<'static, ()> {
    static SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SESSION.lock().unwrap_or_else(|e| e.into_inner())
}

/// `scripts/session.sh` in one host (as by default), through the headless
/// compositor: bash in foot, typed at, lists with procps' `ps` the session's
/// processes, dbus-daemon and foot (clients) and bash (foot's child), all
/// in the one host; `macwl-run --ps` lists them from the Mac, with the
/// clients' Mac pids, while the session's Mac processes are the host and
/// its two clients; and all of them go once bash exits.
#[test]
#[ignore = "needs nix and builds the release binaries"]
fn session_shares_one_host() {
    let _one = one_session();
    // A short path: Unix socket paths are at most 103 bytes.
    let dir = std::path::PathBuf::from(format!("/tmp/macwl-test-session-{}", std::process::id()));
    let ps_txt = dir.join("ps.txt");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/session.sh");
    let mut session = std::process::Command::new(script)
        .env("MACWL_SESSION_COMPOSITOR", "headless")
        .env("MACWL_SESSION_DIR", &dir)
        .env("MACWL_TYPE", format!("ps -eo pid=,ppid=,comm= > {}; sleep 3; exit\\n", ps_txt.display()))
        .env_remove("MACWL_HOST")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Its errors, for a failure to show (the compositor's log is long).
    let errors = rest_of_in_background(session.stderr.take().unwrap());
    let end = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while std::fs::metadata(&ps_txt).map_or(true, |m| m.len() == 0) {
        if std::time::Instant::now() > end || session.try_wait().unwrap().is_some() {
            let _ = session.kill();
            panic!("no ps output from the session");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let inside = std::fs::read_to_string(&ps_txt).unwrap();
    let comms: Vec<&str> = inside.lines().filter_map(|l| l.split_whitespace().nth(2)).collect();
    for want in ["dbus-daemon", "foot", "bash", "ps"] {
        assert!(comms.contains(&want), "{want} in {inside}");
    }
    let out = macwl_run().arg("--ps").arg("--host").arg(dir.join("macwl-host")).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    let clients: Vec<i32> = listing.lines().skip(1).filter_map(|l| l.split_whitespace().nth(8)?.parse().ok()).collect();
    assert_eq!(clients.len(), 2, "dbus-daemon's and foot's clients: {listing}");
    assert!(listing.contains("dbus-daemon") && listing.contains("bin/foot"), "{listing}");
    let st = session.wait().unwrap();
    let errors = errors.join().unwrap();
    let errors: Vec<&str> = errors.lines().filter(|l| !l.contains("macwl_compositor")).collect();
    assert!(st.success(), "{st:?}: {}", errors.join("\n"));
    // The host goes 2 s after its last process.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while dir.join("macwl-host").exists() || clients.iter().any(|&c| unsafe { libc::kill(c, 0) } == 0) {
        assert!(std::time::Instant::now() < end, "the session's host or clients stayed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Mac programs in foot, through the headless compositor (`scripts/session.sh`
/// in one host, typed at with `MACWL_TYPE`): in bash, Ctrl-Z stops Mac
/// `/bin/sleep` (`jobs`: Stopped), `bg` runs it (Running), `fg` and Ctrl-C
/// end it (130); a `/bin/sleep | tr` pipeline stops whole and continues;
/// Mac `vim` stops on Ctrl-Z, comes back with `fg` and saves.
#[test]
#[ignore = "needs nix and builds the release binaries"]
fn session_runs_mac_programs() {
    let _one = one_session();
    let dir = std::path::PathBuf::from(format!("/tmp/macwl-test-mac-{}", std::process::id()));
    let d = dir.display();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/session.sh");
    let typed = format!(
        "/bin/sleep 100\\n\\p\\p\\cZ\\pjobs > {d}/j1\\nbg\\n\\pjobs > {d}/j2\\nfg\\n\\p\\cC\\pecho $? > {d}/s1\\n\
         /bin/sleep 100 | /usr/bin/tr a b\\n\\p\\p\\cZ\\pjobs > {d}/j3\\nfg\\n\\p\\cC\\p\
         /usr/bin/vim -u NONE -N {d}/v.txt\\n\\p\\p\\pihello vim\\e\\p\\cZ\\pjobs > {d}/j4\\nfg\\n\\p\\p:wq\\n\\p\
         echo done > {d}/done\\n\\pexit\\n"
    );
    let mut session = std::process::Command::new(script)
        .env("MACWL_SESSION_COMPOSITOR", "headless")
        .env("MACWL_SESSION_DIR", &dir)
        .env("MACWL_TYPE", typed)
        .env_remove("MACWL_HOST")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let errors = rest_of_in_background(session.stderr.take().unwrap());
    let end = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while !dir.join("done").exists() {
        if std::time::Instant::now() > end || session.try_wait().unwrap().is_some() {
            let _ = session.kill();
            let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap_or_default();
            panic!("the session didn't finish: j1 {:?} j2 {:?} s1 {:?} j3 {:?} j4 {:?}", read("j1"), read("j2"), read("s1"), read("j3"), read("j4"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
    assert!(read("j1").contains("Stopped") && read("j1").contains("/bin/sleep 100"), "{}", read("j1"));
    assert!(read("j2").contains("Running"), "{}", read("j2"));
    assert_eq!(read("s1"), "130\n");
    assert!(read("j3").contains("Stopped") && read("j3").contains("/usr/bin/tr a b"), "{}", read("j3"));
    assert!(read("j4").contains("Stopped") && read("j4").contains("vim"), "{}", read("j4"));
    assert_eq!(read("v.txt"), "hello vim\n");
    let st = session.wait().unwrap();
    let errors = errors.join().unwrap();
    let errors: Vec<&str> = errors.lines().filter(|l| !l.contains("macwl_compositor")).collect();
    assert!(st.success(), "{st:?}: {}", errors.join("\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stand-in seen from the Mac: `macwl-run --ps` lists the Linux process
/// a Mac program stands in for, with the program's command line and its
/// Mac pid (marked `*`); and a client whose process exec'd a Mac program
/// ends as the program did.
#[test]
#[ignore = "needs nix"]
fn stand_ins_in_the_hosts_listing() {
    let bash = std::path::Path::new(&linux_package("bash")).join("bin/bash");
    let host = Served::start_with(&[("MACWL_MAC_PROGRAMS", "1")]);
    let mut client = host.client().arg(&bash).arg("-c").arg("/bin/sleep 2; /bin/sh -c 'exit 5'").stdin(std::process::Stdio::null()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let out = macwl_run().arg("--ps").arg("--host").arg(&host.path).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    let row = listing.lines().find(|l| l.ends_with("/bin/sleep 2")).unwrap_or_else(|| panic!("no stand-in in {listing}"));
    let mac: i32 = row.split_whitespace().nth(8).and_then(|m| m.strip_suffix('*')).and_then(|m| m.parse().ok()).unwrap_or_else(|| panic!("{row}"));
    assert_eq!(unsafe { libc::kill(mac, 0) }, 0, "its Mac program runs: {row}");
    let st = client.wait().unwrap();
    assert_eq!(st.code(), Some(5), "{st:?}");
}

/// `macwl-run --ps --all` (docs/processes.md, step 9): every host's
/// processes, through the user's coordinator, which it starts if none
/// runs; hosts that were running before it link to it as it starts. Here
/// two hosts of their own, each holding a few processes.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn ps_all_lists_every_host() {
    use std::io::{BufRead, BufReader};
    let path = format!("{}/../../target/guest/procs", env!("CARGO_MANIFEST_DIR"));
    let path = std::fs::canonicalize(path).expect("run scripts/build-guest.sh first");
    let mut hosts = Vec::new();
    for n in ["2", "3"] {
        let mut child = macwl_run()
            .args([path.as_os_str(), "many".as_ref(), n.as_ref(), "hold".as_ref()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().map(Result::unwrap), Some(format!("{n} alive")));
        hosts.push((child, lines, n));
    }
    let out = macwl_run().args(["--ps", "--all"]).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{listing}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    for (child, _, n) in &hosts {
        // The host's section: its processes, the first and its n children.
        let section: Vec<&str> = listing
            .split("host ")
            .find(|s| s.starts_with(&format!("{}\n", child.id())))
            .unwrap_or_else(|| panic!("no host {} in {listing}", child.id()))
            .lines()
            .collect();
        let rows = section.iter().filter(|l| l.ends_with(&format!("procs many {n} hold"))).count();
        assert_eq!(rows, n.parse::<usize>().unwrap() + 1, "{listing}");
    }
    for (mut child, mut lines, n) in hosts {
        drop(child.stdin.take());
        assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("procs many ok"), "{n}");
        assert!(child.wait().unwrap().success());
    }
}

/// A program set to have a host of its own moves to one as it's exec'd
/// (docs/processes.md, step 9), and stays its parent's child: `wait`, exit
/// codes, killed-by-signal statuses, signals by pid, stops and continues
/// (`jobs`), its parent's pid; one the new host can't load fails its
/// `execve` (bash: 126); `~/.config/macwl/hosts` names programs too; and
/// a moved process whose parent's host dies is its own host's init's.
/// Linux bash and coreutils from nixpkgs.
#[test]
#[ignore = "needs nix"]
fn moved_to_a_host_of_its_own() {
    let bash = std::path::Path::new(&linux_package("bash")).join("bin/bash");
    let cu = std::path::Path::new(&linux_package("coreutils")).join("bin");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad"), b"\x7fELF\x02\x01\x01 not a program, not for any machine at all, and long enough").unwrap();
    std::fs::set_permissions(dir.path().join("bad"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // A program named in the hosts file: `cat` (moved, its parent isn't in
    // its host, so it can't see the parent's /proc).
    std::fs::create_dir_all(dir.path().join(".config/macwl")).unwrap();
    std::fs::write(dir.path().join(".config/macwl/hosts"), "# programs with a host of their own\ncat\n").unwrap();
    let script = format!(
        "set -m; export PATH={cu}; d={d}
         MACWL_NEW_HOST=1 sleep 10 & p=$!; sleep 0.3; kill -TERM $p; wait $p; echo term $?
         MACWL_NEW_HOST=1 {bash} -c 'exit 5'; echo code $?
         MACWL_NEW_HOST=1 {bash} -c 'kill -KILL $$' 2>/dev/null; echo killed $?
         MACWL_NEW_HOST=1 {bash} -c 'echo ppid $PPID; test -e /proc/$PPID; echo parent seen $?'; echo parent $$
         MACWL_NEW_HOST=1 $d/bad 2>/dev/null; echo bad $?
         MACWL_NEW_HOST=1 sleep 3 & p=$!; sleep 0.3; kill -STOP $p; sleep 0.2; jobs -l > $d/j; [[ $(<$d/j) == *Stopped* ]] && echo Stopped; kill -CONT $p; sleep 0.2; jobs -l > $d/j; [[ $(<$d/j) == *Running* ]] && echo Running; kill -INT $p; wait $p; echo int $?
         test -e /proc/$$; echo self seen $?
         echo x | cat /proc/$$/stat >/dev/null 2>&1; echo cat moved $?",
        cu = cu.display(),
        d = dir.path().display(),
        bash = bash.display(),
    );
    let out = macwl_run().env("HOME", dir.path()).arg(&bash).arg("-c").arg(&script).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        "term 143\ncode 5\nkilled 137\nppid 100000\nparent seen 1\nparent 100000\nbad 126\nStopped\nRunning\nint 130\nself seen 0\ncat moved 1\n"
            .replace("100000", stdout.lines().find_map(|l| l.strip_prefix("parent ").filter(|p| p.bytes().all(|b| b.is_ascii_digit()))).unwrap_or("?")),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success());

    // Its parent's host killed: the moved process is its own host's init's.
    let file = dir.path().join("orphan");
    // (In single quotes: the moved bash's $$, not this one's.)
    let inner = format!("echo $(cut -d\" \" -f4 /proc/$$/stat) > {f}; sleep 1; echo $(cut -d\" \" -f4 /proc/$$/stat) >> {f}", f = file.display());
    let script = format!("export PATH={}; MACWL_NEW_HOST=1 {} -c '{inner}' & sleep 0.4; kill -KILL $$", cu.display(), bash.display());
    let out = macwl_run().arg(&bash).arg("-c").arg(&script).output().unwrap();
    assert_eq!(out.status.code(), Some(128 + 9), "{out:?}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let seen = loop {
        let seen = std::fs::read_to_string(&file).unwrap_or_default();
        if seen.lines().count() == 2 || std::time::Instant::now() > deadline {
            break seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let ppids: Vec<&str> = seen.lines().collect();
    assert_eq!(ppids.len(), 2, "{seen:?}");
    assert_ne!(ppids[0], "1", "{seen:?}");
    assert_eq!(ppids[1], "1", "{seen:?}");
}

/// A moved process's host dying ends it in its parent's eyes as killed by
/// SIGKILL (docs/processes.md, step 9, "A host's failure").
#[test]
#[ignore = "needs nix"]
fn a_moved_hosts_death_is_a_sigkill() {
    use std::io::Read;
    let bash = std::path::Path::new(&linux_package("bash")).join("bin/bash");
    let cu = std::path::Path::new(&linux_package("coreutils")).join("bin");
    let script = format!("MACWL_NEW_HOST=1 {}/sleep 30; echo died $?", cu.display());
    let mut child = macwl_run().arg(&bash).arg("-c").arg(&script).stdout(std::process::Stdio::piped()).spawn().unwrap();
    // The moved process's host, from the listing: the stand-in's mark.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let moved: i32 = loop {
        let out = macwl_run().args(["--ps", "--all"]).output().unwrap();
        let listing = String::from_utf8_lossy(&out.stdout).into_owned();
        let found = listing
            .lines()
            .filter(|l| l.ends_with("sleep 30"))
            .find_map(|l| l.split_whitespace().nth(8)?.strip_suffix('@')?.parse().ok());
        if let Some(pid) = found {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "no moved process in {listing}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(unsafe { libc::kill(moved, libc::SIGKILL) }, 0);
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "died 137\n");
    assert!(child.wait().unwrap().success());
}

fn shmfutex_guest() -> std::path::PathBuf {
    let path = format!("{}/../../target/guest/shmfutex", env!("CARGO_MANIFEST_DIR"));
    std::fs::canonicalize(path).expect("run scripts/build-guest.sh first")
}

/// `guest/shmfutex` with `args`: its stdout, after
/// checking it ended well and the runtime said nothing of futexes.
fn shmfutex(args: &[&str]) -> String {
    let out = macwl_run().arg(shmfutex_guest()).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && stdout.ends_with(&format!("shmfutex {} ok\n", args[0])), "{args:?}: {stdout}, stderr: {stderr}");
    assert!(!stderr.contains("futex returned") && !stderr.contains("internal result"), "{args:?}: {stderr}");
    stdout
}

/// Process-shared mutexes, condition variables, semaphores and barriers
/// across processes: forked ones sharing anonymous memory, and a program
/// exec'd afresh mapping a POSIX shm object by name, or a memfd it was
/// passed over a Unix socket.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn process_shared_sync() {
    for test in ["fork", "shm", "memfd"] {
        shmfutex(&[test]);
    }
}

/// The same between processes of two clients of one host: one makes a POSIX shm object, the other maps it by name.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn process_shared_sync_between_clients() {
    let host = Served::start();
    let name = format!("/shmfutex.clients.{}", std::process::id());
    let maker = host.client().arg(shmfutex_guest()).args(["shm", &name]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let peer = host.client().arg(shmfutex_guest()).args(["peer", "shm", &name]).output().unwrap();
    let made = maker.wait_with_output().unwrap();
    assert!(peer.status.success(), "peer: {peer:?}");
    assert_eq!(String::from_utf8_lossy(&made.stdout), "shmfutex shm ok\n", "{made:?}");
}

/// Robust mutexes, plain and priority-inheriting, whose holder exits
/// holding them: the next locker gets EOWNERDEAD, whether it was waiting
/// already or came later; and whose holder is killed with SIGKILL.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn robust_mutexes() {
    shmfutex(&["robust"]);
    shmfutex(&["robust", "kill"]);
}

/// A priority-inheriting process-shared mutex (FUTEX_LOCK_PI and
/// UNLOCK_PI) contended by three processes; trylock while another holds it.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn pi_mutexes() {
    shmfutex(&["pi"]);
}

/// FUTEX_CMP_REQUEUE, FUTEX_WAKE_OP and futex_waitv between processes,
/// with the counts of waiters woken and moved.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn futex_ops_across_processes() {
    for test in ["requeue", "wakeop", "waitv"] {
        shmfutex(&[test]);
    }
}

/// Two processes see each other's writes through a file, POSIX shm, a
/// memfd (each mapped afresh by both) and shared anonymous memory; one
/// unmapping leaves the other's view; a private mapping's first write
/// stays its own while its unwritten pages show the file's changes.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn shared_windows() {
    shmfutex(&["windows"]);
}

/// A file mapped by many processes of a host (as a library is) is one
/// host mapping however many map it, as `vmmap` lists the host's.
#[test]
#[ignore = "needs scripts/build-guest.sh (nix cross toolchain)"]
fn one_window_for_a_library_in_many_processes() {
    use std::io::{BufRead, BufReader, Write};
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("libbig.so");
    std::fs::File::create(&lib).unwrap().write_all(&vec![b'x'; 8 << 20]).unwrap();
    let mappings = |n: usize| {
        let mut child = macwl_run()
            .arg(shmfutex_guest())
            .args(["library".as_ref(), lib.as_os_str(), n.to_string().as_ref()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().map(Result::unwrap), Some(format!("{n} mapped")));
        let vmmap = std::process::Command::new("vmmap").arg("-wide").arg(child.id().to_string()).output().unwrap();
        let count = String::from_utf8_lossy(&vmmap.stdout).lines().filter(|l| l.contains("libbig.so")).count();
        drop(child.stdin.take());
        assert_eq!(lines.next().map(Result::unwrap).as_deref(), Some("shmfutex library ok"));
        assert!(child.wait().unwrap().success());
        count
    };
    let one = mappings(1);
    assert!(one >= 1, "the file is mapped in the host");
    assert_eq!(mappings(30), one, "30 processes, as many host mappings as for one");
}
