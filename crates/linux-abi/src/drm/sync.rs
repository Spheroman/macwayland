//! Fences, DRM sync objects and sync files, as Linux's DRM core keeps them
//! for a render node (docs/gpu.md, "Sync and presenting").
//!
//! - A [`Fence`] signals once (the GPU finished a submission, or it was
//!   made signalled), running what waits on it.
//! - A [`Syncobj`] is a timeline: fences by point, point 0 being a binary
//!   syncobj's one payload. A point is signalled once the fence at it, or
//!   at the first point after it, is (a timeline's fences signal in order).
//! - A sync file is an fd for one fence, readable once it signals: the near
//!   end of a socket pair whose far end the fence holds, and closes as it
//!   signals (as a pidfd's is, `syscall::pidfd`). An fd for a syncobj
//!   (`HANDLE_TO_FD` without EXPORT_SYNC_FILE) is a socket pair's end too.
//!   Both are known again by their inode when they come back, through
//!   `SCM_RIGHTS` or another process (`registry`).

use std::{
    collections::{BTreeMap, HashMap},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Instant,
};

use crate::{errno, syscall::Waker};

type Callback = Box<dyn FnOnce() + Send>;

/// One fence.
#[derive(Default)]
pub struct Fence {
    state: Mutex<FenceState>,
    /// When it went through each stage, with `MACWL_GPU_TRACE` on
    /// (`super::trace`).
    pub(super) stamps: Mutex<super::trace::Stamps>,
}

#[derive(Default)]
struct FenceState {
    signaled: bool,
    /// An error it signalled with (the GPU process died: device lost).
    error: bool,
    then: Vec<Callback>,
}

impl std::fmt::Debug for Fence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fence({})", self.signaled())
    }
}

impl Fence {
    pub fn new() -> Arc<Fence> {
        Arc::new(Fence::default())
    }

    /// A fence that has signalled already.
    pub fn signaled_now() -> Arc<Fence> {
        let f = Fence::new();
        f.signal();
        f
    }

    pub fn signaled(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).signaled
    }

    pub fn failed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).error
    }

    /// Signal it (once; later calls do nothing).
    pub fn signal(&self) {
        self.finish(false);
    }

    /// Signal it as failed: what waits on it is told the device was lost.
    pub fn fail(&self) {
        self.finish(true);
    }

    fn finish(&self, error: bool) {
        super::trace::stamp(self, super::trace::Stage::Signaled);
        let then = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.signaled {
                return;
            }
            s.signaled = true;
            s.error = error;
            std::mem::take(&mut s.then)
        };
        for f in then {
            f();
        }
    }

    /// Run `f` once it has signalled (at once if it has).
    pub fn then(&self, f: impl FnOnce() + Send + 'static) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !s.signaled {
                s.then.push(Box::new(f));
                return;
            }
        }
        f();
    }

    /// A fence signalling once all of `fences` have.
    pub fn all(fences: &[Arc<Fence>]) -> Arc<Fence> {
        let out = Fence::new();
        let left = Arc::new(Mutex::new(fences.len()));
        if fences.is_empty() {
            out.signal();
        }
        for f in fences {
            let (out, left, of) = (out.clone(), left.clone(), f.clone());
            f.then(move || {
                if of.failed() {
                    out.fail();
                }
                let mut n = left.lock().unwrap_or_else(|e| e.into_inner());
                *n -= 1;
                if *n == 0 {
                    out.signal();
                }
            });
        }
        out
    }
}

/// Wait (thread `t`) until `ready` says so, `ready` being looked at again
/// each time `register`ed fences wake the thread; ETIME at `deadline`.
/// EINTR for a signal to take, and PARK (made again) as any wait.
pub fn wait_until(
    p: &crate::Process,
    t: &crate::syscall::Thread,
    deadline: Option<Instant>,
    mut ready: impl FnMut(&dyn Fn(&Arc<Fence>)) -> Result<bool, i64>,
) -> Result<(), i64> {
    loop {
        let waker = t.waker.clone();
        let register = move |f: &Arc<Fence>| {
            let w = waker.clone();
            f.then(move || w.wake());
        };
        if ready(&register)? {
            return Ok(());
        }
        if p.signal_ready(t) || p.exiting() {
            return Err(errno::EINTR);
        }
        if !crate::syscall::wait_woken(t, deadline)? {
            return if ready(&|_| {})? { Ok(()) } else { Err(errno::ETIME) };
        }
    }
}

/// A DRM sync object.
#[derive(Default, Debug)]
pub struct Syncobj {
    /// Fences by point (0: a binary syncobj's payload).
    points: Mutex<BTreeMap<u64, Arc<Fence>>>,
    /// Woken as fences are attached (waits for submission).
    attached: Mutex<Vec<Arc<Waker>>>,
    /// The far ends of fds made for it (`fd_for`), kept while it lives.
    ends: Mutex<Vec<OwnedFd>>,
}

impl Syncobj {
    pub fn new(signaled: bool) -> Arc<Syncobj> {
        let s = Arc::new(Syncobj::default());
        if signaled {
            s.attach(0, Fence::signaled_now());
        }
        s
    }

    /// The fence at `point` (or, for a timeline, the first after it), if
    /// one has been attached.
    pub fn fence(&self, point: u64) -> Option<Arc<Fence>> {
        let points = self.points.lock().unwrap_or_else(|e| e.into_inner());
        if point == 0 {
            return points.get(&0).cloned();
        }
        points.range(point..).next().map(|(_, f)| f.clone())
    }

    /// Attach `fence` at `point` (0: replace the binary payload).
    pub fn attach(&self, point: u64, fence: Arc<Fence>) {
        self.points.lock().unwrap_or_else(|e| e.into_inner()).insert(point, fence);
        for w in std::mem::take(&mut *self.attached.lock().unwrap_or_else(|e| e.into_inner())) {
            w.wake();
        }
    }

    /// Take out its payload: the binary one, and a timeline's every point
    /// (as Linux's reset replaces the whole fence chain; Venus then
    /// submits point 1 again).
    pub fn reset(&self) {
        self.points.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The last point signalled, every one before it too (0 for none).
    pub fn signaled_point(&self) -> u64 {
        let points = self.points.lock().unwrap_or_else(|e| e.into_inner());
        let mut last = 0;
        for (&p, f) in points.range(1..) {
            if !f.signaled() {
                break;
            }
            last = p;
        }
        last
    }

    /// The last point a fence has been attached at (0 for none).
    pub fn last_point(&self) -> u64 {
        self.points.lock().unwrap_or_else(|e| e.into_inner()).range(1..).next_back().map_or(0, |(&p, _)| p)
    }

    /// For a wait for submission: wake `waker` when a fence is attached.
    pub fn on_attach(&self, waker: Arc<Waker>) {
        self.attached.lock().unwrap_or_else(|e| e.into_inner()).push(waker);
    }

    /// Drop fences before the last signalled point, to keep timelines from
    /// growing for ever.
    pub fn trim(&self) {
        let last = self.signaled_point();
        if last > 0 {
            let mut points = self.points.lock().unwrap_or_else(|e| e.into_inner());
            let keep = points.split_off(&last);
            let binary = points.remove(&0);
            *points = keep;
            if let Some(b) = binary {
                points.insert(0, b);
            }
        }
    }
}

/// What an fd made here stands for.
#[derive(Debug, Clone)]
pub enum Token {
    SyncFile(Arc<Fence>),
    Syncobj(Arc<Syncobj>),
}

#[derive(Debug, Clone)]
enum WeakToken {
    SyncFile(Weak<Fence>),
    Syncobj(Weak<Syncobj>),
}

/// Fds made here, by the (device, inode) of their host end.
fn registry() -> &'static Mutex<HashMap<(u64, u64), WeakToken>> {
    static R: OnceLock<Mutex<HashMap<(u64, u64), WeakToken>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

fn identity(fd: i32) -> Option<(u64, u64)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev as u64, st.st_ino))
}

/// What host fd `fd` stands for, if it's one made here.
pub fn token_of(fd: i32) -> Option<Token> {
    let id = identity(fd)?;
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    let token = match r.get(&id)? {
        WeakToken::SyncFile(f) => f.upgrade().map(Token::SyncFile),
        WeakToken::Syncobj(s) => s.upgrade().map(Token::Syncobj),
    };
    if token.is_none() {
        r.remove(&id);
    }
    token
}

/// A new host fd standing for `token`; for a sync file, readable once its
/// fence signals.
pub fn fd_for(token: Token) -> Result<OwnedFd, i64> {
    let (near, far) = UnixStream::pair().map_err(|e| errno::from_io(&e))?;
    let near: OwnedFd = near.into();
    let far: OwnedFd = far.into();
    let id = identity(near.as_raw_fd()).ok_or(errno::EIO)?;
    let weak = match &token {
        Token::SyncFile(f) => WeakToken::SyncFile(Arc::downgrade(f)),
        Token::Syncobj(s) => WeakToken::Syncobj(Arc::downgrade(s)),
    };
    registry().lock().unwrap_or_else(|e| e.into_inner()).insert(id, weak);
    match token {
        // The far end goes as the fence signals.
        Token::SyncFile(f) => f.then(move || drop(far)),
        // Never readable: the far end lives as long as the syncobj does.
        Token::Syncobj(s) => s.ends.lock().unwrap_or_else(|e| e.into_inner()).push(far),
    }
    Ok(near)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_points() {
        let s = Syncobj::new(false);
        let (a, b) = (Fence::new(), Fence::new());
        s.attach(1, a.clone());
        s.attach(3, b.clone());
        assert_eq!(s.signaled_point(), 0);
        assert!(Arc::ptr_eq(&s.fence(2).unwrap(), &b));
        assert!(s.fence(4).is_none());
        a.signal();
        assert_eq!(s.signaled_point(), 1);
        b.signal();
        assert_eq!(s.signaled_point(), 3);
        s.trim();
        assert!(s.fence(1).is_some_and(|f| Arc::ptr_eq(&f, &b)));
    }

    #[test]
    fn a_sync_file_reads_ready_once_signalled() {
        let f = Fence::new();
        let fd = fd_for(Token::SyncFile(f.clone())).unwrap();
        let ready = |fd: &OwnedFd| {
            let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, 0) == 1 }
        };
        assert!(!ready(&fd));
        assert!(matches!(token_of(fd.as_raw_fd()), Some(Token::SyncFile(g)) if Arc::ptr_eq(&g, &f)));
        f.signal();
        assert!(ready(&fd));
    }

    #[test]
    fn all_of_several() {
        let (a, b) = (Fence::new(), Fence::new());
        let both = Fence::all(&[a.clone(), b.clone()]);
        a.signal();
        assert!(!both.signaled());
        b.signal();
        assert!(both.signaled());
    }
}
