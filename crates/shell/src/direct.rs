//! Clients' frames drawn straight into the shell's drawable (docs/gpu.md,
//! "Drawing into the drawable"): the shell's half; the GPU worker's is
//! `vkr_direct.c` in our virglrenderer patch.
//!
//! While one opaque surface fills the screen, the shell holds a drawable
//! and offers its IOSurface to the GPU workers, for the next frame of any
//! of the buffers that surface has committed (named by their shared
//! memory's key, `macwl_compositor::fences::buffer_key`). A worker
//! submitting that buffer's prime blit first copies the frame into the
//! drawable and says so (CLAIM), then says when the copy is done (DONE);
//! the drawable is presented then, with nothing for the shell to draw.
//!
//! The workers find the shell through a bootstrap name, written to
//! `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY.direct`; each says HELLO with its
//! port. Messages are Mach messages, laid out as `vkr_direct.c`'s.

use std::{
    ffi::CString,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_quartz_core::CAMetalDrawable;

pub const KEY: usize = 64;
pub type Key = [u8; KEY];
const MAX_KEYS: usize = 8;

const MSG_HELLO: i32 = 1;
const MSG_OFFER: i32 = 2;
const MSG_REVOKE: i32 = 3;
const MSG_CLAIM: i32 = 4;
const MSG_DONE: i32 = 5;
const MSG_REVOKED: i32 = 6;
const MSG_REKEY: i32 = 7;

#[allow(non_camel_case_types)]
type mach_port_t = u32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
    bits: u32,
    size: u32,
    remote_port: mach_port_t,
    local_port: mach_port_t,
    voucher_port: mach_port_t,
    id: i32,
}

#[repr(C, packed(4))]
struct OfferMsg {
    header: Header,
    descriptor_count: u32,
    port_name: mach_port_t,
    port_pad1: u32,
    /// pad2: 16 bits, disposition: 8, type: 8.
    port_word: u32,
    id: u64,
    width: u32,
    height: u32,
    key_count: u32,
    keys: [Key; MAX_KEYS],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct IdMsg {
    header: Header,
    id: u64,
    key: Key,
}

const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_MSG_TYPE_MOVE_SEND: u32 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_PORT_DESCRIPTOR: u32 = 0;

unsafe extern "C" {
    static mach_task_self_: mach_port_t;
    static bootstrap_port: mach_port_t;
    fn mach_msg(msg: *mut Header, option: i32, send_size: u32, rcv_size: u32, rcv_name: mach_port_t, timeout: u32, notify: mach_port_t) -> i32;
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> i32;
    fn mach_port_insert_right(task: mach_port_t, name: mach_port_t, poly: mach_port_t, poly_type: u32) -> i32;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> i32;
    fn bootstrap_register(bp: mach_port_t, name: *const std::ffi::c_char, sp: mach_port_t) -> i32;
}

fn task() -> mach_port_t {
    unsafe { mach_task_self_ }
}

/// A drawable, presented from the receive thread (Metal's drawables can be).
pub struct SendDrawable(pub Retained<ProtocolObject<dyn CAMetalDrawable>>);
unsafe impl Send for SendDrawable {}

pub struct Offer {
    pub id: u64,
    pub drawable: Option<SendDrawable>,
    pub keys: Vec<Key>,
    /// A worker took it, for a frame of this buffer.
    pub claimed: Option<Key>,
    pub claimed_at: f64,
    /// Revoked, waiting for the workers to say so.
    pub revoking: usize,
    /// Workers it went to, and those that couldn't take it.
    pub sent_to: usize,
    pub declined: usize,
}

#[derive(Default)]
pub struct State {
    peers: Vec<mach_port_t>,
    pub offer: Option<Offer>,
    /// The buffer whose frame was last presented directly, if the shell
    /// hasn't seen its commit yet (the copy can finish first).
    pub last_done: Option<Key>,
    next_id: u64,
    trace: bool,
}

impl State {
    pub fn has_peers(&self) -> bool {
        !self.peers.is_empty()
    }
}

/// What became of a committed frame's offer.
#[derive(PartialEq, Eq, Debug)]
pub enum Claim {
    /// Not claimed for it: the shell draws it.
    No,
    /// A worker is copying it into the offered drawable.
    Claimed,
    /// Copied and presented already.
    Presented,
}

#[derive(Clone)]
pub struct Direct {
    pub state: Arc<(Mutex<State>, Condvar)>,
}

impl Direct {
    /// Register as `dev.macwl.shell.PID` and say so in
    /// `RUNTIME_DIR/DISPLAY.direct`. `on_change` runs (on the receive
    /// thread) when an offer was used or let go of, to offer again.
    pub fn start(runtime_dir: &Path, display: &str, on_change: impl Fn() + Send + 'static) -> Option<Direct> {
        let mut port: mach_port_t = 0;
        unsafe {
            if mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut port) != 0 {
                return None;
            }
            if mach_port_insert_right(task(), port, port, MACH_MSG_TYPE_MAKE_SEND) != 0 {
                return None;
            }
        }
        let name = format!("dev.macwl.shell.{}", std::process::id());
        let cname = CString::new(name.clone()).ok()?;
        let r = unsafe { bootstrap_register(bootstrap_port, cname.as_ptr(), port) };
        if r != 0 {
            eprintln!("shell: direct: bootstrap_register: {r}");
            return None;
        }
        let file = runtime_dir.join(format!("{display}.direct"));
        std::fs::write(&file, &name).ok()?;
        let direct = Direct { state: Arc::default() };
        direct.state.0.lock().unwrap().trace = std::env::var_os("MACWL_KEY_TRACE").is_some();
        let state = direct.state.clone();
        std::thread::Builder::new().name("direct".into()).spawn(move || receive(port, &state, &on_change)).ok()?;
        Some(direct)
    }

    /// Offer `drawable` for the next frame of any of `keys`.
    pub fn offer(&self, drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>, keys: &[Key]) -> bool {
        use objc2_metal::MTLTexture as _;
        let texture = drawable.texture();
        let Some(surface) = texture.iosurface() else { return false };
        let (width, height) = (texture.width() as u32, texture.height() as u32);
        let mut s = self.state.0.lock().unwrap();
        if s.offer.is_some() || s.peers.is_empty() {
            return false;
        }
        s.next_id += 1;
        let id = s.next_id;
        let mut msg_keys = [[0u8; KEY]; MAX_KEYS];
        for (k, key) in msg_keys.iter_mut().zip(keys) {
            *k = *key;
        }
        let peers = s.peers.clone();
        let mut sent = 0;
        let mut dead = Vec::new();
        for peer in peers {
            let mut m = OfferMsg {
                header: Header {
                    bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                    size: size_of::<OfferMsg>() as u32,
                    remote_port: peer,
                    id: MSG_OFFER,
                    ..Default::default()
                },
                descriptor_count: 1,
                // A send right per peer, moved to it.
                port_name: surface.create_mach_port(),
                port_pad1: 0,
                port_word: (MACH_MSG_TYPE_MOVE_SEND << 16) | (MACH_MSG_PORT_DESCRIPTOR << 24),
                id,
                width,
                height,
                key_count: keys.len().min(MAX_KEYS) as u32,
                keys: msg_keys,
            };
            let r = unsafe { mach_msg(&mut m.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, size_of::<OfferMsg>() as u32, 0, 0, 100, 0) };
            if r == 0 {
                sent += 1;
            } else {
                unsafe { mach_port_deallocate(task(), m.port_name) };
                dead.push(peer);
            }
        }
        s.peers.retain(|p| !dead.contains(p));
        if sent == 0 {
            return false;
        }
        s.offer = Some(Offer {
            id,
            drawable: Some(SendDrawable(drawable)),
            keys: keys.to_vec(),
            claimed: None,
            claimed_at: 0.0,
            revoking: 0,
            sent_to: sent,
            declined: 0,
        });
        if s.trace {
            eprintln!("shell: direct offer {id} ({} keys)", keys.len());
        }
        true
    }

    /// The offer is for these keys now (a window's buffers, learned as it
    /// commits each).
    pub fn rekey(&self, keys: &[Key]) {
        let mut s = self.state.0.lock().unwrap();
        let peers = s.peers.clone();
        let Some(offer) = s.offer.as_mut() else { return };
        if offer.claimed.is_some() || offer.revoking > 0 || offer.keys == keys {
            return;
        }
        offer.keys = keys.to_vec();
        let mut msg_keys = [[0u8; KEY]; MAX_KEYS];
        for (k, key) in msg_keys.iter_mut().zip(keys) {
            *k = *key;
        }
        for peer in peers {
            // An offer's layout, without its port.
            let mut m = OfferMsg {
                header: Header { bits: MACH_MSG_TYPE_COPY_SEND, size: size_of::<OfferMsg>() as u32, remote_port: peer, id: MSG_REKEY, ..Default::default() },
                descriptor_count: 0,
                port_name: 0,
                port_pad1: 0,
                port_word: 0,
                id: offer.id,
                width: 0,
                height: 0,
                key_count: keys.len().min(MAX_KEYS) as u32,
                keys: msg_keys,
            };
            unsafe { mach_msg(&mut m.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, size_of::<OfferMsg>() as u32, 0, 0, 100, 0) };
        }
    }

    /// Take the offer back, unless it's claimed (then it's presented as
    /// it's done).
    pub fn revoke(&self) {
        let mut s = self.state.0.lock().unwrap();
        let peers = s.peers.clone();
        let Some(offer) = s.offer.as_mut() else { return };
        if offer.claimed.is_some() || offer.revoking > 0 {
            return;
        }
        // (Those that declined it already have let it go.)
        let id = offer.id;
        let mut waiting = offer.sent_to - offer.declined;
        for peer in peers {
            let mut m = IdMsg {
                header: Header { bits: MACH_MSG_TYPE_COPY_SEND, size: size_of::<IdMsg>() as u32, remote_port: peer, id: MSG_REVOKE, ..Default::default() },
                id,
                key: [0; KEY],
            };
            // A worker gone won't answer, nor write.
            if unsafe { mach_msg(&mut m.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, size_of::<IdMsg>() as u32, 0, 0, 100, 0) } != 0 {
                waiting = waiting.saturating_sub(1);
            }
        }
        if waiting == 0 {
            s.offer = None;
        } else if let Some(offer) = s.offer.as_mut() {
            offer.revoking = waiting;
        }
    }

    /// Give up on an offer claimed `limit` ago and never done (its worker
    /// died, say): nothing will write its drawable now. Whether it did.
    pub fn expire(&self, limit: f64) -> bool {
        let mut s = self.state.0.lock().unwrap();
        let now = objc2_quartz_core::CACurrentMediaTime();
        if s.offer.as_ref().is_some_and(|o| o.claimed.is_some() && now - o.claimed_at > limit) {
            s.offer = None;
            return true;
        }
        false
    }

    /// For a frame of `key` just committed: wait until the offer is claimed
    /// for it, or `give_up` says to stop (checked each 100 µs), or `limit`
    /// passes.
    pub fn wait_claim(&self, key: &Key, limit: Duration, give_up: impl Fn() -> bool) -> Claim {
        let start = Instant::now();
        let (lock, cond) = &*self.state;
        let mut s = lock.lock().unwrap();
        loop {
            if s.last_done.as_ref() == Some(key) {
                s.last_done = None;
                return Claim::Presented;
            }
            match s.offer.as_ref() {
                Some(o) if o.claimed.as_ref() == Some(key) => return Claim::Claimed,
                Some(o) if o.keys.contains(key) && o.claimed.is_none() && o.revoking == 0 => {}
                _ => return Claim::No,
            }
            if start.elapsed() >= limit || give_up() {
                return Claim::No;
            }
            s = cond.wait_timeout(s, Duration::from_micros(100)).unwrap().0;
        }
    }
}

fn receive(port: mach_port_t, state: &(Mutex<State>, Condvar), on_change: &dyn Fn()) {
    #[repr(C, align(8))]
    struct Buf([u8; 2048]);
    let mut buf = Buf([0; 2048]);
    loop {
        let header = buf.0.as_mut_ptr().cast::<Header>();
        let r = unsafe { mach_msg(header, MACH_RCV_MSG, 0, buf.0.len() as u32, port, 0, 0) };
        if r != 0 {
            eprintln!("shell: direct: receive: {r:#x}");
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let h = unsafe { *header };
        let (lock, cond) = state;
        let mut s = lock.lock().unwrap();
        match h.id {
            MSG_HELLO => {
                // The worker's port, its send right ours now.
                if h.remote_port != 0 {
                    s.peers.push(h.remote_port);
                    if s.trace {
                        eprintln!("shell: direct: a GPU worker connected");
                    }
                }
                drop(s);
                on_change();
                continue;
            }
            MSG_CLAIM | MSG_DONE | MSG_REVOKED => {}
            _ => continue,
        }
        let m = unsafe { std::ptr::read_unaligned(header.cast::<IdMsg>()) };
        let id = m.id;
        let trace = s.trace;
        let Some(offer) = s.offer.as_mut().filter(|o| o.id == id) else { continue };
        let now = objc2_quartz_core::CACurrentMediaTime();
        let mut changed = false;
        match h.id {
            MSG_CLAIM => {
                offer.claimed = Some(m.key);
                offer.claimed_at = now;
            }
            MSG_DONE => {
                if trace {
                    eprintln!("shell: direct offer {id} done {:.1} ({:.3} ms after its claim)", now * 1e6, (now - offer.claimed_at) * 1e3);
                }
                if let Some(d) = offer.drawable.take() {
                    use objc2_metal::MTLDrawable as _;
                    d.0.present();
                }
                s.last_done = offer.claimed;
                s.offer = None;
                changed = true;
            }
            MSG_REVOKED => {
                if offer.claimed.is_some() {
                    // Claimed, then failed: given up.
                    s.offer = None;
                    changed = true;
                } else if offer.revoking > 0 {
                    offer.revoking -= 1;
                    if offer.revoking == 0 {
                        s.offer = None;
                        changed = true;
                    }
                } else {
                    // A worker couldn't take it; once none can, it's done.
                    offer.declined += 1;
                    if offer.declined >= offer.sent_to {
                        s.offer = None;
                        changed = true;
                    }
                }
            }
            _ => {}
        }
        cond.notify_all();
        drop(s);
        if changed {
            on_change();
        }
    }
}
