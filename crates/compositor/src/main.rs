//! Headless host: runs the compositor and dumps the first few frames of the
//! screen as PPMs (and the latest one as `frame-last.ppm`): the window, and
//! its popups over it.
//!
//! `MACWL_TYPE="text"` types `text` into the focused window a second after
//! the first frame, through the same input path the shell uses (a test aid:
//! `\n` is Return, `\e` Escape, `\cX` Ctrl+X, `\p` pauses for a second).
//! It drives the pointer too: `\mX,Y;` moves it to pixel (X, Y) of the
//! screen, `\bN` clicks button N (1 left, 2 middle, 3 right), `\dN` and
//! `\uN` press and release it (a move between them drags), `\wN;` turns
//! the wheel N clicks (negative: up). `\kA+B+C;` presses evdev keys A, B and
//! C and lets them go (`\k29+42+47;` is Ctrl+Shift+V).

use std::io::Write;

use std::collections::HashMap;

use macwl_compositor::{BufferData, InputEvent, Layer, Rect};
use tracing::info;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let (keys, key_events) = macwl_compositor::input_channel();
    // MACWL_CLIPBOARD: text on the host's clipboard, for clients to paste.
    let (clipboard, clipboard_events) = macwl_compositor::clipboard_channel();
    if let Ok(text) = std::env::var("MACWL_CLIPBOARD") {
        let _ = clipboard.send(text);
    }
    let options =
        macwl_compositor::Options { input: Some(key_events), clipboard: Some(clipboard_events), ..Default::default() };
    let screen = options.screen;
    macwl_compositor::run_with(
        Box::new(Screen { keys, text: std::env::var("MACWL_TYPE").ok(), n: 0, screen, surfaces: HashMap::new(), layers: Vec::new() }),
        options,
    )
}

/// What's on screen: each surface's latest pixels, and where they go.
struct Screen {
    keys: macwl_compositor::InputSender,
    text: Option<String>,
    /// Frames so far.
    n: u32,
    screen: (i32, i32),
    /// Each surface's pixels, tightly packed, with their size and whether
    /// they're opaque.
    surfaces: HashMap<u32, (Vec<u8>, i32, i32, bool)>,
    layers: Vec<Layer>,
}

impl macwl_compositor::Host for Screen {
    fn frame(&mut self, id: u32, pixels: &[u8], data: &BufferData, _damage: &[Rect]) {
        info!(
            "commit {id}: {}x{} stride={} format={:?} pool_len={}",
            data.width,
            data.height,
            data.stride,
            data.format,
            pixels.len()
        );
        let (w, h, stride, off) = (data.width as usize, data.height as usize, data.stride as usize, data.offset as usize);
        let mut copy = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            copy.extend_from_slice(&pixels[off + y * stride..][..w * 4]);
        }
        let opaque = data.format != macwl_compositor::ShmFormat::Argb8888;
        self.surfaces.insert(id, (copy, data.width, data.height, opaque));
        self.dump();
    }

    fn copied(&mut self, text: &str) {
        info!("copied: {text:?}");
    }

    fn cursor(&mut self, cursor: macwl_compositor::Cursor) {
        match cursor {
            macwl_compositor::Cursor::Image { width, height, scale, hotspot, .. } => {
                info!("cursor: image {width}x{height} scale {scale} hotspot {hotspot:?}")
            }
            cursor => info!("cursor: {cursor:?}"),
        }
    }

    fn layout(&mut self, layers: &[Layer]) {
        info!("layout: {layers:?}");
        self.surfaces.retain(|id, _| layers.iter().any(|l| l.id == *id));
        self.layers = layers.to_vec();
        self.dump();
    }
}

impl Screen {
    /// Draw the layers, bottom to top (blending popups' premultiplied
    /// alpha), and write the screen out.
    fn dump(&mut self) {
        if self.layers.is_empty() {
            return;
        }
        let (sw, sh) = (self.screen.0 as usize, self.screen.1 as usize);
        let mut out = vec![0u8; sw * sh * 3];
        for layer in &self.layers {
            let Some((pixels, w, h, opaque)) = self.surfaces.get(&layer.id) else { continue };
            for y in 0..*h {
                let sy = layer.y + y;
                if !(0..sh as i32).contains(&sy) {
                    continue;
                }
                for x in 0..*w {
                    let sx = layer.x + x;
                    if !(0..sw as i32).contains(&sx) {
                        continue;
                    }
                    // wl_shm formats are little-endian: bytes are B, G, R, A.
                    let p = &pixels[(y * w + x) as usize * 4..][..4];
                    let a = if *opaque { 255 } else { p[3] as u32 };
                    let o = &mut out[(sy as usize * sw + sx as usize) * 3..][..3];
                    for (dst, src) in o.iter_mut().zip([p[2], p[1], p[0]]) {
                        *dst = (src as u32 + *dst as u32 * (255 - a) / 255).min(255) as u8;
                    }
                }
            }
        }
        if self.n < 3 {
            dump_ppm(&format!("frame-{}.ppm", self.n), &out, sw, sh);
        }
        dump_ppm("frame-last.ppm", &out, sw, sh);
        self.n += 1;
        self.start_typing();
    }

    /// Type `MACWL_TYPE` a second after the first frame.
    fn start_typing(&mut self) {
        let Some(text) = self.text.take() else { return };
        let keys = self.keys.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let mut at = None;
            for step in script(&text) {
                match step {
                    Step::Text(part) => type_text(&keys, &control_chars(&part.replace("\\n", "\n").replace("\\e", "\x1b"))),
                    Step::Pause => std::thread::sleep(std::time::Duration::from_secs(1)),
                    Step::Move(x, y) => move_pointer(&keys, &mut at, (x, y)),
                    Step::Button(n, press) => {
                        for pressed in press.map_or(vec![true, false], |p| vec![p]) {
                            send_pointer(&keys, |time_ms| InputEvent::PointerButton { button: button(n), pressed, time_ms });
                        }
                    }
                    Step::Keys(codes) => {
                        let presses = codes.iter().map(|&c| (c, true)).chain(codes.iter().rev().map(|&c| (c, false)));
                        for (code, pressed) in presses {
                            send_pointer(&keys, |time_ms| InputEvent::Key { code, pressed, time_ms });
                        }
                    }
                    Step::Wheel(n) => {
                        for _ in 0..n.unsigned_abs() {
                            let dy = 15.0 * n.signum() as f64;
                            send_pointer(&keys, |time_ms| InputEvent::PointerAxis {
                                dx: 0.0,
                                dy,
                                steps: (0, 120 * n.signum()),
                                finger: false,
                                stop: false,
                                time_ms,
                            });
                        }
                    }
                }
            }
        });
    }
}

/// A step of a `MACWL_TYPE` script.
#[derive(Debug, PartialEq)]
enum Step {
    /// Text to type (with `\n`, `\e` and `\cX` still escaped).
    Text(String),
    Pause,
    Move(f64, f64),
    /// Button N clicked (None), pressed or released.
    Button(u32, Option<bool>),
    Wheel(i32),
    /// Evdev keys held down together.
    Keys(Vec<u32>),
}

/// `text` as steps: the pointer's escapes and pauses, and text between them.
fn script(text: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut rest = text;
    let mut typed = String::new();
    while let Some(i) = rest.find('\\') {
        typed.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        // An argument ending at `;`, or a single digit.
        let until_semicolon = |s: &str| s.split_once(';').map(|(arg, rest)| (arg.to_string(), rest.len()));
        let step = match after.chars().next() {
            Some('p') => Some((Step::Pause, after.len() - 1)),
            Some('m') => until_semicolon(&after[1..]).and_then(|(arg, left)| {
                let (x, y) = arg.split_once(',')?;
                Some((Step::Move(x.trim().parse().ok()?, y.trim().parse().ok()?), left))
            }),
            Some('k') => until_semicolon(&after[1..]).and_then(|(arg, left)| {
                let keys: Option<Vec<u32>> = arg.split('+').map(|k| k.trim().parse().ok()).collect();
                Some((Step::Keys(keys?), left))
            }),
            Some('w') => until_semicolon(&after[1..]).and_then(|(arg, left)| Some((Step::Wheel(arg.trim().parse().ok()?), left))),
            Some(c @ ('b' | 'd' | 'u')) => after[1..].chars().next().and_then(|n| n.to_digit(10)).map(|n| {
                let press = match c {
                    'd' => Some(true),
                    'u' => Some(false),
                    _ => None,
                };
                (Step::Button(n, press), after.len() - 2)
            }),
            _ => None,
        };
        match step {
            Some((step, left)) => {
                if !typed.is_empty() {
                    steps.push(Step::Text(std::mem::take(&mut typed)));
                }
                steps.push(step);
                rest = &after[after.len() - left..];
            }
            // Not the pointer's: leave it for typing.
            None => {
                typed.push('\\');
                rest = after;
            }
        }
    }
    typed.push_str(rest);
    if !typed.is_empty() {
        steps.push(Step::Text(typed));
    }
    steps
}

/// X11's button numbers (1 left, 2 middle, 3 right) as evdev codes.
fn button(n: u32) -> u32 {
    match n {
        2 => macwl_compositor::BTN_MIDDLE,
        3 => macwl_compositor::BTN_RIGHT,
        _ => macwl_compositor::BTN_LEFT,
    }
}

/// Send an input event stamped now, and give the client time with it.
fn send_pointer(keys: &macwl_compositor::InputSender, event: impl FnOnce(u32) -> InputEvent) {
    let event = event(START.elapsed().as_millis() as u32);
    info!("{event:?}");
    let _ = keys.send(event);
    std::thread::sleep(std::time::Duration::from_millis(40));
}

/// Glide the pointer from where it is (`at`) to `to`, as a mouse would, in
/// a few steps: jump there the first time.
fn move_pointer(keys: &macwl_compositor::InputSender, at: &mut Option<(f64, f64)>, to: (f64, f64)) {
    const STEPS: u32 = 8;
    let from = at.unwrap_or(to);
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let (x, y) = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        let _ = keys.send(InputEvent::PointerMotion { x, y, time_ms: START.elapsed().as_millis() as u32 });
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    info!("pointer at {to:?}");
    *at = Some(to);
}

static START: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);

/// `\cX` in `text` as the control character Ctrl+X types.
fn control_chars(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.clone().next() == Some('c') {
            chars.next();
            if let Some(x) = chars.next() {
                out.push(((x.to_ascii_uppercase() as u8) & 0x1f) as char);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Type `text` (US layout) as key presses and releases; control characters
/// as Ctrl and their letter.
fn type_text(keys: &macwl_compositor::InputSender, text: &str) {
    const SHIFT: u32 = 42;
    const CTRL: u32 = 29;
    let mut t = 0;
    let mut key = |code, pressed| {
        t += 15;
        if pressed {
            info!("key {code} down");
        }
        let _ = keys.send(InputEvent::Key { code, pressed, time_ms: t });
        std::thread::sleep(std::time::Duration::from_millis(40));
    };
    for c in text.chars() {
        if let ctrl @ '\x01'..='\x1a' = c
            && c != '\n'
        {
            let (code, _) = evdev_for_char((b'a' + ctrl as u8 - 1) as char).unwrap();
            key(CTRL, true);
            key(code, true);
            key(code, false);
            key(CTRL, false);
            continue;
        }
        let Some((code, shift)) = evdev_for_char(c) else { continue };
        if shift {
            key(SHIFT, true);
        }
        key(code, true);
        key(code, false);
        if shift {
            key(SHIFT, false);
        }
    }
}

/// The key (and whether Shift) that types `c` on a US keyboard.
fn evdev_for_char(c: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    const PLAIN: &str = "1234567890-=[];'\\,./` \n";
    const PLAIN_KEYS: [u32; 22] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 26, 27, 39, 40, 43, 51, 52, 53, 41, 57];
    const SHIFTED: &str = "!@#$%^&*()_+{}:\"|<>?~";
    if c.is_ascii_lowercase() {
        return Some((LETTERS[(c as u8 - b'a') as usize], false));
    }
    if c.is_ascii_uppercase() {
        return Some((LETTERS[(c as u8 - b'A') as usize], true));
    }
    if c == '\n' {
        return Some((28, false));
    }
    if c == '\x1b' {
        return Some((1, false));
    }
    if let Some(i) = PLAIN.chars().position(|p| p == c) {
        return PLAIN_KEYS.get(i).map(|&k| (k, false));
    }
    SHIFTED.chars().position(|p| p == c).map(|i| (PLAIN_KEYS[i], true))
}

/// Write RGB pixels out as a binary PPM.
fn dump_ppm(path: &str, rgb: &[u8], w: usize, h: usize) {
    let mut out = Vec::with_capacity(rgb.len() + 32);
    write!(out, "P6\n{w} {h}\n255\n").unwrap();
    out.extend_from_slice(rgb);
    // Whole or not at all, should the compositor be stopped mid-write.
    let partial = format!("{path}.partial");
    std::fs::write(&partial, out).unwrap();
    std::fs::rename(&partial, path).unwrap();
    info!("dumped {path}");
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts() {
        let text = |s: &str| Step::Text(s.into());
        assert_eq!(script("ls\\n"), [text("ls\\n")]);
        assert_eq!(
            script("a\\pb\\m10, 20.5;\\b1\\d3\\u3\\w-2;\\k29+42+47;\\cC\\x"),
            [
                text("a"),
                Step::Pause,
                text("b"),
                Step::Move(10.0, 20.5),
                Step::Button(1, None),
                Step::Button(3, Some(true)),
                Step::Button(3, Some(false)),
                Step::Wheel(-2),
                Step::Keys(vec![29, 42, 47]),
                text("\\cC\\x"),
            ]
        );
    }
}
