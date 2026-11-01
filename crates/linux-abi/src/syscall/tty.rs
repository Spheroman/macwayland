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
        TIOCSCTTY => ok(unsafe { libc::ioctl(host, libc::TIOCSCTTY as u64, arg as libc::c_int) } as i64).map(|_| 0),
        TIOCNOTTY => ok(unsafe { libc::ioctl(host, libc::TIOCNOTTY) } as i64).map(|_| 0),
        _ => Err(errno::ENOTTY),
    }
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
