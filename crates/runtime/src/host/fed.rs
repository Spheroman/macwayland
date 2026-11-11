//! Federated hosts (docs/processes.md, section 1, "Federated hosts"; step
//! 9): a program can run in a host of its own and stay an ordinary
//! process of its session.
//!
//! It moves at `execve`, when the program is one set to have a host of its
//! own (`MACWL_NEW_HOST=1` in the environment the `execve` passes, or its
//! path or name in `~/.config/macwl/hosts`). The process's host starts a
//! new one (`macwl-run --adopted`, a new Mac process, never a fork) with a
//! socket to it, and hands it the process (`ADOPT`): its pid, parent, group,
//! session and terminal, what it keeps across an exec ([`Process::encode_for_move`])
//! and its fds. The new host loads the program and says whether it could
//! (`ADOPTED`, or `REFUSED` with the errno `execve` then returns), and the
//! process becomes a stand-in in the old host's table, as for a Mac
//! program (section 7): its parent `wait`s for it there, and signals to it
//! go to its host.
//!
//! The socket stays as the link between the two hosts (the doc's design
//! had every message go through the coordinator; a direct link is one hop,
//! and its end is the other host's end): the new host reports the
//! process's stops, continues and end on it ([`CHANGED`]), as an agent
//! reports a Mac program's; the old host sends it signals ([`SIGNAL`]) and
//! its new parent if it's orphaned ([`REPARENT`]). If the new host dies,
//! the stand-in ends as killed by SIGKILL; if the old one does, the moved
//! process's parent has gone, and it's the new host's init's.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    sync::{Arc, Condvar, Mutex, Weak},
    time::Duration,
};

use linux_abi::{
    errno,
    syscall::{Exec, ProcStatus, SigInfo, Thread},
    wire::{Decoder, Encoder, Truncated},
    Process,
};

use super::{Elsewhere, First, RangePids, StandIn, Table, Target};
use crate::{mac::Change, proto, Error, Exit, Proc};

/// Old host to new: the process (its fds with it, and in [`FDS`] messages
/// after it).
const ADOPT: u8 = 1;
/// Old host to new: more of the fds.
const FDS: u8 = 2;
/// Old host to new: a signal for the process, or one of its threads.
const SIGNAL: u8 = 3;
/// Old host to new: the process's parent is this one now (its old one
/// ended).
const REPARENT: u8 = 4;
/// New host to old: the program is loaded and runs.
const ADOPTED: u8 = 16;
/// New host to old: it couldn't be (a Linux errno), and the new host goes.
const REFUSED: u8 = 17;
/// New host to old: the process stopped, continued or ended.
const CHANGED: u8 = 18;
/// New host to old: the process is in this group and session now
/// (`setpgid`, `setsid` there).
const MEMBER: u8 = 19;
/// Either way: what this host reaches, its own and through its other
/// links ([`Reach`]).
const REACH: u8 = 32;
/// Either way: a signal for a process group, for the members over there.
const TO_GROUP: u8 = 33;
/// Either way: a signal for every process (`kill(-1)`).
const TO_ALL: u8 = 34;
/// Either way: a signal for a pid or tid over there, answered.
const ASK: u8 = 35;
/// The answer: the signal's result (or the errno, negated).
const ANSWER: u8 = 36;
/// Either way: make a process group a terminal's foreground (`tcsetpgrp`
/// from another host than the terminal's), answered.
const ASK_FOREGROUND: u8 = 37;
/// Either way: the pids of the processes there and beyond (for `/proc`),
/// answered.
const LIST_PIDS: u8 = 38;
/// Either way: what's known of a process there or beyond (for `/proc`),
/// answered.
const ASK_STATUS: u8 = 39;

/// How long the old host waits for the new one's answer to a hand-off.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);
/// How long a signal to another host's process waits for its answer.
const ASK_WITHIN: Duration = Duration::from_secs(2);

/// What a host reaches through a link: the process groups (with their
/// sessions) that have members there or beyond, the pids (ranges,
/// `[start, end)`) that are processes and threads there or beyond, and
/// the terminals whose job control is there or beyond: each pty's
/// session and foreground group (-1 for none).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reach {
    pub groups: BTreeSet<(i64, i64)>,
    pub pids: BTreeSet<(i64, i64)>,
    pub ttys: BTreeMap<u32, (i64, i64)>,
}

impl Reach {
    fn encode(&self, e: &mut Encoder) {
        for set in [&self.groups, &self.pids] {
            e.u64(set.len() as u64);
            for &(a, b) in set {
                e.i64(a).i64(b);
            }
        }
        e.u64(self.ttys.len() as u64);
        for (&n, &(sid, pgrp)) in &self.ttys {
            e.u64(n as u64).i64(sid).i64(pgrp);
        }
    }

    fn decode(d: &mut Decoder) -> Result<Reach, Truncated> {
        let mut r = Reach::default();
        for set in [&mut r.groups, &mut r.pids] {
            for _ in 0..d.count(16)? {
                set.insert((d.i64()?, d.i64()?));
            }
        }
        for _ in 0..d.count(24)? {
            r.ttys.insert(d.u64()? as u32, (d.i64()?, d.i64()?));
        }
        Ok(r)
    }

    fn has_pid(&self, pid: i64) -> bool {
        self.pids.range(..=(pid, i64::MAX)).next_back().is_some_and(|&(start, end)| start <= pid && pid < end)
    }

    fn extend(&mut self, other: &Reach) {
        self.groups.extend(other.groups.iter().copied());
        self.pids.extend(other.pids.iter().copied());
        for (&n, &t) in &other.ttys {
            self.ttys.entry(n).or_insert(t);
        }
    }
}

/// The other end of a link between two hosts.
#[derive(Debug)]
pub(crate) struct Peer {
    /// The other host's Mac pid.
    pub mac_pid: libc::pid_t,
    sock: Mutex<UnixStream>,
    /// What it said it reaches.
    reach: Mutex<Reach>,
    /// What this host last told it it reaches.
    told: Mutex<Option<Reach>>,
    /// Requests made of it, by id: their answers once they come.
    asks: Mutex<(u64, HashMap<u64, Option<Vec<u8>>>)>,
    answered: Condvar,
}

impl Peer {
    fn new(mac_pid: libc::pid_t, sock: UnixStream) -> Arc<Peer> {
        Arc::new(Peer {
            mac_pid,
            sock: Mutex::new(sock),
            reach: Mutex::default(),
            told: Mutex::new(None),
            asks: Mutex::default(),
            answered: Condvar::new(),
        })
    }

    fn send(&self, kind: u8, body: &[u8]) {
        let sock = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = proto::send(sock.as_raw_fd(), kind, body, &[]);
    }

    /// Signal `pid` there (or its thread `tid`), not waiting to hear.
    pub fn signal(&self, pid: i64, tid: Option<i64>, info: SigInfo) {
        let mut e = Encoder::default();
        e.i64(pid).bool(tid.is_some()).i64(tid.unwrap_or(0));
        encode_info(&mut e, &info);
        self.send(SIGNAL, &e.0);
    }

    /// `to` (a pid or a thread there) gets `info`: how that went.
    fn ask(&self, to: Target, info: SigInfo) -> Result<i64, i64> {
        let mut e = Encoder::default();
        encode_target(&mut e, to);
        encode_info(&mut e, &info);
        result_of(self.request(ASK, &e.0))
    }

    /// Pty `n`'s foreground is group `pgrp` (asked by a process of session
    /// `sid`): how that went.
    pub fn ask_foreground(&self, n: u32, pgrp: i64, sid: i64) -> Result<i64, i64> {
        let mut e = Encoder::default();
        e.u64(n as u64).i64(pgrp).i64(sid);
        result_of(self.request(ASK_FOREGROUND, &e.0))
    }

    /// The pids there and beyond.
    fn list_pids(&self) -> Vec<i64> {
        let Some(body) = self.request(LIST_PIDS, &[]) else { return Vec::new() };
        let mut d = Decoder(&body);
        (|| {
            let mut pids = Vec::new();
            for _ in 0..d.count(8)? {
                pids.push(d.i64()?);
            }
            Ok::<_, Truncated>(pids)
        })()
        .unwrap_or_default()
    }

    /// What's known there of `pid`.
    fn status(&self, pid: i64) -> Result<ProcStatus, i64> {
        let mut e = Encoder::default();
        e.i64(pid);
        let body = self.request(ASK_STATUS, &e.0).ok_or(errno::ESRCH)?;
        decode_status(&mut Decoder(&body)).map_err(|_| errno::ESRCH)?
    }

    /// A request (`kind`, its id then `body`), and its answer (None: none
    /// came in time, or the link went).
    fn request(&self, kind: u8, body: &[u8]) -> Option<Vec<u8>> {
        let id = {
            let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
            asks.0 += 1;
            let id = asks.0;
            asks.1.insert(id, None);
            id
        };
        let mut e = Encoder::default();
        e.u64(id);
        e.0.extend_from_slice(body);
        self.send(kind, &e.0);
        let deadline = std::time::Instant::now() + ASK_WITHIN;
        let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(Some(_)) = asks.1.get(&id) {
                return asks.1.remove(&id).flatten().filter(|b| !b.is_empty());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                asks.1.remove(&id);
                return None;
            }
            asks = self.answered.wait_timeout(asks, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The link has gone: whatever was asked of it is answered with ESRCH.
    fn gone(&self) {
        let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
        for r in asks.1.values_mut() {
            r.get_or_insert(Vec::new());
        }
        self.answered.notify_all();
        *self.reach.lock().unwrap_or_else(|e| e.into_inner()) = Reach::default();
    }

    /// Answer request `id`.
    fn answer(&self, id: u64, body: &[u8]) {
        let mut e = Encoder::default();
        e.u64(id);
        e.0.extend_from_slice(body);
        self.send(ANSWER, &e.0);
    }

    /// `pid`'s parent is `ppid` from now on.
    pub fn reparent(&self, pid: i64, ppid: i64) {
        let mut e = Encoder::default();
        e.i64(pid).i64(ppid);
        self.send(REPARENT, &e.0);
    }

    /// `pid` (here, whose parent is the other host's) changed.
    pub fn changed(&self, pid: i64, change: &Change) {
        let mut e = Encoder::default();
        e.i64(pid);
        match *change {
            Change::Stopped(sig) => e.u64(0).u64(sig),
            Change::Continued => e.u64(1),
            Change::Ended { status, user_ns, sys_ns } => e.u64(2).i64(status as i64).u64(user_ns).u64(sys_ns),
        };
        self.send(CHANGED, &e.0);
    }

    /// `pid` (here, whose parent is the other host's) is in group `pgid`
    /// and session `sid` now.
    pub fn member(&self, pid: i64, pgid: i64, sid: i64) {
        let mut e = Encoder::default();
        e.i64(pid).i64(pgid).i64(sid);
        self.send(MEMBER, &e.0);
    }

    pub fn reaches_group(&self, pgid: i64) -> bool {
        self.reach.lock().unwrap_or_else(|e| e.into_inner()).groups.range((pgid, i64::MIN)..=(pgid, i64::MAX)).next().is_some()
    }

    pub fn reaches_group_in(&self, pgid: i64, sid: i64) -> bool {
        self.reach.lock().unwrap_or_else(|e| e.into_inner()).groups.contains(&(pgid, sid))
    }

    fn reaches_pid(&self, pid: i64) -> bool {
        self.reach.lock().unwrap_or_else(|e| e.into_inner()).has_pid(pid)
    }

    fn tty(&self, n: u32) -> Option<(i64, i64)> {
        self.reach.lock().unwrap_or_else(|e| e.into_inner()).ttys.get(&n).copied()
    }
}

/// A signal's (or `tcsetpgrp`'s) answer: the result, or the errno negated;
/// no answer is ESRCH.
fn result_of(body: Option<Vec<u8>>) -> Result<i64, i64> {
    match body.and_then(|b| Decoder(&b).i64().ok()) {
        Some(r) if r < 0 => Err(-r),
        Some(r) => Ok(r),
        None => Err(errno::ESRCH),
    }
}

fn answer_of(r: Result<i64, i64>) -> Vec<u8> {
    let mut e = Encoder::default();
    e.i64(match r {
        Ok(v) => v,
        Err(e) => -e,
    });
    e.0
}

fn encode_status(e: &mut Encoder, r: &Result<ProcStatus, i64>) {
    match r {
        Err(err) => drop(e.bool(false).i64(*err)),
        Ok(s) => {
            e.bool(true).bytes(&s.comm).i64(s.ppid).i64(s.pgid).i64(s.sid).u64(s.state as u64).u64(s.threads);
            e.u64(s.user_ns).u64(s.sys_ns).u64(s.children_user_ns).u64(s.children_sys_ns).u64(s.start_ns);
            e.bool(s.tty.is_some()).u64(s.tty.unwrap_or(0) as u64).i64(s.tpgid);
            e.opt_bytes(s.cmdline.as_deref()).u64(s.rss_pages);
        }
    }
}

fn decode_status(d: &mut Decoder) -> Result<Result<ProcStatus, i64>, Truncated> {
    if !d.bool()? {
        return Ok(Err(d.i64()?));
    }
    let (comm, ppid, pgid, sid, state, threads) = (d.bytes()?, d.i64()?, d.i64()?, d.i64()?, d.u64()? as u8, d.u64()?);
    let (user_ns, sys_ns, children_user_ns, children_sys_ns, start_ns) = (d.u64()?, d.u64()?, d.u64()?, d.u64()?, d.u64()?);
    let (has_tty, tty, tpgid) = (d.bool()?, d.u64()? as u32, d.i64()?);
    let (cmdline, rss_pages) = (d.opt_bytes()?, d.u64()?);
    Ok(Ok(ProcStatus {
        comm,
        ppid,
        pgid,
        sid,
        state,
        threads,
        user_ns,
        sys_ns,
        children_user_ns,
        children_sys_ns,
        start_ns,
        tty: has_tty.then_some(tty),
        tpgid,
        cmdline,
        rss_pages,
    }))
}

fn encode_info(e: &mut Encoder, info: &SigInfo) {
    e.u64(info.signo).i64(info.code as i64).i64(info.pid as i64).u64(info.uid as u64).i64(info.status as i64);
}

fn decode_info(d: &mut Decoder) -> Result<SigInfo, Truncated> {
    let (signo, code, from, uid, status) = (d.u64()?, d.i64()? as i32, d.i64()?, d.u64()? as u32, d.i64()? as i32);
    Ok(SigInfo { uid, status, ..SigInfo::user(signo, code, from) })
}

fn encode_target(e: &mut Encoder, to: Target) {
    match to {
        Target::Pid(pid) => e.u64(0).i64(pid),
        Target::Thread { tgid, tid } => e.u64(1).bool(tgid.is_some()).i64(tgid.unwrap_or(0)).i64(tid),
        Target::Group(pgid) => e.u64(2).i64(pgid),
        Target::All { except } => e.u64(3).i64(except),
    };
}

fn decode_target(d: &mut Decoder) -> Result<Target, Truncated> {
    Ok(match d.u64()? {
        0 => Target::Pid(d.i64()?),
        1 => {
            let (has, tgid, tid) = (d.bool()?, d.i64()?, d.i64()?);
            Target::Thread { tgid: has.then_some(tgid), tid }
        }
        2 => Target::Group(d.i64()?),
        3 => Target::All { except: d.i64()? },
        _ => return Err(Truncated),
    })
}

fn decode_change(d: &mut Decoder) -> Result<Change, Truncated> {
    Ok(match d.u64()? {
        0 => Change::Stopped(d.u64()?),
        1 => Change::Continued,
        2 => Change::Ended { status: d.i64()? as i32, user_ns: d.u64()?, sys_ns: d.u64()? },
        _ => return Err(Truncated),
    })
}

impl Table {
    /// The hosts this one is linked to.
    fn peers(&self) -> Vec<Arc<Peer>> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a linked host but `except` reaches group `pgid`.
    pub(super) fn reached(&self, pgid: i64, except: Option<&Arc<Peer>>) -> bool {
        self.peers().iter().any(|p| except.is_none_or(|x| !Arc::ptr_eq(p, x)) && p.reaches_group(pgid))
    }

    /// Whether a linked host reaches group `pgid` of session `sid`.
    pub(super) fn reached_in(&self, pgid: i64, sid: i64) -> bool {
        self.peers().iter().any(|p| p.reaches_group_in(pgid, sid))
    }

    /// Whether this host is linked to one but `except`.
    pub(super) fn has_peers(&self, except: Option<&Arc<Peer>>) -> bool {
        self.peers().iter().any(|p| except.is_none_or(|x| !Arc::ptr_eq(p, x)))
    }

    /// A signal for a pid or thread not here: asked of the linked host
    /// (but `except`) that reaches it, or ESRCH.
    pub(super) fn route(&self, to: Target, info: SigInfo, except: Option<&Arc<Peer>>) -> Result<i64, i64> {
        let id = match to {
            Target::Pid(pid) => pid,
            Target::Thread { tid, .. } => tid,
            _ => return Err(errno::ESRCH),
        };
        let peer = self.peers().into_iter().find(|p| except.is_none_or(|x| !Arc::ptr_eq(p, x)) && p.reaches_pid(id));
        match peer {
            Some(peer) => peer.ask(to, info),
            None => Err(errno::ESRCH),
        }
    }

    /// Pty `n`'s job control, if it's another host's: its session and
    /// foreground group (-1: none), as its host last said.
    pub(super) fn remote_tty(&self, n: u32) -> Option<(i64, Option<i64>)> {
        self.peers().iter().find_map(|p| p.tty(n)).map(|(sid, pgrp)| (sid, (pgrp >= 0).then_some(pgrp)))
    }

    /// The pids of every process here and in the linked hosts (but
    /// `except`) and beyond, for `/proc`. Moved processes' stand-ins are
    /// their hosts' to list.
    pub(super) fn federated_pids(&self, except: Option<&Arc<Peer>>) -> BTreeSet<i64> {
        let mut pids: BTreeSet<i64> = {
            let s = self.state();
            s.procs.iter().filter(|(_, e)| !matches!(&e.stand, Some(StandIn { of: Elsewhere::Host(_), .. }))).map(|(&pid, _)| pid).collect()
        };
        for p in self.peers() {
            if except.is_none_or(|x| !Arc::ptr_eq(&p, x)) {
                pids.extend(p.list_pids());
            }
        }
        pids
    }

    /// What's known of `pid`, here (with its command line from its
    /// memory) or in the linked host (but `except`) that reaches it.
    pub(super) fn federated_status(&self, pid: i64, except: Option<&Arc<Peer>>) -> Result<ProcStatus, i64> {
        let here = {
            let s = self.state();
            s.procs.get(&pid).map(|e| matches!(&e.stand, Some(StandIn { of: Elsewhere::Host(_), .. })))
        };
        if here == Some(false) {
            return self.status_of(pid, true);
        }
        let peer = self.peers().into_iter().find(|p| except.is_none_or(|x| !Arc::ptr_eq(p, x)) && p.reaches_pid(pid));
        match peer.map(|p| p.status(pid)) {
            Some(Ok(status)) => Ok(status),
            // A moved process's stand-in, if its host doesn't answer.
            _ if here == Some(true) => self.status_of(pid, true),
            Some(Err(e)) => Err(e),
            None => Err(errno::ESRCH),
        }
    }

    /// `tcsetpgrp` on pty `n`, another host's: asked of it.
    pub(super) fn remote_foreground(&self, n: u32, pgrp: i64, sid: i64) -> Result<i64, i64> {
        match self.peers().into_iter().find(|p| p.tty(n).is_some()) {
            Some(peer) => peer.ask_foreground(n, pgrp, sid),
            None => Err(errno::ENOTTY),
        }
    }

    /// Group `pgid`'s signal, for its members in the linked hosts (but
    /// `except`) that reach it; `keyboard`: pty that raised it.
    pub(super) fn fan_out_group(&self, pgid: i64, info: SigInfo, keyboard: Option<u32>, except: Option<&Arc<Peer>>) {
        for p in self.peers() {
            if except.is_none_or(|x| !Arc::ptr_eq(&p, x)) && p.reaches_group(pgid) {
                let mut e = Encoder::default();
                e.i64(pgid).bool(keyboard.is_some()).u64(keyboard.unwrap_or(0) as u64);
                encode_info(&mut e, &info);
                p.send(TO_GROUP, &e.0);
            }
        }
    }

    /// `kill(-1)`'s signal, for every linked host but `except`.
    pub(super) fn fan_out_all(&self, except_pid: i64, info: SigInfo, except: Option<&Arc<Peer>>) {
        for p in self.peers() {
            if except.is_none_or(|x| !Arc::ptr_eq(&p, x)) {
                let mut e = Encoder::default();
                e.i64(except_pid);
                encode_info(&mut e, &info);
                p.send(TO_ALL, &e.0);
            }
        }
    }

    /// What this host reaches itself: the groups of its processes, and its
    /// pid ranges and the pids it has from elsewhere (its moved first
    /// process). Moved processes' stand-ins don't count: their hosts say.
    fn own_reach(&self) -> Reach {
        let mut r = Reach::default();
        r.pids.extend(self.pids.ranges());
        let s = self.state();
        for (&n, t) in &s.ttys {
            if let Some(sid) = t.sid {
                r.ttys.insert(n, (sid, t.pgrp.unwrap_or(-1)));
            }
        }
        for (&pid, e) in &s.procs {
            if matches!(&e.stand, Some(StandIn { of: Elsewhere::Host(_), .. })) {
                continue;
            }
            r.groups.insert((e.pgid, e.sid));
            if !r.has_pid(pid) {
                r.pids.insert((pid, pid + 1));
            }
        }
        r
    }

    /// Tell each linked host what this one reaches (its own, and through
    /// its other links), if that has changed since it was last told.
    /// (Never with the table held.)
    pub(super) fn advertise(&self) {
        let peers = self.peers();
        if peers.is_empty() {
            return;
        }
        let own = self.own_reach();
        for p in &peers {
            let mut r = own.clone();
            for q in peers.iter().filter(|q| !Arc::ptr_eq(q, p)) {
                r.extend(&q.reach.lock().unwrap_or_else(|e| e.into_inner()));
            }
            let mut told = p.told.lock().unwrap_or_else(|e| e.into_inner());
            if told.as_ref() != Some(&r) {
                let mut e = Encoder::default();
                r.encode(&mut e);
                p.send(REACH, &e.0);
                *told = Some(r);
            }
        }
    }

    fn add_peer(&self, peer: &Arc<Peer>) {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).push(peer.clone());
    }

    fn remove_peer(&self, peer: &Arc<Peer>) {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| !Arc::ptr_eq(p, peer));
        peer.gone();
    }

    /// What either end of a link says that both ends handle: reaches,
    /// signals for groups, everyone, pids, and their answers. True if it
    /// was one of those.
    fn common_message(&self, peer: &Arc<Peer>, msg: &proto::Msg) -> bool {
        let mut d = Decoder(&msg.body);
        match msg.kind {
            REACH => {
                if let Ok(r) = Reach::decode(&mut d) {
                    *peer.reach.lock().unwrap_or_else(|e| e.into_inner()) = r;
                    // What the other links reach through this one changed.
                    self.advertise();
                }
            }
            TO_GROUP => {
                if let Ok((pgid, kb, n, info)) = (|| Ok::<_, Truncated>((d.i64()?, d.bool()?, d.u64()?, decode_info(&mut d)?)))() {
                    let _ = self.deliver_from(Target::Group(pgid), info, kb.then_some(n as u32), Some(peer));
                }
            }
            TO_ALL => {
                if let (Ok(except), Ok(info)) = (d.i64(), decode_info(&mut d)) {
                    let _ = self.deliver_from(Target::All { except }, info, None, Some(peer));
                }
            }
            ASK => {
                let Ok((id, to, info)) = (|| Ok::<_, Truncated>((d.u64()?, decode_target(&mut d)?, decode_info(&mut d)?)))() else { return true };
                let r = self.deliver_from(to, info, None, Some(peer));
                peer.answer(id, &answer_of(r));
            }
            ASK_FOREGROUND => {
                let Ok((id, n, pgrp, sid)) = (|| Ok::<_, Truncated>((d.u64()?, d.u64()? as u32, d.i64()?, d.i64()?)))() else { return true };
                let r = self.set_foreground(n, pgrp, sid, Some(peer));
                peer.answer(id, &answer_of(r));
            }
            LIST_PIDS => {
                let Ok(id) = d.u64() else { return true };
                let pids = self.federated_pids(Some(peer));
                let mut e = Encoder::default();
                e.u64(pids.len() as u64);
                for pid in pids {
                    e.i64(pid);
                }
                peer.answer(id, &e.0);
            }
            ASK_STATUS => {
                let (Ok(id), Ok(pid)) = (d.u64(), d.i64()) else { return true };
                let r = self.federated_status(pid, Some(peer));
                let mut e = Encoder::default();
                encode_status(&mut e, &r);
                peer.answer(id, &e.0);
            }
            ANSWER => {
                if let Ok(id) = d.u64() {
                    let mut asks = peer.asks.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(slot) = asks.1.get_mut(&id) {
                        *slot = Some(d.0.to_vec());
                    }
                    peer.answered.notify_all();
                }
            }
            _ => return false,
        }
        true
    }
}

/// Whether `exec` names a program set to have a host of its own: by
/// `MACWL_NEW_HOST=1` in the environment it's given, or by its path or
/// name in `~/.config/macwl/hosts` (a line each; `#` starts a comment).
fn wants_own_host(exec: &Exec) -> bool {
    if exec.envp.iter().any(|e| e == b"MACWL_NEW_HOST=1") {
        return true;
    }
    let Some(home) = std::env::var_os("HOME") else { return false };
    let Ok(text) = std::fs::read(std::path::Path::new(&home).join(".config/macwl/hosts")) else { return false };
    text.split(|&b| b == b'\n').map(|l| l.split(|&b| b == b'#').next().unwrap_or_default().trim_ascii()).any(|l| {
        !l.is_empty() && if l.contains(&b'/') { l == exec.exe.as_slice() } else { l == exec.comm.as_slice() }
    })
}

impl Table {
    /// Whether `proc`'s `execve` of `exec` moves it to a host of its own:
    /// a program set to have one, unless it's alone in this host already.
    pub(crate) fn moves(&self, exec: &Exec) -> bool {
        self.live.load(std::sync::atomic::Ordering::Acquire) > 1 && wants_own_host(exec)
    }

    /// Move `proc` (whose thread `thread` is exec'ing `exec`) to a new host
    /// of its own: once the new host has its program loaded, `proc` stands
    /// in for it here (its tasks end, as for a Mac program's `execve`).
    /// The errno for `execve` if it can't.
    pub(crate) fn move_to_host(self: &Arc<Self>, proc: &Proc, thread: &Thread, exec: &Exec) -> Result<(), i64> {
        let pid = proc.abi.pid;
        let (ppid, pgid, sid, ctty) = self.state().procs.get(&pid).map(|e| (e.ppid, e.pgid, e.sid, e.ctty)).ok_or(errno::ESRCH)?;
        // It doesn't move again from its new host.
        let mut exec = exec.clone();
        exec.envp.retain(|e| !e.starts_with(b"MACWL_NEW_HOST="));
        let (sock, mac_pid) = spawn_host().map_err(|e| errno::from_io(&e))?;
        let mut moved = Encoder::default();
        let files = proc.abi.encode_for_move(thread, &exec, &mut moved);
        let mut e = Encoder::default();
        e.i64(pid).i64(ppid).i64(pgid).i64(sid).bool(ctty.is_some()).u64(ctty.unwrap_or(0) as u64).u64(files.len() as u64);
        e.0.extend_from_slice(&moved.0);
        let fds: Vec<RawFd> = files.iter().map(|(_, f)| f.host.as_raw_fd()).collect();
        let mut batches = fds.chunks(proto::MAX_FDS);
        let sent = proto::send(sock.as_raw_fd(), ADOPT, &e.0, batches.next().unwrap_or(&[]))
            .and_then(|()| batches.try_for_each(|b| proto::send(sock.as_raw_fd(), FDS, &[], b)));
        let gone = |sock: UnixStream| {
            drop(sock);
            reap(mac_pid);
        };
        if sent.is_err() {
            gone(sock);
            return Err(errno::EAGAIN);
        }
        let _ = sock.set_read_timeout(Some(ANSWER_WITHIN));
        let mut reader = proto::Reader::default();
        match reader.recv(sock.as_raw_fd()) {
            Some(msg) if msg.kind == ADOPTED => {}
            Some(msg) if msg.kind == REFUSED => {
                gone(sock);
                return Err(Decoder(&msg.body).i64().unwrap_or(errno::ENOEXEC));
            }
            _ => {
                gone(sock);
                return Err(errno::EAGAIN);
            }
        }
        let _ = sock.set_read_timeout(None);
        let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
        let Ok(send) = sock.try_clone() else {
            gone(sock);
            return Err(errno::EAGAIN);
        };
        let peer = Peer::new(mac_pid, send);
        // Terminals' masters it passed stay held here as well: their last
        // reference going here would hang them up.
        let held: Vec<_> = files.into_iter().map(|(_, f)| f).filter(|f| f.tty().is_some_and(|t| t.master)).collect();
        let cmdline: Vec<u8> = exec.argv.iter().flat_map(|a| a.iter().copied().chain([0])).collect();
        {
            let mut s = self.state();
            if let Some(e) = s.procs.get_mut(&pid) {
                // It has exec'd: setpgid on it is EACCES from now on.
                e.execd = true;
                e.stand = Some(StandIn { of: Elsewhere::Host(peer.clone()), stopped: false, cmdline, comm: exec.comm.clone(), pending: Vec::new(), _held: held });
            }
        }
        self.add_peer(&peer);
        let weak = Arc::downgrade(self);
        let spawned = linux_abi::host_threads::spawn("host link", move || from_child_host(weak, pid, peer, sock, reader));
        if spawned.is_err() {
            eprintln!("macwl-run: pid {pid}: no thread for its host's link");
        }
        self.advertise();
        Ok(())
    }

    /// Process `pid` is in group `pgid` and session `sid` now: the host of
    /// its parent hears of it if that's another (`remote`), and linked
    /// hosts of what this one reaches.
    pub(super) fn group_changed(&self, pid: i64, remote: bool, pgid: i64, sid: i64) {
        if remote && let Some(peer) = self.parent_peer.get() {
            peer.member(pid, pgid, sid);
        }
        self.advertise();
    }

    /// The host's first process, `pid`, moved here from the host at the
    /// other end of `peer` (`link` its socket, to read), where its parent
    /// is: that host hears of the process, and signals it.
    pub(crate) fn adopted(self: &Arc<Self>, pid: i64, peer: Arc<Peer>, link: UnixStream) {
        if let Some(e) = self.state().procs.get_mut(&pid) {
            e.remote_parent = true;
            e.execd = true;
        }
        let _ = self.parent_peer.set(peer.clone());
        self.add_peer(&peer);
        let weak = Arc::downgrade(self);
        if linux_abi::host_threads::spawn("parent host link", move || from_parent_host(weak, peer, link)).is_err() {
            eprintln!("macwl-run: no thread for the link to the parent host");
        }
        self.advertise();
    }

    /// Tell the host of `pid`'s parent, if that's another host, of its
    /// change. True if it was told (the parent isn't here).
    pub(super) fn tell_remote_parent(&self, pid: i64, change: impl FnOnce() -> Change) -> bool {
        let remote = self.state().procs.get(&pid).is_some_and(|e| e.remote_parent);
        if !remote {
            return false;
        }
        if let Some(peer) = self.parent_peer.get() {
            peer.changed(pid, &change());
        }
        true
    }
}

/// Reap host `mac_pid`, which has gone or is going (a thread waits for it,
/// so no caller does).
fn reap(mac_pid: libc::pid_t) {
    let _ = linux_abi::host_threads::spawn("host reaper", move || {
        let mut status = 0;
        while unsafe { libc::waitpid(mac_pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
    });
}

/// The old host's side of a link: the moved process `pid`'s changes, as
/// they come, until the new host goes (the process ends with it, as killed
/// by SIGKILL, if it hadn't).
fn from_child_host(table: Weak<Table>, pid: i64, peer: Arc<Peer>, sock: UnixStream, mut reader: proto::Reader) {
    loop {
        let Some(msg) = reader.recv(sock.as_raw_fd()) else { break };
        let Some(table) = table.upgrade() else { break };
        if table.common_message(&peer, &msg) {
            continue;
        }
        let mut d = Decoder(&msg.body);
        match msg.kind {
            CHANGED => {
                if let (Ok(p), Ok(change)) = (d.i64(), decode_change(&mut d)) {
                    table.stand_in_changed(p, change);
                }
            }
            MEMBER => {
                if let (Ok(p), Ok(pgid), Ok(sid)) = (d.i64(), d.i64(), d.i64()) {
                    if let Some(e) = table.state().procs.get_mut(&p) {
                        e.pgid = pgid;
                        e.sid = sid;
                    }
                    table.advertise();
                }
            }
            _ => {}
        }
    }
    if let Some(table) = table.upgrade() {
        table.remove_peer(&peer);
        let ours = {
            let s = table.state();
            s.procs.get(&pid).and_then(|e| e.stand.as_ref()).is_some_and(|m| matches!(&m.of, Elsewhere::Host(p) if Arc::ptr_eq(p, &peer)))
        };
        if ours {
            table.stand_in_changed(pid, Change::Ended { status: 9, user_ns: 0, sys_ns: 0 });
        }
        table.advertise();
    } else {
        peer.gone();
    }
    let mut status = 0;
    while unsafe { libc::waitpid(peer.mac_pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
}

/// The new host's side of a link: signals and reparentings for its
/// processes from the old host, and what both ends say, until it goes;
/// then the processes whose parent was there are init's.
fn from_parent_host(table: Weak<Table>, peer: Arc<Peer>, sock: UnixStream) {
    let mut reader = proto::Reader::default();
    while let Some(msg) = reader.recv(sock.as_raw_fd()) {
        let Some(table) = table.upgrade() else { return };
        if table.common_message(&peer, &msg) {
            continue;
        }
        let mut d = Decoder(&msg.body);
        match msg.kind {
            SIGNAL => {
                let Ok((pid, has_tid, tid, info)) = (|| Ok::<_, Truncated>((d.i64()?, d.bool()?, d.i64()?, decode_info(&mut d)?)))() else { continue };
                let to = if has_tid { Target::Thread { tgid: Some(pid), tid } } else { Target::Pid(pid) };
                let _ = table.deliver_from(to, info, None, Some(&peer));
            }
            REPARENT => {
                if let (Ok(pid), Ok(ppid)) = (d.i64(), d.i64())
                    && let Some(e) = table.state().procs.get_mut(&pid)
                {
                    e.ppid = ppid;
                }
            }
            _ => {}
        }
    }
    // The old host has gone: its processes can't wait for these any more.
    if let Some(table) = table.upgrade() {
        table.remove_peer(&peer);
        {
            let mut s = table.state();
            for e in s.procs.values_mut().filter(|e| e.remote_parent) {
                e.remote_parent = false;
                e.ppid = 1;
            }
        }
        table.advertise();
    } else {
        peer.gone();
    }
}

/// Start `macwl-run --adopted`: a new host, a new Mac process in a Mac
/// session of its own, its signals as a new program's, with a socket to
/// this host as its fd 3 and this host's stderr as its own. The socket's
/// end here, and its Mac pid.
fn spawn_host() -> std::io::Result<(UnixStream, libc::pid_t)> {
    let (mine, theirs) = UnixStream::pair()?;
    for fd in [mine.as_raw_fd(), theirs.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let exe = crate::mac::macwl_run_exe()?;
    let cstr = |b: &[u8]| std::ffi::CString::new(b).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let args = [cstr(exe.as_os_str().as_encoded_bytes())?, cstr(b"--adopted")?];
    // Its environment is this host's (MACWL_* settings, HOME): the guest's
    // goes in the hand-off.
    let env: Vec<std::ffi::CString> = std::env::vars_os().filter_map(|(k, v)| cstr(&[k.as_encoded_bytes(), b"=", v.as_encoded_bytes()].concat()).ok()).collect();
    let flags = crate::mac::POSIX_SPAWN_SETSID
        | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short
        | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
        | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
    let pid = crate::mac::spawn(&args[0], &args, &env, flags, |actions, attr| unsafe {
        let mut all: libc::sigset_t = 0;
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(attr, &all);
        let mut none: libc::sigset_t = 0;
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(attr, &none);
        libc::posix_spawn_file_actions_addopen(actions, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
        libc::posix_spawn_file_actions_addopen(actions, 1, c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
        libc::posix_spawn_file_actions_adddup2(actions, 2, 2);
        libc::posix_spawn_file_actions_adddup2(actions, theirs.as_raw_fd(), 3);
    })
    .map_err(std::io::Error::from_raw_os_error)?;
    Ok((mine, pid))
}

/// `macwl-run --adopted`: a host for a process moving here from the host
/// at the other end of fd 3, as it execs (see the module's docs).
pub fn run_adopted() -> Result<Exit, Error> {
    let sock = unsafe { UnixStream::from_raw_fd(3) };
    unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut reader = proto::Reader::default();
    let refuse = |sock: &UnixStream, err: i64| {
        let mut e = Encoder::default();
        e.i64(err);
        let _ = proto::send(sock.as_raw_fd(), REFUSED, &e.0, &[]);
    };
    let Some(msg) = reader.recv(sock.as_raw_fd()).filter(|m| m.kind == ADOPT) else {
        return Err(Error::Unexpected("no process to adopt".into()));
    };
    let mut d = Decoder(&msg.body);
    let header = (|| Ok::<_, Truncated>((d.i64()?, d.i64()?, d.i64()?, d.i64()?, d.bool()?, d.u64()?, d.count(4)?)))();
    let Ok((pid, ppid, pgid, sid, has_ctty, ctty, wanted)) = header else {
        refuse(&sock, errno::EINVAL);
        return Err(Error::Unexpected("a bad hand-off".into()));
    };
    // Its fds: those that came with the message, then the rest.
    let mut fds: Vec<OwnedFd> = msg.fds;
    while fds.len() < wanted {
        match reader.recv(sock.as_raw_fd()) {
            Some(m) if m.kind == FDS => fds.extend(m.fds),
            _ => {
                refuse(&sock, errno::EINVAL);
                return Err(Error::Unexpected("a hand-off's fds didn't come".into()));
            }
        }
    }
    let Ok((process, thread, exe, argv, envp)) = Process::decode_moved(&mut d, fds) else {
        refuse(&sock, errno::EINVAL);
        return Err(Error::Unexpected("a bad hand-off".into()));
    };
    let elf = match crate::launch::read_program(&process, std::ffi::OsStr::from_bytes(&exe)) {
        Ok((elf, _)) => elf,
        Err(why) => {
            refuse(&sock, errno::ENOENT);
            return Err(Error::Unexpected(why));
        }
    };
    let pids = RangePids::new()?;
    let (Ok(send), Ok(answer)) = (sock.try_clone(), sock.try_clone()) else {
        refuse(&sock, errno::EAGAIN);
        return Err(Error::Unexpected("no socket".into()));
    };
    let peer = Peer::new(unsafe { libc::getppid() }, send);
    let loaded: Box<dyn FnOnce(Result<(), i64>)> = Box::new(move |r| match r {
        Ok(()) => drop(proto::send(answer.as_raw_fd(), ADOPTED, &[], &[])),
        Err(e) => {
            let mut b = Encoder::default();
            b.i64(e);
            let _ = proto::send(answer.as_raw_fd(), REFUSED, &b.0, &[]);
        }
    });
    let first = First::Adopted { ppid, pgid, sid, ctty: has_ctty.then_some(ctty as u32), peer, link: sock, loaded };
    let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let envp: Vec<&[u8]> = envp.iter().map(Vec::as_slice).collect();
    match super::run_first(Box::new(pids), pid, &elf, &argv, &envp, process, thread, first) {
        // Refused, and its execve said why: nothing more to say.
        Err(Error::Load(_)) => Ok(Exit::Code(127)),
        r => r,
    }
}
