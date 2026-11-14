//! Terminal ioctls: `struct termios` translated between Linux's layout and
//! flag values and Darwin's, which differ in almost every bit.

use super::ok;
use crate::{
    errno,
    memory::{GuestMemory, GuestMemoryExt},
};

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
/// `tcdrain` (arg non-zero) or `tcsendbreak` (arg 0).
pub const TCSBRK: u64 = 0x5409;
/// `tcflow`.
pub const TCXONC: u64 = 0x540a;
/// `tcflush`.
pub const TCFLSH: u64 = 0x540b;
pub const TCGETS2: u64 = 0x802c_542a;
pub const TCSETS2: u64 = 0x402c_542b;
pub const TCSETSW2: u64 = 0x402c_542c;
pub const TCSETSF2: u64 = 0x402c_542d;

const NCCS: usize = 19;

/// Linux's `struct termios2` (`struct termios` is the first 36 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Termios2 {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; NCCS],
    ispeed: u32,
    ospeed: u32,
}
const TERMIOS_SIZE: usize = 36;

/// (Linux bit, Darwin bit) pairs, for flags that exist on both.
const IFLAG: &[(u32, libc::tcflag_t)] = &[
    (0o1, libc::IGNBRK),
    (0o2, libc::BRKINT),
    (0o4, libc::IGNPAR),
    (0o10, libc::PARMRK),
    (0o20, libc::INPCK),
    (0o40, libc::ISTRIP),
    (0o100, libc::INLCR),
    (0o200, libc::IGNCR),
    (0o400, libc::ICRNL),
    (0o2000, libc::IXON),
    (0o4000, libc::IXANY),
    (0o10000, libc::IXOFF),
    (0o20000, libc::IMAXBEL),
    (0o40000, libc::IUTF8),
];
const OFLAG: &[(u32, libc::tcflag_t)] = &[
    (0o1, libc::OPOST),
    (0o4, libc::ONLCR),
    (0o10, libc::OCRNL),
    (0o20, libc::ONOCR),
    (0o40, libc::ONLRET),
    (0o100, libc::OFILL),
    (0o200, libc::OFDEL),
    (0o14000, libc::OXTABS), // XTABS (TAB3)
];
const CFLAG: &[(u32, libc::tcflag_t)] = &[
    (0o100, libc::CSTOPB),
    (0o200, libc::CREAD),
    (0o400, libc::PARENB),
    (0o1000, libc::PARODD),
    (0o2000, libc::HUPCL),
    (0o4000, libc::CLOCAL),
    (0o20000000000, libc::CRTSCTS),
];
const LFLAG: &[(u32, libc::tcflag_t)] = &[
    (0o1, libc::ISIG),
    (0o2, libc::ICANON),
    (0o10, libc::ECHO),
    (0o20, libc::ECHOE),
    (0o40, libc::ECHOK),
    (0o100, libc::ECHONL),
    (0o200, libc::NOFLSH),
    (0o400, libc::TOSTOP),
    (0o1000, libc::ECHOCTL),
    (0o2000, libc::ECHOPRT),
    (0o4000, libc::ECHOKE),
    (0o10000, libc::FLUSHO),
    (0o40000, libc::PENDIN),
    (0o100000, libc::IEXTEN),
    (0o200000, libc::EXTPROC),
];
/// Linux `c_cc` index → Darwin's.
const CC: &[(usize, usize)] = &[
    (0, libc::VINTR),
    (1, libc::VQUIT),
    (2, libc::VERASE),
    (3, libc::VKILL),
    (4, libc::VEOF),
    (5, libc::VTIME),
    (6, libc::VMIN),
    (8, libc::VSTART),
    (9, libc::VSTOP),
    (10, libc::VSUSP),
    (11, libc::VEOL),
    (12, libc::VREPRINT),
    (13, libc::VDISCARD),
    (14, libc::VWERASE),
    (15, libc::VLNEXT),
    (16, libc::VEOL2),
];
const CSIZE: u32 = 0o60;
const CBAUD: u32 = 0o10017;
const BOTHER: u32 = 0o10000;
/// Linux's `Bnnn` codes in CBAUD, by speed.
const SPEEDS: &[(u32, u32)] = &[
    (0, 0),
    (0o1, 50),
    (0o2, 75),
    (0o3, 110),
    (0o4, 134),
    (0o5, 150),
    (0o6, 200),
    (0o7, 300),
    (0o10, 600),
    (0o11, 1200),
    (0o12, 1800),
    (0o13, 2400),
    (0o14, 4800),
    (0o15, 9600),
    (0o16, 19200),
    (0o17, 38400),
    (0o10001, 57600),
    (0o10002, 115200),
    (0o10003, 230400),
];

fn to_darwin(linux: u32, table: &[(u32, libc::tcflag_t)]) -> libc::tcflag_t {
    table.iter().filter(|&&(l, _)| linux & l == l).fold(0, |acc, &(_, d)| acc | d)
}

fn to_linux(darwin: libc::tcflag_t, table: &[(u32, libc::tcflag_t)]) -> u32 {
    table.iter().filter(|&&(_, d)| darwin & d == d).fold(0, |acc, &(l, _)| acc | l)
}

fn from_darwin(t: &libc::termios) -> Termios2 {
    let speed = t.c_ospeed as u32;
    let code = SPEEDS.iter().find(|&&(_, s)| s == speed).map_or(BOTHER, |&(c, _)| c);
    let csize = ((t.c_cflag & libc::CSIZE) >> 8) as u32 * 0o20;
    let mut cc = [0u8; NCCS];
    for &(l, d) in CC {
        cc[l] = t.c_cc[d];
    }
    Termios2 {
        iflag: to_linux(t.c_iflag, IFLAG),
        oflag: to_linux(t.c_oflag, OFLAG),
        cflag: to_linux(t.c_cflag, CFLAG) | csize | code,
        lflag: to_linux(t.c_lflag, LFLAG),
        line: 0,
        cc,
        ispeed: t.c_ispeed as u32,
        ospeed: speed,
    }
}

/// Apply a Linux termios onto `t` (the current Darwin settings, so bits
/// Linux can't express are kept).
fn apply(l: &Termios2, t: &mut libc::termios, termios2: bool) {
    t.c_iflag = to_darwin(l.iflag, IFLAG);
    t.c_oflag = to_darwin(l.oflag, OFLAG);
    t.c_cflag = to_darwin(l.cflag, CFLAG) | (((l.cflag & CSIZE) / 0o20) as libc::tcflag_t) << 8;
    t.c_lflag = to_darwin(l.lflag, LFLAG);
    for &(li, d) in CC {
        t.c_cc[d] = l.cc[li];
    }
    let code = l.cflag & CBAUD;
    let speed = if code == BOTHER && termios2 {
        Some(l.ospeed)
    } else {
        SPEEDS.iter().find(|&&(c, _)| c == code).map(|&(_, s)| s)
    };
    if let Some(speed) = speed {
        t.c_ispeed = speed as libc::speed_t;
        t.c_ospeed = speed as libc::speed_t;
    }
}

fn get(host: i32) -> Result<libc::termios, i64> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::tcgetattr(host, &mut t) } as i64)?;
    Ok(t)
}

/// Pseudo-terminal and controlling-terminal ioctls.
pub const TIOCSCTTY: u64 = 0x540e;
pub const TIOCNOTTY: u64 = 0x5422;
pub const TIOCGSID: u64 = 0x5429;
pub const TIOCGPTN: u64 = 0x8004_5430;
pub const TIOCSPTLCK: u64 = 0x4004_5431;
pub const TIOCGPTPEER: u64 = 0x5441;

/// Where a pty's size waits until its slave is opened (from any guest
/// process: the slave is usually opened by a forked child).
fn parked_size_file(p: &super::Process, pty: u32) -> Option<std::path::PathBuf> {
    p.fs.private_dir("pty").ok().map(|d| d.join(pty.to_string()))
}

/// `TIOCSWINSZ` on master `host` before its slave is open: remember `ws`.
/// False if `host` isn't a pty master.
pub(super) fn park_winsize(p: &super::Process, host: i32, ws: &libc::winsize) -> bool {
    let Some(file) = pty_number(host).ok().and_then(|n| parked_size_file(p, n)) else { return false };
    let bytes: Vec<u8> = [ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel].iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(file, bytes).is_ok()
}

/// The size parked for master `host`'s pty, if any.
pub(super) fn pending_winsize(p: &super::Process, host: i32) -> Option<libc::winsize> {
    let b = std::fs::read(parked_size_file(p, pty_number(host).ok()?)?).ok()?;
    let v: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    (v.len() == 4).then(|| libc::winsize { ws_row: v[0], ws_col: v[1], ws_xpixel: v[2], ws_ypixel: v[3] })
}

/// A pty slave (`/dev/ttysNNN`) was just opened as `fd`: apply the size its
/// master was given before.
pub(super) fn slave_opened(p: &super::Process, host_path: &[u8], fd: i32) {
    let Some(n) = host_path.strip_prefix(b"/dev/ttys").and_then(|d| std::str::from_utf8(d).ok()?.parse::<u32>().ok()) else { return };
    let Some(file) = parked_size_file(p, n) else { return };
    if let Ok(b) = std::fs::read(&file) {
        let v: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        if v.len() == 4 {
            let ws = libc::winsize { ws_row: v[0], ws_col: v[1], ws_xpixel: v[2], ws_ypixel: v[3] };
            unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
        }
        let _ = std::fs::remove_file(file);
    }
}

/// The pty number of master `host` (macOS names the slave /dev/ttysNNN).
fn pty_number(host: i32) -> Result<u32, i64> {
    let mut name = [0 as libc::c_char; 128];
    ok(unsafe { libc::ioctl(host, libc::TIOCPTYGNAME as u64, name.as_mut_ptr()) } as i64)?;
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes();
    let digits = name.strip_prefix(b"/dev/ttys").ok_or(errno::ENOTTY)?;
    std::str::from_utf8(digits).ok().and_then(|d| d.parse().ok()).ok_or(errno::ENOTTY)
}

/// The pty ioctls, on master `host`. `open_peer` opens the slave for
/// TIOCGPTPEER with Darwin open flags, returning the new guest fd.
pub(super) fn pty_ioctl<M: GuestMemory + ?Sized>(mem: &M, host: i32, req: u64, arg: u64, open_peer: impl FnOnce(i32) -> Result<i64, i64>) -> Result<i64, i64> {
    match req {
        TIOCGPTN => {
            mem.write(arg, &pty_number(host)?.to_le_bytes())?;
            Ok(0)
        }
        TIOCSPTLCK => {
            let lock: i32 = mem.read_pod(arg)?;
            if lock == 0 {
                ok(unsafe { libc::grantpt(host) } as i64)?;
                ok(unsafe { libc::unlockpt(host) } as i64)?;
            }
            Ok(0)
        }
        TIOCGPTPEER => {
            ok(unsafe { libc::grantpt(host) } as i64)?;
            ok(unsafe { libc::unlockpt(host) } as i64)?;
            open_peer(crate::abi::open_flags_to_darwin(arg))
        }
        _ => Err(errno::ENOTTY),
    }
}

/// Which end of which pty an open file is: macOS names pty `n`'s slave
/// `/dev/ttysNNN`, and Linux `/dev/pts/n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TtyId {
    pub n: u32,
    pub master: bool,
}

/// The pty `host` is an end of, if it is one.
pub fn identify(host: i32) -> Option<TtyId> {
    if unsafe { libc::isatty(host) } == 0 {
        return None;
    }
    // Only a master answers TIOCPTYGNAME.
    if let Ok(n) = pty_number(host) {
        return Some(TtyId { n, master: true });
    }
    let mut name = [0 as libc::c_char; 128];
    if unsafe { libc::ttyname_r(host, name.as_mut_ptr(), name.len()) } != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes();
    let n = std::str::from_utf8(name.strip_prefix(b"/dev/ttys")?).ok()?.parse().ok()?;
    Some(TtyId { n, master: false })
}

/// What a process in the background of its controlling terminal does to
/// it, for the job-control check (Linux's `__tty_check_change`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyOp {
    Read,
    Write,
    /// Changing its settings: `tcsetattr`, `tcsetpgrp`, `tcflush`, ...
    Change,
}

pub const SIGINT: u64 = 2;
pub const SIGQUIT: u64 = 3;
pub const SIGTSTP: u64 = 20;
pub const SIGTTIN: u64 = 21;
pub const SIGTTOU: u64 = 22;

/// Linux's rule for a process outside its terminal's foreground group doing
/// `op`: Ok(None) to go ahead, Ok(Some(sig)) to send `sig` to its process
/// group and have the call made again once that's been dealt with (Linux's
/// ERESTARTSYS: a stop, then the call again when continued), or an error.
/// `tostop`: the terminal's TOSTOP (a write is only checked with it);
/// `ignored`: the caller blocks or ignores the signal; `orphaned`: its
/// process group is orphaned (no one in the session outside it to continue
/// it).
pub fn background(op: TtyOp, tostop: bool, ignored: bool, orphaned: bool) -> Result<Option<u64>, i64> {
    let sig = match op {
        TtyOp::Read => SIGTTIN,
        TtyOp::Write if !tostop => return Ok(None),
        TtyOp::Write | TtyOp::Change => SIGTTOU,
    };
    if ignored {
        // A background read can't wait for a stop that won't come; a write
        // or change just goes ahead.
        return if sig == SIGTTIN { Err(errno::EIO) } else { Ok(None) };
    }
    if orphaned {
        return Err(errno::EIO);
    }
    Ok(Some(sig))
}

/// Whether the terminal open as `host` has TOSTOP set.
pub fn tostop(host: i32) -> bool {
    get(host).is_ok_and(|t| t.c_lflag & libc::TOSTOP != 0)
}

/// What the line discipline of a terminal does with bytes written to its
/// master: its signal characters, from its termios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Keyboard {
    pub isig: bool,
    pub icanon: bool,
    pub iexten: bool,
    pub istrip: bool,
    pub intr: u8,
    pub quit: u8,
    pub susp: u8,
    pub lnext: u8,
}

impl Keyboard {
    /// The rules of the terminal open as `host` (either end), as its termios
    /// says now (whoever set it last).
    pub fn of(host: i32) -> Result<Keyboard, i64> {
        let t = get(host)?;
        Ok(Keyboard {
            isig: t.c_lflag & libc::ISIG != 0,
            icanon: t.c_lflag & libc::ICANON != 0,
            iexten: t.c_lflag & libc::IEXTEN != 0,
            istrip: t.c_iflag & libc::ISTRIP != 0,
            intr: t.c_cc[libc::VINTR],
            quit: t.c_cc[libc::VQUIT],
            susp: t.c_cc[libc::VSUSP],
            lnext: t.c_cc[libc::VLNEXT],
        })
    }

    /// The signals `data` raises, as Linux's `n_tty` reads it: each with the
    /// index of the byte that raises it. `quoted`: a VLNEXT ended what was
    /// written before, so the first byte is taken literally. Also returns
    /// whether a VLNEXT at the end quotes what comes next.
    pub fn signals(&self, data: &[u8], mut quoted: bool) -> (Vec<(usize, u64)>, bool) {
        // Linux disables a character with 0; Darwin's termios says 0xff
        // (_POSIX_VDISABLE) for the ones never set.
        let is = |c: u8, cc: u8| cc != 0 && cc != 0xff && c == cc;
        let mut out = Vec::new();
        for (i, &b) in data.iter().enumerate() {
            if quoted {
                quoted = false;
                continue;
            }
            let c = if self.istrip { b & 0x7f } else { b };
            if self.isig {
                let sig = if is(c, self.intr) {
                    Some(SIGINT)
                } else if is(c, self.quit) {
                    Some(SIGQUIT)
                } else if is(c, self.susp) {
                    Some(SIGTSTP)
                } else {
                    None
                };
                if let Some(sig) = sig {
                    out.push((i, sig));
                    continue;
                }
            }
            // Literal-next quotes the next byte only in canonical mode on
            // Linux (n_tty handles it with the line editing characters).
            if self.icanon && self.iexten && is(c, self.lnext) {
                quoted = true;
            }
        }
        (out, quoted)
    }
}

/// Write `data` to pty master `host` as typed keys: up to and including
/// each signal character, then `signal` for it, as the line discipline
/// raises it on reading that byte. Returns what was written (or the error,
/// if nothing was) and whether a VLNEXT now quotes the next byte.
pub fn write_keys(host: i32, data: &[u8], keys: &Keyboard, quoted: bool, mut signal: impl FnMut(u64)) -> (Result<i64, i64>, bool) {
    let (sigs, _) = keys.signals(data, quoted);
    let mut done = 0usize;
    let ends = sigs.iter().map(|&(i, sig)| (i + 1, Some(sig))).chain(std::iter::once((data.len(), None)));
    for (end, sig) in ends {
        while done < end {
            let want = end - done;
            let n = unsafe { libc::write(host, data[done..end].as_ptr().cast(), want) };
            if n < 0 {
                let e = super::last_errno();
                if e == errno::EINTR && done > 0 {
                    continue;
                }
                let quoted = keys.signals(&data[..done], quoted).1;
                return (if done == 0 { Err(e) } else { Ok(done as i64) }, quoted);
            }
            done += n as usize;
            if (n as usize) < want {
                // A short write: the pty is full for now.
                let quoted = keys.signals(&data[..done], quoted).1;
                return (Ok(done as i64), quoted);
            }
        }
        if let Some(sig) = sig {
            signal(sig);
        }
    }
    (Ok(done as i64), keys.signals(data, quoted).1)
}

/// A terminal's size, as `TIOCGWINSZ` says (None: not a terminal, or a
/// master whose slave isn't open).
pub fn winsize(host: i32) -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(host, libc::TIOCGWINSZ, &mut ws) } == 0).then_some(ws)
}

pub(super) fn ioctl<M: GuestMemory + ?Sized>(mem: &M, host: i32, req: u64, arg: u64) -> Result<i64, i64> {
    match req {
        TCGETS | TCGETS2 => {
            let l = from_darwin(&get(host)?);
            let size = if req == TCGETS { TERMIOS_SIZE } else { std::mem::size_of::<Termios2>() };
            let bytes = unsafe { std::slice::from_raw_parts((&l as *const Termios2).cast::<u8>(), size) };
            mem.write(arg, bytes)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let termios2 = req > 0xffff;
            let mut l = Termios2::default();
            let size = if termios2 { std::mem::size_of::<Termios2>() } else { TERMIOS_SIZE };
            let bytes = unsafe { std::slice::from_raw_parts_mut((&mut l as *mut Termios2).cast::<u8>(), size) };
            mem.read(arg, bytes)?;
            let mut t = get(host)?;
            apply(&l, &mut t, termios2);
            let when = match req {
                TCSETS | TCSETS2 => libc::TCSANOW,
                TCSETSW | TCSETSW2 => libc::TCSADRAIN,
                _ => libc::TCSAFLUSH,
            };
            ok(unsafe { libc::tcsetattr(host, when, &t) } as i64).map(|_| 0)
        }
        TCSBRK if arg != 0 => ok(unsafe { libc::tcdrain(host) } as i64).map(|_| 0),
        TCSBRK => ok(unsafe { libc::tcsendbreak(host, 0) } as i64).map(|_| 0),
        TCXONC => {
            // Linux's TCOOFF, TCOON, TCIOFF, TCION are 0 to 3.
            let action = match arg {
                0 => libc::TCOOFF,
                1 => libc::TCOON,
                2 => libc::TCIOFF,
                3 => libc::TCION,
                _ => return Err(errno::EINVAL),
            };
            ok(unsafe { libc::tcflow(host, action) } as i64).map(|_| 0)
        }
        TCFLSH => {
            // Linux's TCIFLUSH, TCOFLUSH, TCIOFLUSH are 0, 1, 2.
            let queue = match arg {
                0 => libc::TCIFLUSH,
                1 => libc::TCOFLUSH,
                2 => libc::TCIOFLUSH,
                _ => return Err(errno::EINVAL),
            };
            ok(unsafe { libc::tcflush(host, queue) } as i64).map(|_| 0)
        }
        _ => Err(errno::ENOTTY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cooked terminal's keys, as `stty sane` sets them.
    fn sane() -> Keyboard {
        Keyboard { isig: true, icanon: true, iexten: true, istrip: false, intr: 3, quit: 0x1c, susp: 0x1a, lnext: 0x16 }
    }

    #[test]
    fn keyboard_signals_follow_the_termios() {
        let k = sane();
        assert_eq!(k.signals(b"ls\n", false), (vec![], false));
        assert_eq!(k.signals(b"ab\x03cd\x1a\x1c", false), (vec![(2, SIGINT), (5, SIGTSTP), (6, SIGQUIT)], false));
        // ISIG off (a full-screen program's raw mode): bytes, not signals.
        assert_eq!(Keyboard { isig: false, ..k }.signals(b"\x03\x1a", false).0, vec![]);
        // Other characters chosen (stty intr ^X), or turned off: 0 is
        // Linux's _POSIX_VDISABLE, 0xff Darwin's.
        let other = Keyboard { intr: 0x18, susp: 0, quit: 0xff, ..k };
        assert_eq!(other.signals(b"\x03\x18\x1a\x1c\x00\xff", false).0, vec![(1, SIGINT)]);
        // ISTRIP: the eighth bit goes before the comparison.
        assert_eq!(Keyboard { istrip: true, ..k }.signals(b"\x83", false).0, vec![(0, SIGINT)]);
        assert_eq!(k.signals(b"\x83", false).0, vec![]);
    }

    #[test]
    fn literal_next_quotes_one_byte_in_canonical_mode() {
        let k = sane();
        // Ctrl-V Ctrl-C is a ^C character; the next Ctrl-C a signal.
        assert_eq!(k.signals(b"\x16\x03\x03", false), (vec![(2, SIGINT)], false));
        // Ctrl-V Ctrl-V quotes the second.
        assert_eq!(k.signals(b"\x16\x16\x03", false), (vec![(2, SIGINT)], false));
        // A Ctrl-V at the end of one write quotes the start of the next.
        assert_eq!(k.signals(b"a\x16", false), (vec![], true));
        assert_eq!(k.signals(b"\x03\x03", true), (vec![(1, SIGINT)], false));
        // Not in non-canonical mode (Linux's n_tty), nor without IEXTEN.
        assert_eq!(Keyboard { icanon: false, ..k }.signals(b"\x16\x03", false).0, vec![(1, SIGINT)]);
        assert_eq!(Keyboard { iexten: false, ..k }.signals(b"\x16\x03", false).0, vec![(1, SIGINT)]);
    }

    #[test]
    fn background_access_follows_linux() {
        use TtyOp::*;
        // Reads: SIGTTIN; EIO if it's blocked or ignored, or the group is
        // orphaned.
        assert_eq!(background(Read, false, false, false), Ok(Some(SIGTTIN)));
        assert_eq!(background(Read, false, true, false), Err(errno::EIO));
        assert_eq!(background(Read, false, false, true), Err(errno::EIO));
        // Writes: only with TOSTOP; blocked or ignored, they go ahead.
        assert_eq!(background(Write, false, false, false), Ok(None));
        assert_eq!(background(Write, false, false, true), Ok(None));
        assert_eq!(background(Write, true, false, false), Ok(Some(SIGTTOU)));
        assert_eq!(background(Write, true, true, false), Ok(None));
        assert_eq!(background(Write, true, false, true), Err(errno::EIO));
        // Changes (tcsetattr, tcsetpgrp): SIGTTOU whatever TOSTOP says;
        // with it blocked, they go ahead (bash's tcsetpgrp from a child).
        assert_eq!(background(Change, false, false, false), Ok(Some(SIGTTOU)));
        assert_eq!(background(Change, false, true, false), Ok(None));
        assert_eq!(background(Change, false, true, true), Ok(None));
        assert_eq!(background(Change, false, false, true), Err(errno::EIO));
    }

    /// Typed keys on a real pty: written up to each signal character, the
    /// signal raised after it, all of it read on the slave.
    #[test]
    fn keys_are_written_around_their_signals() {
        let (mut master, mut slave) = (0, 0);
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        let mut t = get(slave).unwrap();
        // ISIG on, but no flushing on a signal, so the slave reads the rest.
        t.c_lflag = (t.c_lflag | libc::ISIG | libc::NOFLSH) & !(libc::ICANON | libc::ECHO);
        unsafe { libc::tcsetattr(slave, libc::TCSANOW, &t) };
        let keys = Keyboard::of(master).unwrap();
        assert!(keys.isig && keys.intr == 3);
        let mut seen = Vec::new();
        let (r, quoted) = write_keys(master, b"ab\x03cd\x1aef", &keys, false, |sig| {
            let mut n: libc::c_int = 0;
            unsafe { libc::ioctl(slave, libc::FIONREAD, &mut n) };
            seen.push((sig, n));
        });
        assert_eq!((r, quoted), (Ok(8), false));
        // Each signal, in order, once its byte had been written.
        assert_eq!(seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![SIGINT, SIGTSTP]);
        let mut buf = [0u8; 16];
        std::thread::sleep(std::time::Duration::from_millis(20));
        let n = unsafe { libc::read(slave, buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n >= 6, "read {n}");
        assert!(buf[..n as usize].starts_with(b"ab"));
        assert_eq!(identify(master), Some(TtyId { n: identify(slave).unwrap().n, master: true }));
        assert!(!identify(slave).unwrap().master);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    #[test]
    fn round_trips_a_typical_terminal() {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        t.c_iflag = libc::ICRNL | libc::IXON | libc::IUTF8;
        t.c_oflag = libc::OPOST | libc::ONLCR;
        t.c_cflag = libc::CS8 | libc::CREAD;
        t.c_lflag = libc::ISIG | libc::ICANON | libc::ECHO | libc::ECHOE | libc::IEXTEN;
        t.c_cc[libc::VINTR] = 3;
        t.c_cc[libc::VMIN] = 1;
        t.c_ispeed = 38400;
        t.c_ospeed = 38400;
        let l = from_darwin(&t);
        // Linux values: ICRNL|IXON|IUTF8, OPOST|ONLCR, CS8|CREAD|B38400, ISIG|ICANON|ECHO|ECHOE|IEXTEN.
        assert_eq!(l.iflag, 0o400 | 0o2000 | 0o40000);
        assert_eq!(l.oflag, 0o1 | 0o4);
        assert_eq!(l.cflag, 0o60 | 0o200 | 0o17);
        assert_eq!(l.lflag, 0o1 | 0o2 | 0o10 | 0o20 | 0o100000);
        assert_eq!((l.cc[0], l.cc[6]), (3, 1));
        let mut back: libc::termios = unsafe { std::mem::zeroed() };
        apply(&l, &mut back, false);
        assert_eq!(
            (back.c_iflag, back.c_oflag, back.c_cflag, back.c_lflag, back.c_ospeed),
            (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_ospeed)
        );
        assert_eq!(back.c_cc, t.c_cc);
    }
}
