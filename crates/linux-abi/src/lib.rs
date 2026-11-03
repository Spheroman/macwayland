//! Linux arm64 userspace ABI pieces for running Linux programs on macOS.
//!
//! Status: partial. This crate has the data-level building blocks (system call
//! numbers, errno translation, struct layouts, fd table, path mapping), the
//! [`GuestMemory`] interface the core provides, and the system call
//! [`dispatch`]er: memory, files and directories, an emulated `/proc`, pipes,
//! `ppoll`, and sockets with fd passing — see ROADMAP.md for what's missing.

pub mod abi;
pub mod errno;
pub mod fs;
pub mod host_threads;
pub mod hostinfo;
pub mod identity;
pub mod memory;
pub mod nr;
mod proc;
pub mod shm;
pub mod syscall;
pub mod wire;

pub use memory::{Fault, GuestMemory, Region, SharedKey};
pub use syscall::{dispatch, Action, Process};
