//! Minimal Smithay compositor core for macOS.
//!
//! Speaks wl_compositor / wl_shm / xdg_shell (with server-side decorations,
//! which here means none), a seat with a keyboard and pointer, and the data
//! device (clipboard), and hands what's on screen to a caller-supplied
//! [`Host`]: the buffers that a window, its subsurfaces and its popups
//! (menus, tooltips) commit, and where each goes. Rendering lives elsewhere (the headless
//! binary dumps PPMs; `macwl-shell` presents through Metal).

use std::{
    io::{Read, Write},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_output, delegate_seat, delegate_shm, delegate_xdg_decoration,
    delegate_xdg_shell,
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, find_popup_root_surface,
        get_popup_toplevel_coords,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, XkbConfig},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData, Focus, MotionEvent},
    },
    reexports::{
        wayland_protocols::xdg::{decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode, shell::server::xdg_toplevel},
        calloop::{
            self, EventLoop, Interest, Mode as CalloopMode, PostAction, channel,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{
                wl_buffer, wl_output,
                wl_seat,
                wl_shm,
                wl_surface::{self, WlSurface},
            },
        },
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        cursor_shape::CursorShapeManagerState,
        seat::CURSOR_IMAGE_ROLE,
        tablet_manager::TabletSeatHandler,
        output::OutputHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage, SUBSURFACE_ROLE,
            SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction, get_parent, get_role, is_sync_subsurface,
            with_states, with_surface_tree_downward, with_surface_tree_upward,
        },
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE,
            XdgShellHandler, XdgShellState,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        socket::ListeningSocketSource,
    },
};
use tracing::{info, warn};

pub use smithay::{reexports::wayland_server::protocol::wl_shm::Format as ShmFormat, wayland::shm::BufferData};

pub mod keymap;

/// Input from the host (the shell's AppKit events), in Linux terms. Times
/// are the event's, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// A key went down or up: its evdev code ([`keymap::evdev_from_mac`]).
    Key { code: u32, pressed: bool, time_ms: u32 },
    /// The pointer moved to (`x`, `y`) on the screen, in pixels from its top
    /// left corner.
    PointerMotion { x: f64, y: f64, time_ms: u32 },
    /// A button went down or up: its evdev code (`BTN_LEFT`, ...).
    PointerButton { button: u32, pressed: bool, time_ms: u32 },
    /// Scrolling, in screen pixels (positive: down and right, as Wayland has
    /// it): how far content should move, as macOS has it. `steps` are a
    /// wheel's clicks in 120ths, for wheels; `finger` says a trackpad, whose
    /// scrolling ends with `stop`.
    PointerAxis { dx: f64, dy: f64, steps: (i32, i32), finger: bool, stop: bool, time_ms: u32 },
}

/// How much further content moves than a Linux touchpad's scroll deltas
/// say: clients scale them up by this (GTK 4's `MAGIC_SCROLL_FACTOR`), as
/// libinput's units are smaller than content pixels. Finger scrolling goes
/// out divided by it, so content follows the fingers.
const FINGER_SCROLL_FACTOR: f64 = 2.5;

/// Mouse buttons' evdev codes.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Where the host sends [`InputEvent`]s: the compositor wakes the moment one
/// arrives.
pub type InputSender = calloop::channel::Sender<InputEvent>;

/// A channel for input: the sender for the host, the receiving end for
/// [`Options::input`].
pub fn input_channel() -> (InputSender, calloop::channel::Channel<InputEvent>) {
    calloop::channel::channel()
}

/// Where the host sends text copied on it (the Mac's clipboard).
pub type ClipboardSender = calloop::channel::Sender<String>;

/// A channel for the host's clipboard: the sender for the host, the
/// receiving end for [`Options::clipboard`].
pub fn clipboard_channel() -> (ClipboardSender, calloop::channel::Channel<String>) {
    calloop::channel::channel()
}

/// Where the host sends the display's refresh ticks (vsync): at each, the
/// compositor tells clients to draw their next frame.
pub type RefreshSender = calloop::channel::Sender<()>;

/// A channel for refresh ticks: the sender for the host, the receiving end
/// for [`Options::refresh`].
pub fn refresh_channel() -> (RefreshSender, calloop::channel::Channel<()>) {
    calloop::channel::channel()
}

/// How [`run_with`] sets up the compositor.
pub struct Options {
    /// Events to deliver to the focused client.
    pub input: Option<calloop::channel::Channel<InputEvent>>,
    /// The display's refresh ticks. With them, clients draw once per
    /// refresh, starting as it begins (frame callbacks go out on each tick);
    /// without, on a timer.
    pub refresh: Option<calloop::channel::Channel<()>>,
    /// The screen in pixels, and its scale (2 on Retina displays).
    pub screen: (i32, i32),
    pub scale: i32,
    /// The display's refresh rate in mHz, as `wl_output` reports it.
    pub refresh_mhz: i32,
    /// Text copied on the host (the Mac's clipboard), for clients to paste.
    pub clipboard: Option<calloop::channel::Channel<String>>,
}

impl Default for Options {
    /// No input; the screen from MACWL_OUTPUT_SIZE=WxH (or 1920x1200), scale 1, 60 Hz.
    fn default() -> Self {
        let screen = std::env::var("MACWL_OUTPUT_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((1920, 1200));
        Options { input: None, refresh: None, screen, scale: 1, refresh_mhz: 60_000, clipboard: None }
    }
}

/// A rectangle of a buffer, in its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// What the host draws: surfaces (a window and its subsurfaces, then its
/// popups and theirs), each with an id that stays the same for its life.
pub trait Host: Send {
    /// Surface `id` committed a buffer: its pixels (the whole pool) plus
    /// their layout, and what changed since its last frame (clipped to the
    /// buffer; all of it when the client didn't say).
    fn frame(&mut self, id: u32, pixels: &[u8], data: &BufferData, damage: &[Rect]);
    /// What's on screen changed: these surfaces, bottom to top. A surface
    /// not listed isn't shown: forget it, and if it comes back, take all
    /// of its next frame.
    fn layout(&mut self, layers: &[Layer]);
    /// A client copied `text` to the clipboard.
    fn copied(&mut self, _text: &str) {}
    /// What the pointer should look like now.
    fn cursor(&mut self, _cursor: Cursor) {}
}

/// What the pointer looks like, as the client under it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Hidden,
    /// A standard cursor, by its CSS name: "default", "text", "pointer",
    /// "ew-resize", ...
    Named(&'static str),
    /// The client's own image: tightly packed premultiplied BGRA, `width`
    /// by `height` pixels at `scale` pixels a point, with its hotspot in
    /// points from the top left.
    Image { pixels: Vec<u8>, width: i32, height: i32, scale: i32, hotspot: (i32, i32) },
}

/// A surface on screen: its buffer's top left corner, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layer {
    pub id: u32,
    pub x: i32,
    pub y: i32,
}

/// Receives each buffer a window commits, as [`Host::frame`] (for hosts
/// that only show the window, such as tests).
pub type FrameSink = Box<dyn FnMut(&[u8], &BufferData, &[Rect]) + Send>;

impl Host for FrameSink {
    fn frame(&mut self, _id: u32, pixels: &[u8], data: &BufferData, damage: &[Rect]) {
        self(pixels, data, damage)
    }

    fn layout(&mut self, _layers: &[Layer]) {}
}

/// How often clients are told to draw their next frame, without refresh
/// ticks from the host.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// With them: how often to check for clients left waiting (the ticks stop
/// while the display sleeps).
const FALLBACK_INTERVAL: Duration = Duration::from_millis(16);
/// How long a client may wait for a frame callback before the fallback sends
/// it anyway.
const MAX_WAIT: Duration = Duration::from_millis(100);

struct App {
    dh: DisplayHandle,
    host: Box<dyn Host>,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    seat: Seat<Self>,
    data_device_state: DataDeviceState,
    _decoration_state: XdgDecorationState,
    _cursor_shape_state: Option<CursorShapeManagerState>,
    /// The surface the client under the pointer has as its cursor, if it
    /// gave one (whose next buffers change the cursor).
    cursor_surface: Option<WlSurface>,
    /// The cursor to show, once the requests at hand are all handled (a
    /// client sets a cursor and commits its image one after the other),
    /// and the one the host was last told about.
    cursor: Option<Cursor>,
    cursor_shown: Option<Cursor>,
    /// The one screen, in logical units: windows are configured to fill it.
    screen: Size<i32, smithay::utils::Logical>,
    /// The screen in pixels.
    screen_px: (i32, i32),
    /// Windows, bottom to top: the order they were (last) mapped in. What's
    /// shown is the top window that fills the screen, and the dialogs
    /// (windows with a parent) above it.
    windows: Vec<ToplevelSurface>,
    popups: PopupManager,
    /// The layers the host was last told about.
    layout: Vec<Layer>,
    /// A client's copy to read, in this text type.
    copy_pending: Option<String>,
    /// Where the copies read come back to the event loop.
    copies: channel::Sender<String>,
    /// When frame callbacks were last sent.
    last_callbacks: Instant,
    start: Instant,
}

impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A window's first configure, once its parent (if any) is known:
        // a dialog chooses its size; anything else fills the screen,
        // maximized (fullscreen only if it asks: apps hide their toolbars
        // then).
        if let Some(window) = self.windows.iter().find(|w| w.wl_surface() == surface)
            && !window.is_initial_configure_sent()
        {
            let (dialog, screen) = (window.parent().is_some(), self.screen);
            window.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Activated);
                state.bounds = Some(screen);
                if !dialog {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(screen);
                }
            });
            window.send_configure();
        }
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            let _ = popup.send_configure();
        }
        // The surface's new state, and its synchronized subsurfaces' (which
        // apply with it); a synchronized subsurface's own commit waits for
        // its parent's.
        if is_sync_subsurface(surface) {
            return;
        }
        let mut tree = Vec::new();
        with_surface_tree_upward(
            surface,
            (),
            |_, _, &()| TraversalAction::DoChildren(()),
            |s, _, &()| tree.push(s.clone()),
            |_, _, &()| true,
        );
        for s in &tree {
            self.take_buffer(s);
        }
    }
}

impl App {
    /// Take the buffer `surface` committed, if any: a window's, a popup's or
    /// a subsurface's goes to the host, a cursor's becomes the cursor.
    fn take_buffer(&mut self, surface: &WlSurface) {
        let (assignment, damage, scale) = with_states(surface, |states| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            let current = attrs.current();
            (current.buffer.take(), std::mem::take(&mut current.damage), current.buffer_scale.max(1))
        });
        // Windows, their popups and subsurfaces are shown, cursors go to the
        // host as cursors.
        let role = get_role(surface);
        if role == Some(CURSOR_IMAGE_ROLE)
            && let Some(BufferAssignment::NewBuffer(buffer)) = &assignment
        {
            let image = with_buffer_contents(buffer, |ptr, len, data| {
                let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
                cursor_pixels(pool, &data, scale)
            });
            buffer.release();
            match image {
                Ok(image) => {
                    set_cursor_image(surface, Some(image));
                    if self.cursor_surface.as_ref() == Some(surface) {
                        self.send_cursor_image(surface);
                    }
                }
                Err(e) => warn!("cursor not an shm buffer: {e:?}"),
            }
            return;
        }
        let shown = matches!(role, Some(XDG_TOPLEVEL_ROLE | XDG_POPUP_ROLE | SUBSURFACE_ROLE));
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                if shown {
                    let was_mapped = mapped(surface).is_some();
                    let (id, host) = (surface_id(surface), &mut self.host);
                    let result = with_buffer_contents(&buffer, |ptr, len, data| {
                        let pixels = unsafe { std::slice::from_raw_parts(ptr, len) };
                        host.frame(id, pixels, &data, &buffer_damage(&damage, scale, data.width, data.height));
                        set_mapped(surface, Some(Mapped { size: (data.width, data.height), scale }));
                    });
                    if let Err(e) = result {
                        warn!("not an shm buffer: {e:?}");
                    }
                    // A window appearing goes on top, with the keyboard.
                    if role == Some(XDG_TOPLEVEL_ROLE)
                        && !was_mapped
                        && let Some(i) = self.windows.iter().position(|w| w.wl_surface() == surface)
                    {
                        let window = self.windows.remove(i);
                        self.windows.push(window);
                        self.focus_top();
                    }
                }
                buffer.release();
            }
            Some(BufferAssignment::Removed) => {
                set_mapped(surface, None);
                if role == Some(XDG_TOPLEVEL_ROLE) {
                    self.focus_top();
                }
            }
            None => {}
        }
    }
}

/// A surface's id for the host, made on first use.
fn surface_id(surface: &WlSurface) -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    struct Id(u32);
    with_states(surface, |states| {
        states.data_map.insert_if_missing_threadsafe(|| Id(NEXT.fetch_add(1, Ordering::Relaxed)));
        states.data_map.get::<Id>().unwrap().0
    })
}

/// A cursor surface's latest image.
#[derive(Clone)]
struct CursorBuffer {
    /// Tightly packed premultiplied BGRA (opaque for XRGB).
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    /// Pixels per point.
    scale: i32,
}

/// A cursor buffer as a [`CursorBuffer`].
fn cursor_pixels(pool: &[u8], data: &BufferData, scale: i32) -> CursorBuffer {
    let (w, h, stride, offset) = (data.width as usize, data.height as usize, data.stride as usize, data.offset as usize);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = offset + row * stride;
        pixels.extend_from_slice(pool.get(start..start + w * 4).unwrap_or(&[]));
    }
    pixels.resize(w * h * 4, 0);
    if data.format != ShmFormat::Argb8888 {
        pixels.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    CursorBuffer { pixels, width: data.width, height: data.height, scale }
}

type CursorImage = Mutex<Option<CursorBuffer>>;

fn set_cursor_image(surface: &WlSurface, image: Option<CursorBuffer>) {
    with_states(surface, |states| {
        states.data_map.insert_if_missing_threadsafe(|| CursorImage::new(None));
        *states.data_map.get::<CursorImage>().unwrap().lock().unwrap() = image;
    })
}

/// A surface's current buffer: its size in pixels and scale (pixels per
/// surface unit).
#[derive(Debug, Clone, Copy)]
struct Mapped {
    size: (i32, i32),
    scale: i32,
}

fn set_mapped(surface: &WlSurface, mapped: Option<Mapped>) {
    with_states(surface, |states| {
        states.data_map.insert_if_missing_threadsafe(|| Mutex::new(None::<Mapped>));
        *states.data_map.get::<Mutex<Option<Mapped>>>().unwrap().lock().unwrap() = mapped;
    })
}

fn mapped(surface: &WlSurface) -> Option<Mapped> {
    with_states(surface, mapped_in)
}

fn mapped_in(states: &SurfaceData) -> Option<Mapped> {
    states.data_map.get::<Mutex<Option<Mapped>>>().and_then(|m| *m.lock().unwrap())
}

/// The surface at the root of `surface`'s subsurface tree (a window, a
/// popup, a cursor).
fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

/// `root`, its buffer's corner at `origin` on screen, and its subsurfaces,
/// bottom to top, into `out`. Subsurfaces are drawn at the root's scale; one
/// with no buffer hides its own subsurfaces.
fn place_tree(root: &WlSurface, origin: (i32, i32), scale: i32, out: &mut Vec<Placed>) {
    // Each surface's corner: its parent's, plus where it was put on it.
    let corner = |s: &WlSurface, states: &SurfaceData, parent: (i32, i32)| {
        if s == root {
            return origin;
        }
        let loc = states.cached_state.get::<SubsurfaceCachedState>().current().location;
        (parent.0 + loc.x * scale, parent.1 + loc.y * scale)
    };
    with_surface_tree_upward(
        root,
        origin,
        |s, states, &parent| match mapped_in(states) {
            Some(_) if s.is_alive() => TraversalAction::DoChildren(corner(s, states, parent)),
            _ => TraversalAction::SkipChildren,
        },
        |s, states, &parent| {
            if let Some(mapped) = mapped_in(states).filter(|_| s.is_alive()) {
                out.push(Placed { surface: s.clone(), origin: corner(s, states, parent), mapped });
            }
        },
        |_, _, _| true,
    );
}

/// A surface on screen. Hosts draw buffers 1:1 in pixels: the window
/// centred (cut off at the screen's edges when it's bigger), popups where
/// the client asked, relative to it.
struct Placed {
    surface: WlSurface,
    /// Its buffer's top left corner on screen, in pixels.
    origin: (i32, i32),
    mapped: Mapped,
}

impl Placed {
    /// Whether the pointer at (`x`, `y`) on screen (in pixels) is over it:
    /// on its buffer, and in its input region.
    fn contains(&self, x: f64, y: f64) -> bool {
        let (sx, sy) = (x - self.origin.0 as f64, y - self.origin.1 as f64);
        let (w, h) = self.mapped.size;
        if !((0.0..w as f64).contains(&sx) && (0.0..h as f64).contains(&sy)) {
            return false;
        }
        let scale = self.mapped.scale as f64;
        let point = ((sx / scale) as i32, (sy / scale) as i32);
        with_states(&self.surface, |states| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            attrs.current().input_region.as_ref().is_none_or(|r| r.contains(point))
        })
    }
}

/// A surface's window geometry: the part that's the window, without
/// shadows (all of it when the client didn't say).
fn window_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    with_states(surface, |states| states.cached_state.get::<SurfaceCachedState>().current().geometry)
}

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    /// Configured at its first commit, when it's known whether it's a
    /// dialog; it goes on top when it maps.
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        info!("new toplevel");
        self.windows.push(surface);
    }

    /// Fullscreen when an app asks (F11, a video), back to maximized when it
    /// asks again: both fill the screen. Dialogs stay as they are.
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        self.set_fullscreen(&surface, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.set_fullscreen(&surface, false);
    }

    /// Windows are always maximized: asking either way gets that.
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.windows.retain(|w| w != &surface);
        self.focus_top();
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = self.popup_geometry(&surface, positioner);
        surface.with_pending_state(|state| state.geometry = geometry);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("couldn't track popup: {e:?}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        let geometry = self.popup_geometry(&surface, positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        let _ = surface.send_configure();
    }

    /// A menu wants all input until it's dismissed: clicks elsewhere close
    /// it (and its parents), keys go to it.
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else { return };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else { return };
        // As Smithay's anvil does: only for the input that opened it.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial) || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}

impl SeatHandler for App {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// The client under the pointer sets what it looks like.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_surface = None;
        match image {
            CursorImageStatus::Hidden => self.cursor = Some(Cursor::Hidden),
            CursorImageStatus::Named(icon) => self.cursor = Some(Cursor::Named(icon.name())),
            // Its image now if it has one, else when it commits one.
            CursorImageStatus::Surface(surface) => {
                self.send_cursor_image(&surface);
                self.cursor_surface = Some(surface);
            }
        }
    }

    /// The clipboard goes with the keyboard: the client that has it is
    /// offered what's copied, and may copy.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client);
    }
}

impl OutputHandler for App {}

impl App {
    /// Tell every window (and its subsurfaces and popups) to draw its next
    /// frame.
    fn frame_callbacks(&mut self) {
        let now = self.start.elapsed().as_millis() as u32;
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            send_frame_callbacks(toplevel.wl_surface(), now);
            for (popup, _) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
                send_frame_callbacks(popup.wl_surface(), now);
            }
        }
        self.popups.cleanup();
        self.last_callbacks = Instant::now();
    }

    /// The windows shown, bottom to top: the top one that fills the screen
    /// and the dialogs above it.
    fn shown_windows(&self) -> &[ToplevelSurface] {
        let shown = |w: &&ToplevelSurface| w.alive() && mapped(w.wl_surface()).is_some();
        let base = self.windows.iter().rposition(|w| shown(&w) && w.parent().is_none()).unwrap_or(0);
        &self.windows[base..]
    }

    /// What's on screen, bottom to top: windows, each followed by its
    /// popups (each above its parent), each with its subsurfaces (above or
    /// below it, as the client stacked them).
    fn placed(&self) -> Vec<Placed> {
        let (w, h) = self.screen_px;
        let mut placed: Vec<Placed> = Vec::new();
        for window in self.shown_windows() {
            let surface = window.wl_surface();
            let Some(m) = mapped(surface).filter(|_| window.alive()) else { continue };
            // Centred, its window geometry (without shadows, which may go
            // off screen): on the screen, or a dialog on its parent.
            let parent = window.parent().and_then(|p| placed.iter().find(|q| q.surface == p));
            let (px, py, pw, ph) = parent.map_or((0, 0, w, h), |p| (p.origin.0, p.origin.1, p.mapped.size.0, p.mapped.size.1));
            let (gx, gy, gw, gh) = window_geometry(surface)
                .map_or((0, 0, m.size.0, m.size.1), |g| (g.loc.x * m.scale, g.loc.y * m.scale, g.size.w * m.scale, g.size.h * m.scale));
            let origin = ((px + (pw - gw) / 2).max(0) - gx, (py + (ph - gh) / 2).max(0) - gy);
            place_tree(surface, origin, m.scale, &mut placed);
            // Popups are placed relative to the window's geometry, in its
            // units (and drawn at its scale).
            let geometry = window_geometry(surface).map_or((0, 0).into(), |g| g.loc);
            let popups: Vec<_> = PopupManager::popups_for_surface(surface).collect();
            for (popup, offset) in popups.into_iter().rev() {
                let surface = popup.wl_surface();
                let Some(pm) = mapped(surface).filter(|_| surface.is_alive()) else { continue };
                let loc = geometry + offset - popup.geometry().loc;
                let origin = (origin.0 + loc.x * m.scale, origin.1 + loc.y * m.scale);
                place_tree(surface, origin, pm.scale, &mut placed);
            }
        }
        placed
    }

    fn set_fullscreen(&mut self, window: &ToplevelSurface, on: bool) {
        if window.parent().is_none() {
            window.with_pending_state(|state| {
                if on {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                }
            });
        }
        if window.is_initial_configure_sent() {
            window.send_configure();
        }
    }

    /// Give the keyboard to the top window that's shown.
    fn focus_top(&mut self) {
        let top = self.shown_windows().iter().rev().find(|w| mapped(w.wl_surface()).is_some());
        let focus = top.map(|w| w.wl_surface().clone());
        self.focus(focus);
    }

    /// Give the keyboard to `surface`, unless a menu has it.
    fn focus(&mut self, surface: Option<WlSurface>) {
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        if keyboard.is_grabbed() || keyboard.current_focus() == surface {
            return;
        }
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
    }

    /// A click on a window gives it the keyboard, or the dialog it has open.
    fn click_focus(&mut self) {
        let Some(under) = self.seat.get_pointer().and_then(|p| p.current_focus()) else { return };
        let under = root_surface(&under);
        let shown = self.shown_windows();
        let Some(mut window) = shown.iter().position(|w| w.wl_surface() == &under) else { return };
        while let Some(dialog) =
            shown.iter().rposition(|w| w.parent().as_ref() == Some(shown[window].wl_surface()) && mapped(w.wl_surface()).is_some())
        {
            window = dialog;
        }
        let surface = shown[window].wl_surface().clone();
        self.focus(Some(surface));
    }

    /// Read what a client just copied (as text, on a thread: the client
    /// writes it at its own pace), for the host.
    fn read_copy(&mut self) {
        let Some(mime) = self.copy_pending.take() else { return };
        let (mut reader, writer) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(e) => return warn!("no pipe for the clipboard: {e}"),
        };
        if let Err(e) = request_data_device_client_selection(&self.seat, mime, writer.into()) {
            return warn!("couldn't read the clipboard: {e}");
        }
        let copies = self.copies.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if (&mut reader).take(MAX_COPY).read_to_end(&mut bytes).is_ok() {
                let _ = copies.send(String::from_utf8_lossy(&bytes).into_owned());
            }
        });
    }

    /// Tell the host about the cursor, if it changed.
    fn show_cursor(&mut self) {
        if let Some(cursor) = self.cursor.take()
            && self.cursor_shown.as_ref() != Some(&cursor)
        {
            self.host.cursor(cursor.clone());
            self.cursor_shown = Some(cursor);
        }
    }

    /// Show a cursor surface's image, if it has one.
    fn send_cursor_image(&mut self, surface: &WlSurface) {
        let image = with_states(surface, |states| {
            let image = states.data_map.get::<CursorImage>()?.lock().unwrap().clone()?;
            let hotspot = states.data_map.get::<CursorImageSurfaceData>()?.lock().unwrap().hotspot;
            Some((image, hotspot))
        });
        let Some((CursorBuffer { pixels, width, height, scale }, hotspot)) = image else { return };
        self.cursor = Some(Cursor::Image { pixels, width, height, scale, hotspot: (hotspot.x, hotspot.y) });
    }

    /// Tell the host what's on screen, if that changed.
    fn relayout(&mut self) {
        let layers: Vec<Layer> =
            self.placed().iter().map(|p| Layer { id: surface_id(&p.surface), x: p.origin.0, y: p.origin.1 }).collect();
        if layers != self.layout {
            self.host.layout(&layers);
            self.layout = layers;
        }
    }

    /// Where a popup goes, relative to its parent's window geometry: where
    /// the client asked, moved or flipped to stay on screen.
    fn popup_geometry(&self, popup: &PopupSurface, positioner: PositionerState) -> Rectangle<i32, Logical> {
        let kind = PopupKind::Xdg(popup.clone());
        let window = find_popup_root_surface(&kind).ok();
        let Some(window_mapped) = window.as_ref().and_then(mapped) else { return positioner.get_geometry() };
        let window = window.unwrap();
        let placed = self.placed();
        let Some(base) = placed.iter().find(|p| p.surface == window) else { return positioner.get_geometry() };
        // The screen, in the window's units, from its geometry's corner...
        let scale = window_mapped.scale;
        let geometry = window_geometry(&window).map_or((0, 0).into(), |g| g.loc);
        let screen_loc = Point::<i32, Logical>::from((-base.origin.0 / scale, -base.origin.1 / scale)) - geometry;
        // ... and then from the parent's.
        let target = Rectangle::new(
            screen_loc - get_popup_toplevel_coords(&kind),
            (self.screen_px.0 / scale, self.screen_px.1 / scale).into(),
        );
        positioner.get_unconstrained_geometry(target)
    }

    /// Deliver a host input event to the focused client.
    fn input(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key { code, pressed, time_ms } => {
                let Some(keyboard) = self.seat.get_keyboard() else { return };
                let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                // xkb keycodes are evdev codes + 8.
                keyboard.input::<(), _>(self, (code + 8).into(), state, SERIAL_COUNTER.next_serial(), time_ms, |_, _, _| {
                    FilterResult::Forward
                });
            }
            InputEvent::PointerMotion { x, y, time_ms } => {
                let Some(pointer) = self.seat.get_pointer() else { return };
                // In the window's surface coordinates, where each surface is
                // placed: while a button is held, the surface under it keeps
                // the pointer (Smithay's implicit grab) wherever it goes.
                // Locations are in the bottom window's units, from its corner.
                let placed = self.placed();
                let (focus, location) = match placed.iter().find(|p| get_parent(&p.surface).is_none()) {
                    Some(base) => {
                        let scale = base.mapped.scale as f64;
                        let local = |x: f64, y: f64| ((x - base.origin.0 as f64) / scale, (y - base.origin.1 as f64) / scale);
                        let under = placed.iter().rev().find(|p| p.contains(x, y)).map(|p| {
                            let origin = local(p.origin.0 as f64, p.origin.1 as f64);
                            (p.surface.clone(), origin.into())
                        });
                        (under, local(x, y).into())
                    }
                    None => (None, (x, y).into()),
                };
                let before = pointer.current_focus();
                pointer.motion(self, focus.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: time_ms });
                // A menu's grab that has ended (an item chosen) swallows the
                // motion that finds out, re-entering where the pointer was:
                // send it again.
                if pointer.current_location() != location {
                    pointer.motion(self, focus, &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: time_ms });
                }
                pointer.frame(self);
                // Off every window: the plain arrow (a client sets its own
                // when the pointer comes in).
                if before.is_some() && pointer.current_focus().is_none() {
                    self.cursor_surface = None;
                    self.cursor = Some(Cursor::Named("default"));
                }
            }
            InputEvent::PointerButton { button, pressed, time_ms } => {
                let Some(pointer) = self.seat.get_pointer() else { return };
                if pressed {
                    self.click_focus();
                }
                let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
                pointer.button(self, &ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time: time_ms, button, state });
                pointer.frame(self);
            }
            InputEvent::PointerAxis { dx, dy, steps, finger, stop, time_ms } => {
                let Some(pointer) = self.seat.get_pointer() else { return };
                // Screen pixels to the window's units (and a touchpad's).
                let mut divisor = self.placed().iter().find(|p| get_parent(&p.surface).is_none()).map_or(1, |p| p.mapped.scale) as f64;
                if finger {
                    divisor *= FINGER_SCROLL_FACTOR;
                }
                let mut frame = AxisFrame::new(time_ms).source(if finger { AxisSource::Finger } else { AxisSource::Wheel });
                for (axis, value, steps) in [(Axis::Horizontal, dx, steps.0), (Axis::Vertical, dy, steps.1)] {
                    if value != 0.0 {
                        frame = frame.value(axis, value / divisor);
                    }
                    if steps != 0 {
                        frame = frame.v120(axis, steps);
                    }
                    if stop {
                        frame = frame.stop(axis);
                    }
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }
        }
    }
}

/// Text types for the clipboard, best first.
const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

/// Copies bigger than this are cut short on their way to the host.
const MAX_COPY: u64 = 64 << 20;

impl SelectionHandler for App {
    /// What the host copied, when the selection is its.
    type SelectionUserData = Arc<str>;

    /// A client copied: read it as text, once Smithay has it (after this).
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let offered = source.map(|s| s.mime_types()).unwrap_or_default();
        self.copy_pending = TEXT_TYPES.iter().find(|t| offered.iter().any(|o| o == *t)).map(|t| t.to_string());
    }

    /// A client pastes what the host copied.
    fn send_selection(&mut self, _ty: SelectionTarget, _mime: String, fd: OwnedFd, _seat: Seat<Self>, text: &Arc<str>) {
        // On a thread: the client reads at its own pace.
        let text = text.clone();
        std::thread::spawn(move || {
            let _ = std::fs::File::from(fd).write_all(text.as_bytes());
        });
    }
}

impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {}

impl XdgDecorationHandler for App {
    /// Server-side decorations: the shell shows the window full screen, so
    /// there are none, and the client shouldn't draw its own.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ServerSide));
        // Before the window's first commit, this goes with its first
        // configure (which is when it's known whether it's a dialog).
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        info!("client connected: {client_id:?}");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        info!("client disconnected: {client_id:?} ({reason:?})");
    }
}

/// A commit's damage in buffer pixels, clipped to the buffer: surface
/// damage is scaled by the buffer scale (no transforms here). None given
/// means all of it.
fn buffer_damage(damage: &[Damage], scale: i32, width: i32, height: i32) -> Vec<Rect> {
    let clip = |x: i32, y: i32, w: i32, h: i32| {
        let (x0, y0) = (x.clamp(0, width), y.clamp(0, height));
        let (x1, y1) = (x.saturating_add(w).clamp(0, width), y.saturating_add(h).clamp(0, height));
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    };
    let rects: Vec<Rect> = damage
        .iter()
        .filter_map(|d| match d {
            Damage::Buffer(r) => clip(r.loc.x, r.loc.y, r.size.w, r.size.h),
            Damage::Surface(r) => clip(
                r.loc.x.saturating_mul(scale),
                r.loc.y.saturating_mul(scale),
                r.size.w.saturating_mul(scale),
                r.size.h.saturating_mul(scale),
            ),
        })
        .collect();
    if damage.is_empty() { vec![Rect { x: 0, y: 0, w: width, h: height }] } else { rects }
}

fn send_frame_callbacks(surface: &wl_surface::WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

delegate_compositor!(App);
delegate_output!(App);
delegate_data_device!(App);
delegate_xdg_decoration!(App);
delegate_seat!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_cursor_shape!(App);

/// Needed for cursor shapes (which tablet tools can have too; there are
/// no tablets here).
impl TabletSeatHandler for App {}

/// Run the compositor on the current thread, forever, showing only windows
/// (not their popups) through `sink`.
pub fn run(sink: FrameSink) -> Result<(), Box<dyn std::error::Error>> {
    run_with(Box::new(sink), Options::default())
}

/// Run the compositor on the current thread, forever, as `options` say.
pub fn run_with(host: Box<dyn Host>, options: Options) -> Result<(), Box<dyn std::error::Error>> {
    // macOS has no XDG_RUNTIME_DIR; libwayland and wayland-rs both require one.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let dir = std::env::temp_dir().join("macwayland-runtime");
        std::fs::create_dir_all(&dir)?;
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    }

    let display: Display<App> = Display::new()?;
    let dh = display.handle();
    // A seat with a keyboard (US layout until the shell says otherwise) and a
    // pointer; the events come later, from the AppKit backend.
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(XkbConfig::default(), 400, 30)?;
    seat.add_pointer();
    // The screen: its mode is in pixels; windows are configured in
    // logical units (pixels / scale), and draw at the scale.
    let ((w, h), scale) = (options.screen, options.scale.max(1));
    let refresh = if options.refresh_mhz > 0 { options.refresh_mhz } else { 60_000 };
    let output = Output::new(
        "macwl-0".into(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "Apple".into(), model: "Mac display".into() },
    );
    let _output_global = output.create_global::<App>(&dh);
    let mode = Mode { size: (w, h).into(), refresh };
    output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Integer(scale)), Some((0, 0).into()));
    output.set_preferred(mode);
    let (copies, copied) = channel::channel::<String>();
    let mut state = App {
        dh: dh.clone(),
        host,
        compositor_state: CompositorState::new::<App>(&dh),
        xdg_shell_state: XdgShellState::new::<App>(&dh),
        shm_state: ShmState::new::<App>(&dh, vec![wl_shm::Format::Xrgb8888]),
        seat_state,
        seat,
        data_device_state: DataDeviceState::new::<App>(&dh),
        _decoration_state: XdgDecorationState::new::<App>(&dh),
        // MACWL_CURSOR_SHAPE=0 leaves it out: clients draw their own (a
        // test aid for that path).
        _cursor_shape_state: (std::env::var("MACWL_CURSOR_SHAPE").as_deref() != Ok("0"))
            .then(|| CursorShapeManagerState::new::<App>(&dh)),
        cursor_surface: None,
        cursor: None,
        cursor_shown: None,
        screen: (w / scale, h / scale).into(),
        screen_px: (w, h),
        windows: Vec::new(),
        popups: PopupManager::default(),
        layout: Vec::new(),
        copy_pending: None,
        copies,
        last_callbacks: Instant::now(),
        start: Instant::now(),
    };

    // Everything happens in an event loop, the moment there's something to
    // do: a client connecting or sending a request, input from the host.
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    let listener = ListeningSocketSource::with_name("wayland-mac")?;
    info!(
        "listening: XDG_RUNTIME_DIR={} WAYLAND_DISPLAY=wayland-mac",
        std::env::var("XDG_RUNTIME_DIR")?
    );
    handle.insert_source(listener, |stream, _, state| {
        if let Err(e) = state.dh.insert_client(stream, Arc::new(ClientState::default())) {
            warn!("couldn't add client: {e}");
        }
    })?;
    handle.insert_source(Generic::new(display, Interest::READ, CalloopMode::Level), |_, display, state| {
        // Safety: the display isn't dropped while the event loop holds it.
        unsafe { display.get_mut().dispatch_clients(state)? };
        Ok(PostAction::Continue)
    })?;
    handle.insert_source(copied, |event, _, state| {
        if let channel::Event::Msg(text) = event {
            state.host.copied(&text);
        }
    })?;
    if let Some(clipboard) = options.clipboard {
        handle.insert_source(clipboard, |event, _, state| {
            if let channel::Event::Msg(text) = event {
                let types = TEXT_TYPES.iter().map(|t| t.to_string()).collect();
                set_data_device_selection(&state.dh, &state.seat, types, Arc::from(text));
            }
        })?;
    }
    if let Some(input) = options.input {
        handle.insert_source(input, |event, _, state| {
            if let channel::Event::Msg(event) = event {
                state.input(event);
            }
        })?;
    }
    // Frame callbacks at each refresh of the display: clients draw at its
    // pace, each frame from the start of a refresh period, in time to be
    // shown at the next. (Sent after showing a frame instead, they wait for
    // the host's drawable: about 1.6 refreshes a frame.) Without ticks, a
    // timer.
    let interval = match options.refresh {
        Some(refresh) => {
            handle.insert_source(refresh, |event, _, state| {
                if let channel::Event::Msg(()) = event {
                    state.frame_callbacks();
                }
            })?;
            FALLBACK_INTERVAL
        }
        None => FRAME_INTERVAL,
    };
    let fallback_only = interval == FALLBACK_INTERVAL;
    handle.insert_source(Timer::from_duration(interval), move |_, _, state| {
        if !fallback_only || state.last_callbacks.elapsed() > MAX_WAIT {
            state.frame_callbacks();
        }
        TimeoutAction::ToDuration(interval)
    })?;
    loop {
        event_loop.dispatch(None, &mut state)?;
        state.relayout();
        state.read_copy();
        state.show_cursor();
        state.dh.flush_clients()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Rectangle;

    #[test]
    fn damage_in_buffer_pixels() {
        let r = |x, y, w, h| Rect { x, y, w, h };
        // None given: all of it.
        assert_eq!(buffer_damage(&[], 2, 100, 50), [r(0, 0, 100, 50)]);
        // Surface damage scales with the buffer; both kinds clip to it.
        let damage = [
            Damage::Surface(Rectangle::new((10, 5).into(), (4, 3).into())),
            Damage::Buffer(Rectangle::new((90, 40).into(), (20, 20).into())),
            Damage::Buffer(Rectangle::new((200, 0).into(), (5, 5).into())),
            Damage::Surface(Rectangle::new((0, 0).into(), (i32::MAX, i32::MAX).into())),
        ];
        assert_eq!(buffer_damage(&damage, 2, 100, 50), [r(20, 10, 8, 6), r(90, 40, 10, 10), r(0, 0, 100, 50)]);
    }
}
