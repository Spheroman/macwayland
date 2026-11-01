//! Native latency baseline, measured from outside the render path.
//!
//! `native-probe <draw|layer|metal> [samples]`
//!
//!   draw   classic AppKit: NSView subclass, setNeedsDisplay → drawRect: (TextEdit-style)
//!   layer  Core Animation: change a layer's backgroundColor
//!   metal  CAMetalLayer present (same path as macwl-shell); also reports
//!          MTLDrawable.presentedTime so ScreenCaptureKit can be calibrated
//!
//! A background thread fires fake "input events" at random times; each one is
//! hopped to the main thread (as a real event would be) and flips the full-screen
//! window between two dark greys. ScreenCaptureKit watches the display, and the
//! first captured frame showing the new shade gives event → displayTime.
//!
//! `native-probe metal --no-capture [samples]` skips ScreenCaptureKit and
//! reports presentedTime alone: capturing the screen can make WindowServer
//! composite a full-screen window it would otherwise show directly, which
//! costs refreshes, so this is the figure to compare macwl-shell against.
//!
//! Needs Screen Recording permission for the launching terminal.

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSRectFill, NSScreen,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::CGColor;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSRect, NSString};
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice,
    MTLDevice, MTLDrawable, MTLLoadAction, MTLPixelFormat, MTLRenderPassDescriptor, MTLStoreAction,
};
use objc2_quartz_core::{CACurrentMediaTime, CAMetalDrawable, CAMetalLayer, CATransaction};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamFrameInfoDisplayTime,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};

const DARK: f64 = 0.10;
const LIGHT: f64 = 0.20;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Draw,
    Layer,
    Metal,
}

/// State shared between the trigger thread, the main thread and the SCK queue.
#[derive(Default)]
struct Shared {
    /// (event time, shade it switches to) awaiting its first on-screen frame.
    pending: Option<(f64, bool)>,
    sck_ms: Vec<f64>,
    metal_ms: Vec<f64>,
    capture_running: bool,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.get_or_init(Default::default).lock().unwrap()
}

#[allow(deprecated)] // libc points at the mach2 crate; not worth a dependency for one call
fn mach_to_seconds(t: u64) -> f64 {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        unsafe { libc::mach_timebase_info(&mut info) };
        (info.numer, info.denom)
    });
    t as f64 * numer as f64 / denom as f64 / 1e9
}

// --- draw mode: a plain NSView that paints in drawRect: ---------------------------

struct ProbeViewIvars {
    light: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "MacwlProbeView"]
    #[ivars = ProbeViewIvars]
    struct ProbeView;

    impl ProbeView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, rect: NSRect) {
            let v = if self.ivars().light.get() { LIGHT } else { DARK };
            NSColor::colorWithSRGBRed_green_blue_alpha(v, v, v, 1.0).set();
            NSRectFill(rect);
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }
    }
);

impl ProbeView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ProbeViewIvars { light: Cell::new(false) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

// --- ScreenCaptureKit output -----------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "MacwlProbeCapture"]
    struct CaptureOutput;

    unsafe impl NSObjectProtocol for CaptureOutput {}

    unsafe impl SCStreamOutput for CaptureOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, ty: SCStreamOutputType) {
            if ty == SCStreamOutputType::Screen {
                on_captured_frame(sample);
            }
        }
    }
);

impl CaptureOutput {
    fn new() -> Retained<Self> {
        unsafe { msg_send![Self::alloc(), init] }
    }
}

fn on_captured_frame(sample: &CMSampleBuffer) {
    // Frame metadata: only "complete" frames (status 0) carry new pixels.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return };
    let attachments: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*attachments as *const _ as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let Some(info) = attachments.firstObject() else { return };
    let number = |key: &NSString| info.objectForKey(key).and_then(|o| o.downcast::<NSNumber>().ok());
    let status = number(unsafe { SCStreamFrameInfoStatus }).map(|n| n.integerValue());
    if status != Some(0) {
        return;
    }
    let Some(display_time) = number(unsafe { SCStreamFrameInfoDisplayTime }) else { return };
    let display_time = mach_to_seconds(display_time.unsignedLongLongValue());

    // Sample the centre pixel (BGRA) to see which shade is on screen.
    let Some(pixels) = (unsafe { sample.image_buffer() }) else { return };
    let light = unsafe {
        CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
        let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(&pixels);
        let (w, h) = (CVPixelBufferGetWidth(&pixels), CVPixelBufferGetHeight(&pixels));
        let blue = *base.add((h / 2) * stride + (w / 2) * 4);
        CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
        // DARK ≈ 26–30 and LIGHT ≈ 51–55 in 8-bit, depending on colour management.
        blue > 40
    };

    let mut s = shared();
    s.capture_running = true;
    if let Some((event_time, want)) = s.pending {
        if light == want {
            s.sck_ms.push((display_time - event_time) * 1e3);
            s.pending = None;
        }
    }
}

// --- main-thread rendering --------------------------------------------------------

struct Targets {
    mode: Mode,
    view: Retained<NSView>,
    probe_view: Option<Retained<ProbeView>>,
    metal: Option<(Retained<CAMetalLayer>, Retained<ProtocolObject<dyn MTLCommandQueue>>)>,
}

thread_local! {
    static TARGETS: RefCell<Option<Targets>> = const { RefCell::new(None) };
}

/// Apply a shade change on the main thread using the selected native path.
fn apply(light: bool) {
    TARGETS.with_borrow(|t| {
        let t = t.as_ref().unwrap();
        let v = if light { LIGHT } else { DARK };
        match t.mode {
            Mode::Draw => {
                let pv = t.probe_view.as_ref().unwrap();
                pv.ivars().light.set(light);
                pv.setNeedsDisplay(true);
            }
            Mode::Layer => {
                CATransaction::begin();
                CATransaction::setDisableActions(true);
                let color = CGColor::new_srgb(v, v, v, 1.0);
                t.view.layer().unwrap().setBackgroundColor(Some(&color));
                CATransaction::commit();
            }
            Mode::Metal => {
                let (layer, queue) = t.metal.as_ref().unwrap();
                let Some(drawable) = layer.nextDrawable() else { return };
                let cmd = queue.commandBuffer().unwrap();
                let rpd = MTLRenderPassDescriptor::new();
                let a = unsafe { rpd.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(&drawable.texture()));
                a.setLoadAction(MTLLoadAction::Clear);
                a.setStoreAction(MTLStoreAction::Store);
                a.setClearColor(MTLClearColor { red: v, green: v, blue: v, alpha: 1.0 });
                cmd.renderCommandEncoderWithDescriptor(&rpd).unwrap().endEncoding();
                let d: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
                let event_time = shared().pending.map(|(t, _)| t);
                if let Some(event_time) = event_time {
                    let handler = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                        let glass = unsafe { d.as_ref().presentedTime() };
                        if glass > 0.0 {
                            shared().metal_ms.push((glass - event_time) * 1e3);
                        }
                    });
                    unsafe { d.addPresentedHandler(RcBlock::as_ptr(&handler)) };
                }
                cmd.presentDrawable(d);
                cmd.commit();
            }
        }
    });
}

fn summary(label: &str, samples: &[f64]) -> String {
    if samples.is_empty() {
        return format!("{label}: no samples");
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
    format!(
        "{label}: n={} p50={:.2} p90={:.2} min={:.2} max={:.2} ms",
        s.len(),
        pct(0.5),
        pct(0.9),
        s[0],
        s[s.len() - 1]
    )
}

fn start_capture() {
    let handler = RcBlock::new(|content: *mut SCShareableContent, error: *mut NSError| {
        if let Some(error) = unsafe { error.as_ref() } {
            eprintln!(
                "ScreenCaptureKit unavailable: {} (grant Screen Recording to your terminal)",
                error.localizedDescription()
            );
            std::process::exit(1);
        }
        let content = unsafe { &*content };
        let display = unsafe { content.displays() }.firstObject().expect("no display");
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(160);
            config.setHeight(90);
            config.setMinimumFrameInterval(CMTime::new(1, 240));
            config.setQueueDepth(5);
            config.setShowsCursor(false);
            config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));

            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let output = CaptureOutput::new();
            let queue = DispatchQueue::new("macwl.probe.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .expect("addStreamOutput failed");
            let started = RcBlock::new(|err: *mut NSError| {
                if let Some(err) = err.as_ref() {
                    eprintln!("startCapture failed: {}", err.localizedDescription());
                    std::process::exit(1);
                }
            });
            stream.startCaptureWithCompletionHandler(Some(&started));
            // Keep the stream and its output alive for the life of the process.
            std::mem::forget((stream, output, queue));
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = match args.get(1).map(String::as_str) {
        Some("draw") => Mode::Draw,
        Some("layer") => Mode::Layer,
        Some("metal") => Mode::Metal,
        _ => {
            eprintln!("usage: native-probe <draw|layer|metal> [samples] | native-probe metal --no-capture [samples]");
            std::process::exit(2);
        }
    };
    let no_capture = mode == Mode::Metal && args.iter().any(|a| a == "--no-capture");
    let samples: usize = args[2..].iter().find_map(|n| n.parse().ok()).unwrap_or(30);
    let mtm = MainThreadMarker::new().unwrap();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let screen = NSScreen::mainScreen(mtm).expect("no screen");
    let frame = screen.frame();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

    let (view, probe_view, metal) = match mode {
        Mode::Draw => {
            let pv = ProbeView::new(mtm, frame);
            let view: Retained<NSView> = Retained::into_super(pv.clone());
            (view, Some(pv), None)
        }
        Mode::Layer => {
            let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
            view.setWantsLayer(true);
            let color = CGColor::new_srgb(DARK, DARK, DARK, 1.0);
            view.layer().unwrap().setBackgroundColor(Some(&color));
            (view, None, None)
        }
        Mode::Metal => {
            let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
            let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            layer.setOpaque(true);
            layer.setMaximumDrawableCount(2);
            let scale = screen.backingScaleFactor();
            layer.setContentsScale(scale);
            layer.setDrawableSize(objc2_foundation::NSSize {
                width: frame.size.width * scale,
                height: frame.size.height * scale,
            });
            view.setWantsLayer(true);
            view.setLayer(Some(&layer));
            let queue = device.newCommandQueue().unwrap();
            (view, None, Some((layer, queue)))
        }
    };
    window.setContentView(Some(&view));
    TARGETS.with_borrow_mut(|t| *t = Some(Targets { mode, view, probe_view, metal }));

    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.toggleFullScreen(None);
    if mode == Mode::Metal {
        // Put the initial dark frame on screen.
        DispatchQueue::main().exec_async(|| apply(false));
    }

    if no_capture {
        shared().capture_running = true;
    } else {
        start_capture();
    }

    // Fake input events at random times, hopped to the main thread like real ones.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3000));
        let mut light = false;
        let mut rng: u32 = 0x9e37_79b9;
        let deadline = CACurrentMediaTime() + samples as f64 * 0.5 + 10.0;
        loop {
            {
                let s = shared();
                if !s.capture_running {
                    drop(s);
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let done = if no_capture { s.metal_ms.len() } else { s.sck_ms.len() };
                if done >= samples || CACurrentMediaTime() > deadline {
                    if no_capture {
                        println!("{}", summary("metal (CAMetal)    presentedTime, no capture", &s.metal_ms));
                        std::process::exit(0);
                    }
                    let label = match mode {
                        Mode::Draw => "draw  (drawRect)   sck",
                        Mode::Layer => "layer (CALayer)    sck",
                        Mode::Metal => "metal (CAMetal)    sck",
                    };
                    println!("{}", summary(label, &s.sck_ms));
                    if mode == Mode::Metal {
                        println!("{}", summary("metal (CAMetal)    presentedTime", &s.metal_ms));
                    }
                    std::process::exit(0);
                }
            }
            light = !light;
            shared().pending = Some((CACurrentMediaTime(), light));
            DispatchQueue::main().exec_async(move || apply(light));
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            std::thread::sleep(Duration::from_micros(200_000 + (rng % 50_000) as u64));
        }
    });

    app.run();
}
