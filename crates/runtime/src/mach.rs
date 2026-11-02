//! The few Mach calls fork needs: copy-on-write snapshots of guest memory as
//! memory entries, and ports to hand those to another process.
//!
//! Why Mach: a forked child is a fresh `macwl-run` (Hypervisor.framework
//! isn't safe across a host `fork()`), so it needs the parent's memory handed
//! over. A file would do it, but macOS writes dirty file-backed pages back
//! to disk (~4 GB/min for a busy 1 GiB guest; see examples/disk_writes.rs).
//! A memory entry made with MAP_MEM_VM_COPY is an in-memory copy-on-write
//! snapshot, and a new task inherits its parent's registered ports.

use std::io;

type KernReturn = i32;
type MachPort = u32;

unsafe extern "C" {
    fn mach_task_self() -> MachPort;
    fn mach_make_memory_entry_64(task: MachPort, size: *mut u64, offset: u64, perm: i32, handle: *mut MachPort, parent: MachPort) -> KernReturn;
    fn mach_vm_map(
        task: MachPort,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: MachPort,
        offset: u64,
        copy: u32,
        cur: i32,
        max: i32,
        inherit: u32,
    ) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn mach_ports_register(task: MachPort, ports: *const MachPort, count: u32) -> KernReturn;
    fn mach_ports_lookup(task: MachPort, ports: *mut *mut MachPort, count: *mut u32) -> KernReturn;
    fn mach_vm_deallocate(task: MachPort, addr: u64, size: u64) -> KernReturn;
    fn mach_port_allocate(task: MachPort, right: u32, name: *mut MachPort) -> KernReturn;
    fn mach_port_insert_right(task: MachPort, name: MachPort, right: MachPort, disposition: u32) -> KernReturn;
    fn mach_port_mod_refs(task: MachPort, name: MachPort, right: u32, delta: i32) -> KernReturn;
    fn mach_msg(msg: *mut MsgHeader, option: i32, send_size: u32, rcv_size: u32, rcv_name: MachPort, timeout: u32, notify: MachPort) -> KernReturn;
}

const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_RCV_TIMED_OUT: KernReturn = 0x1000_4003;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;

#[repr(C)]
struct MsgHeader {
    bits: u32,
    size: u32,
    remote: MachPort,
    local: MachPort,
    voucher: MachPort,
    id: i32,
}

/// A message carrying one port and one number.
#[repr(C)]
struct PortMsg {
    header: MsgHeader,
    descriptor_count: u32,
    // mach_msg_port_descriptor_t
    port: MachPort,
    _pad1: u32,
    _pad2: u16,
    disposition: u8,
    kind: u8,
    value: u64,
}

/// Room for a [`PortMsg`] plus the trailer the kernel appends.
#[repr(C)]
struct PortMsgBuffer {
    msg: PortMsg,
    trailer: [u8; 128],
}

/// A port we hold the receive right to.
#[derive(Debug)]
pub struct ReceivePort(MachPort);

impl ReceivePort {
    pub fn new() -> io::Result<Self> {
        let mut name = 0;
        check("mach_port_allocate", unsafe { mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut name) })?;
        Ok(ReceivePort(name))
    }

    /// A send right to it.
    pub fn sender(&self) -> io::Result<SendPort> {
        let kr = unsafe { mach_port_insert_right(mach_task_self(), self.0, self.0, MACH_MSG_TYPE_MAKE_SEND) };
        check("mach_port_insert_right", kr)?;
        Ok(SendPort(self.0))
    }

    /// Wait up to `timeout_ms` for a message carrying a send right and a
    /// number (see [`SendPort::send`]). None if none came in time.
    pub fn receive(&self, timeout_ms: u32) -> io::Result<Option<(SendPort, u64)>> {
        let mut buf: PortMsgBuffer = unsafe { std::mem::zeroed() };
        let size = size_of::<PortMsgBuffer>() as u32;
        let kr = unsafe { mach_msg(&mut buf.msg.header, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, size, self.0, timeout_ms, 0) };
        if kr == MACH_RCV_TIMED_OUT {
            return Ok(None);
        }
        check("mach_msg (receive)", kr)?;
        if buf.msg.header.bits & MACH_MSGH_BITS_COMPLEX == 0 || buf.msg.descriptor_count != 1 {
            return Err(io::Error::other("unexpected Mach message"));
        }
        Ok(Some((SendPort(buf.msg.port), buf.msg.value)))
    }
}

impl Drop for ReceivePort {
    fn drop(&mut self) {
        unsafe { mach_port_mod_refs(mach_task_self(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

/// A send right to someone's port.
#[derive(Debug)]
pub struct SendPort(MachPort);

impl SendPort {
    /// Send (a copy of) send right `right`, and `value`.
    pub fn send_port(&self, right: &SendPort, value: u64) -> io::Result<()> {
        self.send(right.0, value)
    }

    /// Send a send right to `port` (a copy: we keep ours) and `value`.
    fn send(&self, port: MachPort, value: u64) -> io::Result<()> {
        let mut msg = PortMsg {
            header: MsgHeader {
                bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                size: size_of::<PortMsg>() as u32,
                remote: self.0,
                local: 0,
                voucher: 0,
                id: 0,
            },
            descriptor_count: 1,
            port,
            _pad1: 0,
            _pad2: 0,
            disposition: MACH_MSG_TYPE_COPY_SEND as u8,
            kind: MACH_MSG_PORT_DESCRIPTOR,
            value,
        };
        // Never block for long: the other side is our own child or parent.
        let kr = unsafe { mach_msg(&mut msg.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, msg.header.size, 0, 0, 1000, 0) };
        check("mach_msg (send)", kr)
    }

    /// Make this the port a task created from now on inherits (until
    /// [`unregister`]). Process-wide.
    pub fn register(&self) -> io::Result<()> {
        check("mach_ports_register", unsafe { mach_ports_register(mach_task_self(), &self.0, 1) })
    }

    /// In a spawned process: the port its parent registered.
    pub fn inherited() -> io::Result<Self> {
        let (mut ports, mut n): (*mut MachPort, u32) = (std::ptr::null_mut(), 0);
        check("mach_ports_lookup", unsafe { mach_ports_lookup(mach_task_self(), &mut ports, &mut n) })?;
        let found = (n > 0).then(|| unsafe { *ports }).filter(|&p| p != 0);
        if !ports.is_null() {
            // The array itself is out-of-line memory the kernel gave us.
            unsafe { mach_vm_deallocate(mach_task_self(), ports as u64, (n as usize * size_of::<MachPort>()) as u64) };
        }
        found.map(SendPort).ok_or_else(|| io::Error::other("no port was handed to this process"))
    }

    /// A send right as a memory entry (what [`MemoryEntry::send_to`] sent).
    pub fn into_memory_entry(self) -> MemoryEntry {
        let entry = MemoryEntry(self.0);
        std::mem::forget(self);
        entry
    }
}

impl Drop for SendPort {
    fn drop(&mut self) {
        unsafe { mach_port_deallocate(mach_task_self(), self.0) };
    }
}

/// Stop handing a registered port to new tasks.
pub fn unregister() {
    unsafe { mach_ports_register(mach_task_self(), std::ptr::null(), 0) };
}



const VM_PROT_READ_WRITE: i32 = 3;
const MAP_MEM_VM_COPY: i32 = 0x20_0000;
const VM_FLAGS_FIXED: i32 = 0;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const VM_INHERIT_COPY: u32 = 1;

fn check(call: &str, kr: KernReturn) -> io::Result<()> {
    if kr == 0 { Ok(()) } else { Err(io::Error::other(format!("{call} failed: kern_return {kr:#x}"))) }
}

/// A send right to a memory entry.
#[derive(Debug)]
pub struct MemoryEntry(MachPort);

impl MemoryEntry {
    /// A copy-on-write snapshot of `[addr, addr + len)` in this task.
    pub fn snapshot(addr: *mut u8, len: u64) -> io::Result<Self> {
        let (mut size, mut port) = (len, 0);
        let perm = VM_PROT_READ_WRITE | MAP_MEM_VM_COPY;
        check("mach_make_memory_entry_64", unsafe { mach_make_memory_entry_64(mach_task_self(), &mut size, addr as u64, perm, &mut port, 0) })?;
        let entry = MemoryEntry(port);
        if size != len {
            return Err(io::Error::other(format!("memory entry covers {size:#x} bytes, not {len:#x}")));
        }
        Ok(entry)
    }

    /// Map the entry over `[addr, addr + len)` in this task, replacing what's
    /// there.
    pub fn map_at(&self, addr: *mut u8, len: u64) -> io::Result<()> {
        let mut at = addr as u64;
        let flags = VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE;
        let kr = unsafe {
            mach_vm_map(mach_task_self(), &mut at, len, 0, flags, self.0, 0, 0, VM_PROT_READ_WRITE, VM_PROT_READ_WRITE, VM_INHERIT_COPY)
        };
        check("mach_vm_map", kr)
    }

    /// Send (a copy of) this entry to `to`.
    pub fn send_to(&self, to: &SendPort, value: u64) -> io::Result<()> {
        to.send(self.0, value)
    }
}

impl Drop for MemoryEntry {
    fn drop(&mut self) {
        unsafe { mach_port_deallocate(mach_task_self(), self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_copy_on_write() {
        const LEN: usize = 1 << 20;
        let map = || unsafe {
            libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as *mut u8
        };
        let (src, dst) = (map(), map());
        unsafe { *src.add(4096) = 1 };
        let snap = MemoryEntry::snapshot(src, LEN as u64).unwrap();
        unsafe { *src.add(4096) = 2 };
        snap.map_at(dst, LEN as u64).unwrap();
        unsafe { *dst.add(8192) = 3 };
        assert_eq!(unsafe { (*dst.add(4096), *src.add(4096), *src.add(8192)) }, (1, 2, 0));
    }

    #[test]
    fn ports_carry_memory_entries() {
        const LEN: usize = 1 << 20;
        let map = || unsafe {
            libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as *mut u8
        };
        let (src, dst) = (map(), map());
        unsafe { *src = 9 };
        let inbox = ReceivePort::new().unwrap();
        let to = inbox.sender().unwrap();
        MemoryEntry::snapshot(src, LEN as u64).unwrap().send_to(&to, 0).unwrap();
        let (got, value) = inbox.receive(1000).unwrap().unwrap();
        assert_eq!(value, 0);
        got.into_memory_entry().map_at(dst, LEN as u64).unwrap();
        assert_eq!(unsafe { *dst }, 9);
        assert!(inbox.receive(10).unwrap().is_none(), "times out when empty");
    }
}
