//! Spike 1: full-screen Metal shell.
//!
//! `macwl-shell --latency`  black screen; any key/click flips it white. Each flip
//!                          logs input-event → on-glass latency using the event's
//!                          hardware timestamp and MTLDrawable.presentedTime.
//! `macwl-shell`            hosts the compositor and draws the window it shows
//!                          (centred) with its popups over it. Keys, mouse and
//!                          trackpad go to the client. Each key press logs
//!                          key → on-glass latency of the client's answer (its
//!                          next frame committed after the key).
//!
//! `--auto N`              no input needed: a background thread fires N fake
//!                          "input events" at random times and the app prints a
//!                          latency summary (event → on-glass). Combine with
//!                          `--latency`.
//!
//! Input is handled by an NSEvent local monitor the moment AppKit delivers it.
//! Pacing (MACWL_PACING):
//!   demand (default)  render and present immediately on input or client commit;
//!                     matches native latency (see native-probe)
//!   link              render on CAMetalDisplayLink ticks; ~1 frame slower
//!
//! Frame callbacks (MACWL_FRAME_CALLBACKS):
//!   refresh (default) at each refresh of the display (a CADisplayLink on its
//!                     own thread): clients draw once a refresh
//!   timer             every 8 ms, regardless
//!
//! MACWL_AUTO_KEYS=N types N keys into the client (alternately `a` and
//! Backspace, so a shell's line stays empty), then prints a latency summary
//! and quits.
//!
//! MACWL_SAMPLES=1 prints every sample in `--auto` runs too.
//!
//! MACWL_LOG_SCROLL=1 prints each trackpad event as macOS has it.
//!
//! Tuning knobs (env): MACWL_FRAME_LATENCY=<float frames, default 1; link only>,
//! MACWL_DRAWABLES=2|3, MACWL_VSYNC=1|0, MACWL_FBONLY=1|0.
//!
//! Esc quits.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
    },
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSEventMask,
    NSBitmapFormat, NSBitmapImageRep, NSCursor, NSCursorFrameResizeDirections, NSCursorFrameResizePosition,
    NSDeviceRGBColorSpace, NSEventPhase, NSEventType, NSImage, NSPasteboard, NSPasteboardTypeString, NSScreen,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use macwl_compositor::{BufferData, Cursor, InputEvent, Layer, Options, Rect};
use objc2_foundation::{NSDefaultRunLoopMode, NSRunLoop, NSRunLoopCommonModes, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};
use objc2_quartz_core::{
    CACurrentMediaTime, CADisplayLink, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate,
    CAMetalDrawable, CAMetalLayer,
};

const KEY_ESCAPE: u16 = 53;

/// A client surface (a window or popup), kept up to date by the compositor
/// thread (which copies in only what each commit changed) and uploaded by
/// the render loop (only what changed since its last upload).
struct ClientFrame {
    width: usize,
    height: usize,
    /// Whether its alpha is to be ignored (XRGB).
    opaque: bool,
    /// Bytes per row of `pixels` (tightly packed BGRA).
    stride: usize,
    pixels: Vec<u8>,
    seq: u64,
    /// When the compositor handed over the latest commit (`CACurrentMediaTime`).
    committed: f64,
    /// What changed since the last upload: all of it, for a new texture.
    dirty: Option<Rect>,
}

impl ClientFrame {
    fn new(width: usize, height: usize, opaque: bool) -> Self {
        ClientFrame { width, height, opaque, stride: width * 4, pixels: vec![0; width * 4 * height], seq: 0, committed: 0.0, dirty: None }
    }

    /// Copy `r` from a buffer (`pixels` at `offset`, `stride` bytes a row).
    fn copy_in(&mut self, pixels: &[u8], offset: usize, stride: usize, r: Rect) {
        let (x, w) = (r.x as usize * 4, r.w as usize * 4);
        for row in r.y as usize..(r.y + r.h) as usize {
            let (src, dst) = (offset + row * stride + x, row * self.stride + x);
            let (Some(from), Some(to)) = (pixels.get(src..src + w), self.pixels.get_mut(dst..dst + w)) else { return };
            to.copy_from_slice(from);
        }
        self.dirty = Some(match self.dirty {
            None => r,
            Some(d) => {
                let (x0, y0) = (d.x.min(r.x), d.y.min(r.y));
                let (x1, y1) = ((d.x + d.w).max(r.x + r.w), (d.y + d.h).max(r.y + r.h));
                Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
            }
        });
    }
}

/// What the compositor shows: each surface's latest frame, and which are on
/// screen, where.
#[derive(Default)]
struct Scene {
    surfaces: HashMap<u32, ClientFrame>,
    /// Bottom to top.
    layers: Vec<Layer>,
    /// Bumped whenever anything changes.
    seq: u64,
}

/// The compositor's [`macwl_compositor::Host`]: hands frames and layout to
/// the render loop.
struct ShellHost(Arc<Mutex<Scene>>);

impl macwl_compositor::Host for ShellHost {
    fn frame(&mut self, id: u32, pixels: &[u8], data: &BufferData, damage: &[Rect]) {
        let (w, h) = (data.width as usize, data.height as usize);
        let opaque = data.format != macwl_compositor::ShmFormat::Argb8888;
        let mut scene = self.0.lock().unwrap();
        scene.seq += 1;
        let seq = scene.seq;
        // A new size: start over, with all of it.
        let existing = scene.surfaces.get_mut(&id).filter(|f| f.width == w && f.height == h && f.opaque == opaque);
        let fresh = existing.is_none();
        let frame = match existing {
            Some(frame) => frame,
            None => scene.surfaces.entry(id).insert_entry(ClientFrame::new(w, h, opaque)).into_mut(),
        };
        let all = [Rect { x: 0, y: 0, w: data.width, h: data.height }];
        for &r in if fresh { &all[..] } else { damage } {
            frame.copy_in(pixels, data.offset as usize, data.stride as usize, r);
        }
        frame.seq = seq;
        frame.committed = CACurrentMediaTime();
        drop(scene);
        request_render();
    }

    /// The client under the pointer changed its cursor.
    fn cursor(&mut self, cursor: Cursor) {
        DispatchQueue::main().exec_async(move || {
            let cursor = ns_cursor(&cursor);
            cursor.set();
            CURSOR.with_borrow_mut(|c| *c = Some(cursor));
        });
    }

    /// A client copied: onto the Mac's clipboard.
    fn copied(&mut self, text: &str) {
        let text = text.to_string();
        DispatchQueue::main().exec_async(move || {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            pasteboard.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
            PASTEBOARD_OURS.store(pasteboard.changeCount(), Ordering::Relaxed);
        });
    }

    fn layout(&mut self, layers: &[Layer]) {
        let mut scene = self.0.lock().unwrap();
        scene.surfaces.retain(|id, _| layers.iter().any(|l| l.id == *id));
        scene.layers = layers.to_vec();
        scene.seq += 1;
        drop(scene);
        request_render();
    }
}

thread_local! {
    /// The cursor clients asked for: set again as the pointer moves, should
    /// AppKit have put its own back.
    static CURSOR: RefCell<Option<Retained<NSCursor>>> = const { RefCell::new(None) };
}

/// A client's cursor as a Mac one.
fn ns_cursor(cursor: &Cursor) -> Retained<NSCursor> {
    match cursor {
        Cursor::Named(name) => named_cursor(name),
        // An invisible image, rather than `NSCursor::hide`, which has to be
        // balanced.
        Cursor::Hidden => image_cursor(&[0; 4], 1, 1, 1, (0, 0)),
        Cursor::Image { pixels, width, height, scale, hotspot } => image_cursor(pixels, *width, *height, *scale, *hotspot),
    }
}

/// The Mac cursor for a CSS cursor name (the arrow where there's none,
/// such as "wait": macOS shows its own when an app is busy).
fn named_cursor(name: &str) -> Retained<NSCursor> {
    use NSCursorFrameResizePosition as Edge;
    let resize = |edge| NSCursor::frameResizeCursorFromPosition_inDirections(edge, NSCursorFrameResizeDirections::All);
    match name {
        "text" => NSCursor::IBeamCursor(),
        "vertical-text" => NSCursor::IBeamCursorForVerticalLayout(),
        "pointer" => NSCursor::pointingHandCursor(),
        "grab" | "move" | "all-scroll" | "all-resize" => NSCursor::openHandCursor(),
        "grabbing" => NSCursor::closedHandCursor(),
        "crosshair" | "cell" => NSCursor::crosshairCursor(),
        "not-allowed" | "no-drop" => NSCursor::operationNotAllowedCursor(),
        "copy" => NSCursor::dragCopyCursor(),
        "alias" => NSCursor::dragLinkCursor(),
        "context-menu" => NSCursor::contextualMenuCursor(),
        "zoom-in" => NSCursor::zoomInCursor(),
        "zoom-out" => NSCursor::zoomOutCursor(),
        "col-resize" => NSCursor::columnResizeCursor(),
        "row-resize" => NSCursor::rowResizeCursor(),
        "n-resize" | "ns-resize" => resize(Edge::Top),
        "s-resize" => resize(Edge::Bottom),
        "e-resize" => resize(Edge::Right),
        "w-resize" | "ew-resize" => resize(Edge::Left),
        "ne-resize" | "nesw-resize" => resize(Edge::TopRight),
        "nw-resize" | "nwse-resize" => resize(Edge::TopLeft),
        "se-resize" => resize(Edge::BottomRight),
        "sw-resize" => resize(Edge::BottomLeft),
        _ => NSCursor::arrowCursor(),
    }
}

/// A cursor from premultiplied BGRA pixels at `scale` pixels a point, with
/// its hotspot in points (the arrow, should AppKit not take them).
fn image_cursor(pixels: &[u8], width: i32, height: i32, scale: i32, hotspot: (i32, i32)) -> Retained<NSCursor> {
    let (w, h) = (width as isize, height as isize);
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            // B, G, R, A in memory, premultiplied: as wl_shm's ARGB8888.
            NSBitmapFormat::AlphaFirst | NSBitmapFormat::ThirtyTwoBitLittleEndian,
            w * 4,
            32,
        )
    };
    let Some(rep) = rep.filter(|_| pixels.len() == (w * h * 4) as usize) else { return NSCursor::arrowCursor() };
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), rep.bitmapData(), pixels.len()) };
    let scale = scale.max(1) as f64;
    let size = objc2_foundation::NSSize { width: width as f64 / scale, height: height as f64 / scale };
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.addRepresentation(&rep);
    let hotspot = objc2_foundation::NSPoint { x: hotspot.0 as f64, y: hotspot.1 as f64 };
    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hotspot)
}

/// The Mac clipboard's `changeCount` after the shell last wrote to it (a
/// client's copy), so it isn't handed back as a Mac copy.
static PASTEBOARD_OURS: AtomicIsize = AtomicIsize::new(-1);
/// How often to look for a copy on the Mac (which says nothing when one
/// happens).
const PASTEBOARD_POLL: Duration = Duration::from_millis(250);

/// Hand text copied on the Mac to the compositor, now (what's there when
/// the session starts) and whenever it changes after `seen`.
fn watch_pasteboard(clipboard: macwl_compositor::ClipboardSender, seen: isize) {
    let pasteboard = NSPasteboard::generalPasteboard();
    let count = pasteboard.changeCount();
    if count != seen
        && count != PASTEBOARD_OURS.load(Ordering::Relaxed)
        && let Some(text) = pasteboard.stringForType(unsafe { NSPasteboardTypeString })
    {
        let _ = clipboard.send(text.to_string());
    }
    let when = DispatchTime::try_from(PASTEBOARD_POLL).unwrap();
    let _ = DispatchQueue::main().after(when, move || watch_pasteboard(clipboard, count));
}

/// Draws popups over the window: a textured quad, blended (premultiplied
/// alpha, as wl_shm's ARGB is).
const QUAD_SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Quad { float2 origin; float2 size; float2 screen; uint opaque; };
struct Vertex { float4 position [[position]]; float2 pixel; };

vertex Vertex quad_vertex(uint id [[vertex_id]], constant Quad &q [[buffer(0)]]) {
    float2 corner = float2(id & 1, id >> 1);
    float2 p = q.origin + corner * q.size;
    Vertex v;
    v.position = float4(p.x / q.screen.x * 2 - 1, 1 - p.y / q.screen.y * 2, 0, 1);
    v.pixel = corner * q.size;
    return v;
}

fragment float4 quad_fragment(Vertex v [[stage_in]], constant Quad &q [[buffer(0)]],
                              texture2d<float> tex [[texture(0)]]) {
    float4 c = tex.read(uint2(v.pixel));
    if (q.opaque) c.a = 1;
    return c;
}
"#;

/// [`QUAD_SHADER`]'s `Quad`, in pixels.
#[repr(C)]
struct Quad {
    origin: [f32; 2],
    size: [f32; 2],
    screen: [f32; 2],
    opaque: u32,
    _pad: u32,
}

fn quad_pipeline(device: &ProtocolObject<dyn MTLDevice>) -> Retained<ProtocolObject<dyn MTLRenderPipelineState>> {
    let library = device.newLibraryWithSource_options_error(&NSString::from_str(QUAD_SHADER), None).expect("quad shader");
    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(library.newFunctionWithName(&NSString::from_str("quad_vertex")).as_deref());
    desc.setFragmentFunction(library.newFunctionWithName(&NSString::from_str("quad_fragment")).as_deref());
    let color = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    color.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    color.setBlendingEnabled(true);
    color.setSourceRGBBlendFactor(MTLBlendFactor::One);
    color.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    color.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    color.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    device.newRenderPipelineStateWithDescriptor_error(&desc).expect("quad pipeline")
}

#[derive(Default)]
struct LatencyStats {
    samples: Vec<f64>,
    quiet: bool,
}

impl LatencyStats {
    fn sorted(&self) -> Vec<f64> {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted
    }

    fn push(&mut self, input_to_app_ms: f64, input_to_glass_ms: f64) {
        self.push_labelled("flip", "input→app", input_to_app_ms, "input→glass", input_to_glass_ms);
    }

    fn push_labelled(&mut self, what: &str, first: &str, first_ms: f64, second: &str, glass_ms: f64) {
        self.samples.push(glass_ms);
        if self.quiet {
            return;
        }
        let sorted = self.sorted();
        let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
        println!(
            "{what} #{:<3} {first} {:5.2} ms   {second} {:6.2} ms   (n={} p50={:.2} p90={:.2} min={:.2})",
            self.samples.len(),
            first_ms,
            glass_ms,
            sorted.len(),
            pct(0.5),
            pct(0.9),
            sorted[0],
        );
    }
}

/// Input-side state, written by the event monitor and read by the frame callback.
#[derive(Default)]
struct InputState {
    white: bool,
    /// (event timestamp, time the app saw it) for the next frame to report on.
    pending: Option<(f64, f64)>,
}

struct Auto {
    samples: usize,
    /// Fake event timestamps from the trigger thread.
    trigger: Arc<Mutex<Option<f64>>>,
    shade: bool,
    start: f64,
}

struct Renderer {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn objc2_metal::MTLCommandQueue>>,
    latency_mode: bool,
    scene: Arc<Mutex<Scene>>,
    /// Each surface's texture, the frame it holds, and whether it's opaque.
    textures: HashMap<u32, (Texture, u64, bool)>,
    scene_seq: u64,
    /// What's on screen, bottom to top, as last drawn.
    layers: Vec<Layer>,
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    stats: Arc<Mutex<LatencyStats>>,
    input: Rc<RefCell<InputState>>,
    auto: Option<Auto>,
    config: String,
    /// The last key press not yet answered by a client frame: (event time,
    /// when the shell saw it).
    key_pending: Arc<Mutex<Option<(f64, f64)>>>,
    /// Stop after this many key → glass samples (MACWL_AUTO_KEYS).
    auto_keys: Option<usize>,
}

impl Renderer {
    /// Render and present right now (on-demand pacing).
    fn render_now(&mut self) {
        if let Some(drawable) = self.layer.nextDrawable() {
            self.frame(&drawable);
        }
    }

    fn frame(&mut self, drawable: &ProtocolObject<dyn CAMetalDrawable>) {
        let auto_shade = self.auto.as_mut().map(|auto| {
            if let Some(t) = auto.trigger.lock().unwrap().take() {
                auto.shade = !auto.shade;
                self.input.borrow_mut().pending = Some((t, t));
            }
            auto.shade
        });
        self.check_auto_done();

        let target = drawable.texture();
        let cmd = self.queue.commandBuffer().expect("no command buffer");

        // Clear pass: black/white in latency mode, dark grey behind clients. Auto
        // mode toggles between two near-identical greys so it doesn't strobe.
        let white = self.input.borrow().white;
        let shade = match auto_shade {
            Some(true) => 0.12,
            Some(false) => 0.10,
            None if white => 1.0,
            None if self.latency_mode => 0.0,
            None => 0.08,
        };
        let rpd = MTLRenderPassDescriptor::new();
        let attachment = unsafe { rpd.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor { red: shade, green: shade, blue: shade, alpha: 1.0 });
        cmd.renderCommandEncoderWithDescriptor(&rpd).expect("no encoder").endEncoding();

        let new_client_frame = if self.latency_mode { None } else { self.draw_scene(&cmd, &target) };

        let mtl_drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(drawable);
        // A client frame committed after a key press: the client's answer.
        let answered = new_client_frame.and_then(|committed| {
            let mut pending = self.key_pending.lock().unwrap();
            if pending.is_some_and(|(_, seen)| committed > seen) { pending.take().map(|p| (p, committed)) } else { None }
        });
        if let Some(((key_ts, _), committed)) = answered {
            let stats = self.stats.clone();
            let handler = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                let glass = unsafe { d.as_ref().presentedTime() };
                if glass > 0.0 {
                    stats.lock().unwrap().push_labelled("key", "key→commit", (committed - key_ts) * 1e3, "key→glass", (glass - key_ts) * 1e3);
                }
            });
            unsafe { mtl_drawable.addPresentedHandler(RcBlock::as_ptr(&handler)) };
        }
        if let Some((event_ts, app_ts)) = self.input.borrow_mut().pending.take() {
            let stats = self.stats.clone();
            let handler = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                let glass = unsafe { d.as_ref().presentedTime() };
                if glass > 0.0 {
                    stats.lock().unwrap().push((app_ts - event_ts) * 1e3, (glass - event_ts) * 1e3);
                }
            });
            unsafe { mtl_drawable.addPresentedHandler(RcBlock::as_ptr(&handler)) };
        }
        cmd.presentDrawable(mtl_drawable);
        cmd.commit();
        self.check_auto_keys_done();
    }

    fn check_auto_keys_done(&self) {
        let Some(n) = self.auto_keys else { return };
        let stats = self.stats.lock().unwrap();
        if stats.samples.len() < n {
            return;
        }
        let s = stats.sorted();
        let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
        println!(
            "{}: n={} key→glass p50={:.2} p90={:.2} min={:.2} max={:.2} ms",
            self.config,
            s.len(),
            pct(0.5),
            pct(0.9),
            s[0],
            s[s.len() - 1]
        );
        std::process::exit(0);
    }

    fn check_auto_done(&self) {
        let Some(auto) = &self.auto else { return };
        let stats = self.stats.lock().unwrap();
        let timed_out = CACurrentMediaTime() - auto.start > 2.5 + auto.samples as f64 * 0.5 + 5.0;
        if stats.samples.len() < auto.samples && !timed_out {
            return;
        }
        let s = stats.sorted();
        if s.is_empty() {
            println!("{}: no samples (presentedTime never reported)", self.config);
        } else {
            let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
            println!(
                "{}: n={} event→glass p50={:.2} p90={:.2} min={:.2} max={:.2} ms",
                self.config,
                s.len(),
                pct(0.5),
                pct(0.9),
                s[0],
                s[s.len() - 1]
            );
        }
        std::process::exit(0);
    }

    /// Upload what changed in the client's surfaces, and draw them: the
    /// bottom one (the window) copied in (the fast path, and all there
    /// usually is), the rest (subsurfaces, popups) blended over it. Returns when a new frame was committed, if there
    /// was one.
    fn draw_scene(
        &mut self,
        cmd: &ProtocolObject<dyn MTLCommandBuffer>,
        target: &ProtocolObject<dyn MTLTexture>,
    ) -> Option<f64> {
        let mut new_frame = None;
        {
            let mut scene = self.scene.lock().unwrap();
            if scene.seq != self.scene_seq {
                self.scene_seq = scene.seq;
                self.layers = scene.layers.clone();
                self.textures.retain(|id, _| scene.surfaces.contains_key(id));
                for (&id, frame) in scene.surfaces.iter_mut() {
                    if self.textures.get(&id).is_some_and(|&(_, seq, _)| seq == frame.seq) {
                        continue;
                    }
                    new_frame = Some(new_frame.map_or(frame.committed, |t: f64| t.max(frame.committed)));
                    let tex = upload(&self.device, self.textures.get(&id).map(|(t, ..)| t), frame);
                    self.textures.insert(id, (tex, frame.seq, frame.opaque));
                }
            }
        }
        let (tw, th) = (target.width() as i32, target.height() as i32);
        let mut layers = self.layers.iter().filter_map(|l| self.textures.get(&l.id).map(|(t, _, opaque)| (l, t, *opaque)));
        if let Some((window, tex, _)) = layers.next() {
            // The part on screen.
            let (x0, y0) = (window.x.max(0), window.y.max(0));
            let x1 = (window.x + tex.width() as i32).min(tw);
            let y1 = (window.y + tex.height() as i32).min(th);
            if x1 > x0 && y1 > y0 {
                let blit = cmd.blitCommandEncoder().expect("no blit encoder");
                unsafe {
                    blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                        tex,
                        0,
                        0,
                        MTLOrigin { x: (x0 - window.x) as usize, y: (y0 - window.y) as usize, z: 0 },
                        MTLSize { width: (x1 - x0) as usize, height: (y1 - y0) as usize, depth: 1 },
                        target,
                        0,
                        0,
                        MTLOrigin { x: x0 as usize, y: y0 as usize, z: 0 },
                    )
                };
                blit.endEncoding();
            }
        }
        let popups: Vec<_> = layers.collect();
        if !popups.is_empty() {
            let rpd = MTLRenderPassDescriptor::new();
            let attachment = unsafe { rpd.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(target));
            attachment.setLoadAction(MTLLoadAction::Load);
            attachment.setStoreAction(MTLStoreAction::Store);
            let encoder = cmd.renderCommandEncoderWithDescriptor(&rpd).expect("no encoder");
            encoder.setRenderPipelineState(&self.quad);
            for (layer, tex, opaque) in popups {
                let quad = Quad {
                    origin: [layer.x as f32, layer.y as f32],
                    size: [tex.width() as f32, tex.height() as f32],
                    screen: [tw as f32, th as f32],
                    opaque: opaque as u32,
                    _pad: 0,
                };
                let bytes = NonNull::from(&quad).cast::<c_void>();
                unsafe {
                    encoder.setVertexBytes_length_atIndex(bytes, size_of::<Quad>(), 0);
                    encoder.setFragmentBytes_length_atIndex(bytes, size_of::<Quad>(), 0);
                    encoder.setFragmentTexture_atIndex(Some(tex), 0);
                    encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
                }
            }
            encoder.endEncoding();
        }
        new_frame
    }
}

type Texture = Retained<ProtocolObject<dyn MTLTexture>>;

/// Copy what changed in `frame` into its texture (`existing`, or a new one
/// with all of it when there's none or the size changed).
fn upload(
    device: &ProtocolObject<dyn MTLDevice>,
    existing: Option<&Texture>,
    frame: &mut ClientFrame,
) -> Texture {
    let full = Rect { x: 0, y: 0, w: frame.width as i32, h: frame.height as i32 };
    let mut dirty = frame.dirty.take();
    let tex = match existing {
        Some(t) if t.width() == frame.width && t.height() == frame.height => t.clone(),
        _ => {
            dirty = Some(full);
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    frame.width,
                    frame.height,
                    false,
                )
            };
            desc.setStorageMode(MTLStorageMode::Shared);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            device.newTextureWithDescriptor(&desc).expect("texture alloc")
        }
    };
    if let Some(r) = dirty {
        let region = MTLRegion {
            origin: MTLOrigin { x: r.x as usize, y: r.y as usize, z: 0 },
            size: MTLSize { width: r.w as usize, height: r.h as usize, depth: 1 },
        };
        let start = r.y as usize * frame.stride + r.x as usize * 4;
        unsafe {
            tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(frame.pixels.as_mut_ptr().add(start).cast()).unwrap(),
                frame.stride,
            )
        };
    }
    tex
}

thread_local! {
    /// The renderer lives on the main thread; other threads reach it via
    /// [`request_render`].
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
}

/// True when MACWL_PACING=demand.
static ON_DEMAND: AtomicBool = AtomicBool::new(true);
/// A render is already queued on the main queue; further requests coalesce.
static RENDER_QUEUED: AtomicBool = AtomicBool::new(false);

fn with_renderer(f: impl FnOnce(&mut Renderer)) {
    RENDERER.with_borrow_mut(|r| f(r.as_mut().expect("renderer not installed")));
}

/// Ask for an immediate frame from any thread (no-op under display-link pacing).
fn request_render() {
    if ON_DEMAND.load(Ordering::Relaxed) && !RENDER_QUEUED.swap(true, Ordering::AcqRel) {
        DispatchQueue::main().exec_async(|| {
            RENDER_QUEUED.store(false, Ordering::Release);
            with_renderer(Renderer::render_now);
        });
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MacwlDisplayLinkDelegate"]
    struct LinkDelegate;

    unsafe impl NSObjectProtocol for LinkDelegate {}

    unsafe impl CAMetalDisplayLinkDelegate for LinkDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn needs_update(&self, _link: &CAMetalDisplayLink, update: &CAMetalDisplayLinkUpdate) {
            with_renderer(|r| r.frame(&update.drawable()));
        }
    }
);

impl LinkDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

define_class!(
    /// Passes the display's refresh ticks to the compositor.
    #[unsafe(super(NSObject))]
    #[name = "MacwlRefreshTicker"]
    #[ivars = macwl_compositor::RefreshSender]
    struct Ticker;

    impl Ticker {
        #[unsafe(method(tick:))]
        fn tick(&self, _link: &CADisplayLink) {
            let _ = self.ivars().send(());
        }
    }
);

impl Ticker {
    fn new(sender: macwl_compositor::RefreshSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(sender);
        unsafe { msg_send![super(this), init] }
    }
}

/// Send `refresh` a tick at each refresh of `screen`, from a thread of its
/// own (so a busy main thread can't hold them up).
fn start_refresh_ticks(screen: &NSScreen, refresh: macwl_compositor::RefreshSender) {
    let ticker = Ticker::new(refresh);
    let link = unsafe { screen.displayLinkWithTarget_selector(&ticker, sel!(tick:)) };
    // A display link may run on any thread's run loop; it's made here
    // because NSScreen lives on the main thread. (It keeps its target.)
    struct Moved(Retained<CADisplayLink>);
    unsafe impl Send for Moved {}
    let moved = Moved(link);
    std::thread::Builder::new()
        .name("refresh-ticks".into())
        .spawn(move || {
            let moved = moved;
            let run_loop = NSRunLoop::currentRunLoop();
            unsafe { moved.0.addToRunLoop_forMode(&run_loop, NSDefaultRunLoopMode) };
            run_loop.run();
        })
        .expect("refresh tick thread");
}

/// Set once the window is full screen (and its transition has had time to
/// finish); the `--auto` benchmarks wait for it.
static FULL_SCREEN: AtomicBool = AtomicBool::new(false);

/// Make `window` full screen, asking again until it is: launched while
/// another full-screen window's exit animation runs (a previous run that
/// just quit), macOS drops the request and leaves an ordinary window, which
/// WindowServer composites: two refreshes slower at 120 Hz.
fn enter_full_screen(window: Retained<NSWindow>, mtm: MainThreadMarker) {
    window.toggleFullScreen(None);
    check_full_screen(MainThreadBound::new(window, mtm), 0);
}

fn check_full_screen(window: MainThreadBound<Retained<NSWindow>>, checks: u32) {
    let when = DispatchTime::try_from(Duration::from_millis(500)).unwrap();
    let _ = DispatchQueue::main().after(when, move || {
        let mtm = MainThreadMarker::new().unwrap();
        let w = window.get(mtm);
        if w.styleMask().contains(NSWindowStyleMask::FullScreen) {
            // The transition takes about a second once it has started.
            if checks >= 2 {
                FULL_SCREEN.store(true, Ordering::Release);
                return;
            }
        } else if checks < 20 {
            w.toggleFullScreen(None);
        } else {
            eprintln!("note: window never went full screen");
            FULL_SCREEN.store(true, Ordering::Release);
            return;
        }
        check_full_screen(window, checks + 1);
    });
}

fn wait_for_full_screen() {
    while !FULL_SCREEN.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kVK_ANSI_Q`, and `NSEventModifierFlagCommand`.
const KEY_Q: u16 = 0x0c;
const COMMAND: u64 = 1 << 20;
const CAPS_LOCK_KEY: u16 = 0x39;

/// Send a key event on to the compositor, in Linux terms: key downs and ups
/// (not AppKit's repeats: clients repeat keys themselves, as on Linux), and
/// modifier changes as presses and releases of those keys.
fn forward_key(keys: &macwl_compositor::InputSender, event: &NSEvent, ty: NSEventType, flags: u64) {
    let time_ms = (event.timestamp() * 1000.0) as u32;
    let send = |mac: u16, pressed: bool| {
        if let Some(code) = macwl_compositor::keymap::evdev_from_mac(mac) {
            let _ = keys.send(InputEvent::Key { code, pressed, time_ms });
        }
    };
    if (ty == NSEventType::KeyDown && !event.isARepeat()) || ty == NSEventType::KeyUp {
        send(event.keyCode(), ty == NSEventType::KeyDown);
    } else if ty == NSEventType::FlagsChanged {
        let key = event.keyCode();
        if key == CAPS_LOCK_KEY {
            // macOS reports Caps Lock's state, not the key: it toggles
            // whenever this arrives, as a tap does on Linux.
            send(key, true);
            send(key, false);
        } else if let Some(&(_, bit)) = macwl_compositor::keymap::MODIFIERS.iter().find(|&&(k, _)| k == key) {
            send(key, flags & bit != 0);
        }
    }
}

/// Bumped at each trackpad scroll event: a lift's stop goes out only if
/// nothing has happened since.
static SCROLL_ACTIVITY: AtomicU64 = AtomicU64::new(0);
/// MACWL_LOG_SCROLL=1: print each trackpad event as macOS has it.
static LOG_SCROLL: AtomicBool = AtomicBool::new(false);
/// How long after a lift to wait for macOS's glide before saying scrolling
/// stopped.
const SCROLL_MOMENTUM_WAIT: Duration = Duration::from_millis(50);
/// How long a stop says the fingers have been still: longer than toolkits
/// look back for the speed to glide at (GTK: 150 ms).
const SCROLL_STILL_MS: u32 = 200;

/// Mouse buttons in AppKit's order (`buttonNumber`), as evdev codes: left,
/// right, middle, back, forward.
const BUTTONS: [u32; 5] = [macwl_compositor::BTN_LEFT, macwl_compositor::BTN_RIGHT, macwl_compositor::BTN_MIDDLE, 0x113, 0x114];

/// Send a mouse or trackpad event on to the compositor, in Linux terms:
/// where the pointer is in screen pixels from the top left, buttons, and
/// scrolling. The screen is `layer`'s drawable, which AppKit fits to `view`
/// (a little shorter than the screen, below the notch, on MacBooks).
fn forward_mouse(keys: &macwl_compositor::InputSender, event: &NSEvent, ty: NSEventType, view: &NSView, layer: &CAMetalLayer) {
    let scale = layer.contentsScale();
    let time_ms = (event.timestamp() * 1000.0) as u32;
    match ty {
        NSEventType::MouseMoved
        | NSEventType::LeftMouseDragged
        | NSEventType::RightMouseDragged
        | NSEventType::OtherMouseDragged => {
            let p = view.convertPoint_fromView(event.locationInWindow(), None);
            let (bounds, drawable) = (view.bounds().size, layer.drawableSize());
            let x = p.x / bounds.width * drawable.width;
            let y = (bounds.height - p.y) / bounds.height * drawable.height;
            CURSOR.with_borrow(|cursor| cursor.as_ref().map(|c| c.set()));
            let _ = keys.send(InputEvent::PointerMotion { x, y, time_ms });
        }
        NSEventType::LeftMouseDown
        | NSEventType::RightMouseDown
        | NSEventType::OtherMouseDown
        | NSEventType::LeftMouseUp
        | NSEventType::RightMouseUp
        | NSEventType::OtherMouseUp => {
            let pressed = matches!(ty, NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown);
            if let Some(&button) = BUTTONS.get(event.buttonNumber() as usize) {
                let _ = keys.send(InputEvent::PointerButton { button, pressed, time_ms });
            }
        }
        NSEventType::ScrollWheel => {
            // AppKit's deltas are positive for up and left (after the
            // natural scrolling setting); Wayland's for down and right.
            let (dx, dy) = (-event.scrollingDeltaX(), -event.scrollingDeltaY());
            if !event.hasPreciseScrollingDeltas() {
                // A wheel: lines, a click each (more when spun fast). Linux
                // scrolls 15 units a click.
                let steps = ((dx * 120.0).round() as i32, (dy * 120.0).round() as i32);
                let (dx, dy) = (dx * 15.0 * scale, dy * 15.0 * scale);
                let _ = keys.send(InputEvent::PointerAxis { dx, dy, steps, finger: false, stop: false, time_ms });
                return;
            }
            // A trackpad (or Magic Mouse), in points: the fingers' scrolling,
            // then macOS's own momentum after a flick, as the client sees
            // it in Mac apps. It's all finger scrolling to the client.
            let (phase, momentum) = (event.phase(), event.momentumPhase());
            if LOG_SCROLL.load(Ordering::Relaxed) {
                eprintln!("scroll t={:.3} phase={:#x} momentum={:#x} dx={dx:.2} dy={dy:.2}", event.timestamp(), phase.0, momentum.0);
            }
            // Anything new: a stop waiting on a lift isn't the end.
            let activity = SCROLL_ACTIVITY.fetch_add(1, Ordering::Relaxed) + 1;
            if dx != 0.0 || dy != 0.0 {
                let (dx, dy) = (dx * scale, dy * scale);
                let _ = keys.send(InputEvent::PointerAxis { dx, dy, steps: (0, 0), finger: true, stop: false, time_ms });
            }
            // The client hears scrolling stopped when macOS's glide ends (or
            // when the fingers lift and none starts): that ends its gesture
            // (GTK lets content overshoot an end until then, which the next
            // flick would have to undo first). It's stamped as if the
            // fingers had then been still a while, so the client has no
            // speed to glide on with (GTK takes that of the last 150 ms):
            // the only glide is macOS's.
            let stop = InputEvent::PointerAxis {
                dx: 0.0,
                dy: 0.0,
                steps: (0, 0),
                finger: true,
                stop: true,
                time_ms: time_ms.wrapping_add(SCROLL_STILL_MS),
            };
            let ended = NSEventPhase::Ended | NSEventPhase::Cancelled;
            if momentum.intersects(ended) {
                let _ = keys.send(stop);
            } else if phase.intersects(ended) {
                // macOS starts the glide, if any, within a frame or so.
                let keys = keys.clone();
                let when = DispatchTime::try_from(SCROLL_MOMENTUM_WAIT).unwrap();
                let _ = DispatchQueue::main().after(when, move || {
                    if SCROLL_ACTIVITY.load(Ordering::Relaxed) == activity {
                        let _ = keys.send(stop);
                    }
                });
            }
        }
        _ => {}
    }
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name).map(|v| v != "0").unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let latency_mode = args.iter().any(|a| a == "--latency");
    let auto_samples: Option<usize> = args
        .iter()
        .position(|a| a == "--auto")
        .map(|i| args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(40));
    let frame_latency: f32 =
        std::env::var("MACWL_FRAME_LATENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let drawables: usize = std::env::var("MACWL_DRAWABLES").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let vsync = env_flag("MACWL_VSYNC", true);
    let fb_only = env_flag("MACWL_FBONLY", latency_mode);
    let on_demand = std::env::var("MACWL_PACING").map(|v| v != "link").unwrap_or(true);
    ON_DEMAND.store(on_demand, Ordering::Relaxed);
    LOG_SCROLL.store(env_flag("MACWL_LOG_SCROLL", false), Ordering::Relaxed);
    if !latency_mode && fb_only {
        eprintln!("MACWL_FBONLY=1 needs --latency (compositor mode blits into the drawable)");
        return;
    }
    let mtm = MainThreadMarker::new().expect("must run on the main thread");

    let scene: Arc<Mutex<Scene>> = Arc::default();

    // --- AppKit: one full-screen window backed by a CAMetalLayer -----------------
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let screen = NSScreen::mainScreen(mtm).expect("no screen");

    // The compositor, on its own thread: told the screen's size in pixels and
    // its scale, and fed the keyboard and mouse through `keys`.
    let (keys, key_events) = macwl_compositor::input_channel();
    let (clipboard, clipboard_events) = macwl_compositor::clipboard_channel();
    if !latency_mode {
        watch_pasteboard(clipboard, -1);
    }
    let refresh_callbacks = std::env::var("MACWL_FRAME_CALLBACKS").map(|v| v != "timer").unwrap_or(true);
    let (refresh, refresh_ticks) = macwl_compositor::refresh_channel();
    if refresh_callbacks && !latency_mode {
        start_refresh_ticks(&screen, refresh);
    }
    let auto_keys: Option<usize> = std::env::var("MACWL_AUTO_KEYS").ok().and_then(|n| n.parse().ok());
    if !latency_mode {
        tracing_subscriber::fmt().init();
        let host = ShellHost(scene.clone());
        let scale = screen.backingScaleFactor();
        let pixels = (
            (screen.frame().size.width * scale) as i32,
            (screen.frame().size.height * scale) as i32,
        );
        let refresh_mhz = screen.maximumFramesPerSecond() as i32 * 1000;
        std::thread::spawn(move || {
            let options = Options {
                clipboard: Some(clipboard_events),
                input: Some(key_events),
                refresh: refresh_callbacks.then_some(refresh_ticks),
                screen: pixels,
                scale: scale.round() as i32,
                refresh_mhz,
            };
            macwl_compositor::run_with(
                Box::new(host),
                options,
            )
            .expect("compositor failed");
        });
    }
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            screen.frame(),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("macwayland"));
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

    let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let queue = device.newCommandQueue().expect("no command queue");

    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&device));
    layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    layer.setOpaque(true);
    layer.setFramebufferOnly(fb_only);
    layer.setMaximumDrawableCount(drawables);
    layer.setDisplaySyncEnabled(vsync);

    window.setAcceptsMouseMovedEvents(true);
    let view = window.contentView().expect("no content view");
    view.setWantsLayer(true);
    view.setLayer(Some(&layer));
    // Size the drawable to the full-screen backing size up front; the window
    // only changes size during the full-screen transition.
    let scale = screen.backingScaleFactor();
    layer.setContentsScale(scale);
    layer.setDrawableSize(objc2_foundation::NSSize {
        width: screen.frame().size.width * scale,
        height: screen.frame().size.height * scale,
    });

    // --- Input: handled the moment AppKit delivers it --------------------------
    let input: Rc<RefCell<InputState>> = Rc::default();
    let monitor_input = input.clone();
    let key_pending: Arc<Mutex<Option<(f64, f64)>>> = Arc::default();
    let monitor_key_pending = key_pending.clone();
    let auto_key_sender = keys.clone();
    let (monitor_view, monitor_layer) = (view.clone(), layer.clone());
    let monitor = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let event = unsafe { event.as_ref() };
        let ty = event.r#type();
        let is_key = ty == NSEventType::KeyDown || ty == NSEventType::KeyUp;
        let flags = event.modifierFlags().0 as u64;
        // Quitting: Escape in latency mode (the measurement tool's habit),
        // Cmd+Q otherwise (Escape belongs to the terminal).
        if ty == NSEventType::KeyDown
            && ((latency_mode && event.keyCode() == KEY_ESCAPE) || (event.keyCode() == KEY_Q && flags & COMMAND != 0))
        {
            std::process::exit(0);
        }
        if !latency_mode {
            if ty == NSEventType::KeyDown && !event.isARepeat() {
                *monitor_key_pending.lock().unwrap() = Some((event.timestamp(), CACurrentMediaTime()));
            }
            forward_key(&keys, event, ty, flags);
            forward_mouse(&keys, event, ty, &monitor_view, &monitor_layer);
            // The client draws the answer; nothing to flip here.
            return if is_key { std::ptr::null_mut() } else { event as *const NSEvent as *mut NSEvent };
        }
        let is_down = ty == NSEventType::KeyDown
            || ty == NSEventType::LeftMouseDown
            || ty == NSEventType::RightMouseDown;
        let is_up =
            ty == NSEventType::KeyUp || ty == NSEventType::LeftMouseUp || ty == NSEventType::RightMouseUp;
        let changed = {
            let mut input = monitor_input.borrow_mut();
            if is_down && !(ty == NSEventType::KeyDown && event.isARepeat()) {
                input.white = true;
                input.pending = Some((event.timestamp(), CACurrentMediaTime()));
                true
            } else if is_up {
                input.white = false;
                true
            } else {
                false
            }
        };
        // We're already on the main thread: present now rather than queueing.
        if changed && ON_DEMAND.load(Ordering::Relaxed) {
            with_renderer(Renderer::render_now);
        }
        // Swallow keys (nothing handles them yet, and AppKit would beep); pass
        // everything else on so window management keeps working.
        if is_key { std::ptr::null_mut() } else { event as *const NSEvent as *mut NSEvent }
    });
    let _monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::Any, &monitor) };

    // --- Auto mode: fake input events at random times ---------------------------
    let auto = auto_samples.map(|samples| {
        let trigger: Arc<Mutex<Option<f64>>> = Arc::default();
        let thread_trigger = trigger.clone();
        std::thread::spawn(move || {
            wait_for_full_screen();
            let mut rng: u32 = 0x9e37_79b9;
            loop {
                *thread_trigger.lock().unwrap() = Some(CACurrentMediaTime());
                request_render();
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                std::thread::sleep(Duration::from_micros(200_000 + (rng % 50_000) as u64));
            }
        });
        Auto { samples, trigger, shade: false, start: CACurrentMediaTime() }
    });

    // Auto keys: `a`, Backspace, `a`, ... into the client, each when the last
    // has been answered (or after a second), at a random phase of the frame.
    if let Some(n) = auto_keys.filter(|_| !latency_mode) {
        let (keys, pending) = (auto_key_sender, key_pending.clone());
        std::thread::spawn(move || {
            // Wait out the full-screen transition and the client's start.
            wait_for_full_screen();
            std::thread::sleep(Duration::from_millis(2500));
            let mut rng: u32 = 0x9e37_79b9;
            for i in 0..n * 2 {
                let code = if i % 2 == 0 { 30 } else { 14 }; // KEY_A, KEY_BACKSPACE
                let now = CACurrentMediaTime();
                let time_ms = (now * 1000.0) as u32;
                *pending.lock().unwrap() = Some((now, now));
                let _ = keys.send(InputEvent::Key { code, pressed: true, time_ms });
                let _ = keys.send(InputEvent::Key { code, pressed: false, time_ms });
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                std::thread::sleep(Duration::from_micros(150_000 + (rng % 50_000) as u64));
            }
        });
    }

    let config = format!(
        "{} pacing={} callbacks={} frame_latency={frame_latency} drawables={drawables} vsync={} fbonly={} refresh={}Hz",
        if latency_mode { "latency" } else { "compositor" },
        if on_demand { "demand" } else { "link" },
        if refresh_callbacks { "refresh" } else { "timer" },
        vsync as u8,
        fb_only as u8,
        screen.maximumFramesPerSecond(),
    );
    let renderer = Renderer {
        layer: layer.clone(),
        device: device.clone(),
        queue,
        latency_mode,
        scene,
        textures: HashMap::new(),
        scene_seq: 0,
        layers: Vec::new(),
        quad: quad_pipeline(&device),
        stats: Arc::new(Mutex::new(LatencyStats { quiet: auto.is_some() && std::env::var_os("MACWL_SAMPLES").is_none(), ..Default::default() })),
        input,
        auto,
        config,
        key_pending,
        auto_keys,
    };

    RENDERER.with_borrow_mut(|r| *r = Some(renderer));

    // --- Frame pacing ------------------------------------------------------------
    // Under display-link pacing the delegate is a weak property, so it and the
    // link must outlive the run loop.
    let _link = (!on_demand).then(|| {
        let delegate = LinkDelegate::new(mtm);
        let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
        link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        link.setPreferredFrameLatency(frame_latency);
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        if (link.preferredFrameLatency() - frame_latency).abs() > f32::EPSILON {
            eprintln!("note: preferredFrameLatency clamped to {}", link.preferredFrameLatency());
        }
        (delegate, link)
    });

    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    enter_full_screen(window, mtm);
    // First frame (on-demand pacing only draws when something changes).
    request_render();
    app.run();
}
