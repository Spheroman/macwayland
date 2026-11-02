//! `signalfd`: signals read from a file instead of run as handlers.
//!
//! A program blocks the signals and reads them as `signalfd_siginfo`s, taken
//! from the process's pending set. Like an `eventfd`, it's a host kqueue with
//! a user event, so it can be polled and watched by epoll. The user event is
//! triggered whenever a signal is made pending (and spuriously at times: a
//! read then finds nothing and waits again). Signals from outside arrive as
//! host signals, which the runtime catches for every signal a signalfd has
//! watched, whatever its action, so that they're pending here rather than
//! the host's to act on (see `Signals::watch_for_signalfd`). The host
//! handler triggers the user event itself, after making the signal pending,
//! so a reader can't wake before it's there.
//!
//! Signals sent to one thread (`tgkill`) aren't read here: only the
//! process's.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{atomic::Ordering, Arc, Mutex},
};

use super::{
    kfd, ok,
    signal::{Inbox, SigInfo, ThreadSignals},
    Process,
};
use crate::{
    errno,
    fs::{FdEntry, FileKind, OpenFile},
    memory::{GuestMemory, GuestMemoryExt},
};

const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;
/// SIGKILL and SIGSTOP can't be read (or blocked).
const UNREADABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));
/// `sizeof(struct signalfd_siginfo)`.
const SIGINFO_SIZE: usize = 128;

/// A signal was made pending for the process with `inbox`: wake its
/// signalfds' readers. Safe to call from a signal handler.
pub(super) fn notify(inbox: &Inbox) {
    for slot in &inbox.kqueues {
        let kq = slot.load(Ordering::Acquire);
        if kq >= 0 {
            // EBADF if it was just closed (or another kqueue, now at that
            // number, gets a stray wakeup): harmless either way.
            let _ = kfd::change(kq, kfd::user_event(0, libc::NOTE_TRIGGER));
        }
    }
}

/// A signalfd: the signals it reads, and its kqueue's slot in its process's
/// [`Inbox`].
#[derive(Debug)]
pub struct SignalFd {
    mask: Mutex<u64>,
    /// The kqueue's copy that the inbox's slot points to.
    kq: OwnedFd,
    inbox: Arc<Inbox>,
    slot: usize,
}

impl Drop for SignalFd {
    fn drop(&mut self) {
        self.inbox.kqueues[self.slot].store(-1, Ordering::Release);
    }
}

impl SignalFd {
    /// For a forked child: the signals it reads.
    pub fn mask(&self) -> u64 {
        *self.mask.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn new_signalfd(inbox: &Arc<Inbox>, mask: u64, nonblock: bool) -> Result<FdEntry, i64> {
    let kq = kfd::new_kqueue(nonblock)?;
    kfd::change(kq.as_raw_fd(), kfd::user_event(libc::EV_ADD | libc::EV_CLEAR, 0))?;
    let own = ok(unsafe { libc::dup(kq.as_raw_fd()) } as i64)? as RawFd;
    let own = unsafe { OwnedFd::from_raw_fd(own) };
    let slot = inbox
        .kqueues
        .iter()
        .position(|s| s.compare_exchange(-1, own.as_raw_fd(), Ordering::AcqRel, Ordering::Acquire).is_ok())
        .ok_or(errno::EMFILE)?;
    // Anything already pending is there to read.
    kfd::change(kq.as_raw_fd(), kfd::user_event(0, libc::NOTE_TRIGGER))?;
    let mut entry = FdEntry::new(kq);
    entry.kind = FileKind::SignalFd(SignalFd { mask: Mutex::new(mask), kq: own, inbox: inbox.clone(), slot });
    Ok(entry)
}

/// A host thread's wait for host signals caught for an inbox of its own (a
/// host of many processes routes them itself: see
/// [`Inbox::receive_host_signals`]). A signalfd of no process, underneath.
#[derive(Debug)]
pub struct HostSignalWait {
    kq: FdEntry,
    inbox: Arc<Inbox>,
}

impl HostSignalWait {
    pub fn new(inbox: &Arc<Inbox>) -> Result<Self, i64> {
        Ok(HostSignalWait { kq: new_signalfd(inbox, 0, false)?, inbox: inbox.clone() })
    }

    /// Wait until host signals come, and take them (Linux numbering, bit
    /// n-1 for signal n).
    pub fn wait(&self) -> u64 {
        loop {
            let got = self.inbox.take_host_pending();
            if got != 0 {
                return got;
            }
            // The trigger stays set until taken: a signal between the look
            // and the wait ends the wait at once.
            let _ = kfd::wait(self.kq.host.as_raw_fd());
        }
    }
}

/// A forked child's copy of a signalfd, for the child's `inbox`.
pub(crate) fn signalfd_copy(inbox: &Arc<Inbox>, mask: u64, nonblock: bool) -> Result<FdEntry, i64> {
    new_signalfd(inbox, mask, nonblock)
}

/// `signalfd4(fd, mask, sizemask, flags)`: a new signalfd (fd -1), or a new
/// mask for one.
pub(super) fn signalfd4<M: GuestMemory + ?Sized>(p: &Process, mem: &M, fd: u64, mask: u64, size: u64, flags: u64) -> Result<i64, i64> {
    if size != 8 || flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let mask = mem.read_pod::<u64>(mask)? & !UNREADABLE;
    p.signals().watch_for_signalfd(mask);
    if fd as i32 != -1 {
        let file = p.fds.get(fd)?;
        let FileKind::SignalFd(signalfd) = &file.kind else { return Err(errno::EINVAL) };
        *signalfd.mask.lock().unwrap_or_else(|e| e.into_inner()) = mask;
        kfd::change(signalfd.kq.as_raw_fd(), kfd::user_event(0, libc::NOTE_TRIGGER))?;
        return Ok(fd as i64);
    }
    let entry = new_signalfd(&p.inbox, mask, flags & O_NONBLOCK != 0)?;
    Ok(p.fds.insert(entry.cloexec(flags & O_CLOEXEC != 0)))
}

/// A `signalfd_siginfo`.
fn siginfo(info: &SigInfo) -> [u8; SIGINFO_SIZE] {
    let mut b = [0u8; SIGINFO_SIZE];
    let mut put = |at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
    put(0, &(info.signo as u32).to_le_bytes());
    put(8, &info.code.to_le_bytes());
    put(12, &(info.pid as u32).to_le_bytes());
    put(16, &info.uid.to_le_bytes());
    if let Some((id, overrun, value)) = info.timer {
        put(24, &(id as u32).to_le_bytes()); // ssi_tid: the timer's id
        put(32, &(overrun as u32).to_le_bytes());
        put(48, &value.to_le_bytes()); // ssi_ptr
    }
    put(40, &info.status.to_le_bytes()); // ssi_status (SIGCHLD)
    put(72, &info.addr.to_le_bytes());
    b
}

/// `read` on a signalfd: as many of its pending signals as fit.
pub(crate) fn signalfd_read(p: &Process, file: &OpenFile, signalfd: &SignalFd, buf: &mut [u8]) -> Result<i64, i64> {
    if buf.len() < SIGINFO_SIZE {
        return Err(errno::EINVAL);
    }
    let kq = file.host.as_raw_fd();
    loop {
        let mask = signalfd.mask();
        // The wakeups so far are taken before the signals are: one made
        // pending from here on wakes the kqueue again (taken after, it
        // could take the wakeup of a signal that came in between, and leave
        // the signal there with nothing to say so).
        kfd::drain(kq);
        let mut n = 0;
        {
            let mut s = p.signals();
            // Host signals and timers' too, for the process (no thread).
            p.take_external(&mut s, &ThreadSignals::default(), -1);
            while n + SIGINFO_SIZE <= buf.len() {
                let Some(info) = s.pending.take(mask) else { break };
                buf[n..n + SIGINFO_SIZE].copy_from_slice(&siginfo(&info));
                n += SIGINFO_SIZE;
            }
            if n > 0 {
                if s.pending.any(mask) {
                    kfd::change(kq, kfd::user_event(0, libc::NOTE_TRIGGER))?;
                }
                return Ok(n as i64);
            }
        }
        if kfd::nonblocking(file) {
            return Err(errno::EAGAIN);
        }
        match kfd::wait(kq) {
            Ok(()) => {}
            // A host signal ran its handler on this thread: one of ours (go
            // round and read it), or another (for the guest to handle).
            Err(e) if e == errno::EINTR => {
                let mut s = p.signals();
                p.take_external(&mut s, &ThreadSignals::default(), -1);
                if !s.pending.any(mask) {
                    return Err(errno::EINTR);
                }
            }
            Err(e) => return Err(e),
        }
    }
}
