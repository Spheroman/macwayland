//! Subsurfaces, from a real client: a synchronized one's buffer comes with
//! its parent's commit, it's drawn where it was put (above or below its
//! parent), a desynchronized one's commits are its own, and one without a
//! buffer isn't shown.

use std::{
    os::fd::AsFd,
    sync::mpsc,
    time::{Duration, Instant},
};

use macwl_compositor::{BufferData, Host, Layer, Options, Rect};
use rustix::{
    fs::{Mode, ftruncate},
    shm,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Debug)]
enum Seen {
    Frame { id: u32, size: (i32, i32) },
    Layout(Vec<Layer>),
}

struct Recorder(mpsc::Sender<Seen>);

impl Host for Recorder {
    fn frame(&mut self, id: u32, _pixels: &[u8], data: &BufferData, _damage: &[Rect]) {
        let _ = self.0.send(Seen::Frame { id, size: (data.width, data.height) });
    }
    fn layout(&mut self, layers: &[Layer]) {
        let _ = self.0.send(Seen::Layout(layers.to_vec()));
    }
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    configured: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(c: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_compositor" => c.compositor = Some(registry.bind(name, 4, qh, ())),
                "wl_subcompositor" => c.subcompositor = Some(registry.bind(name, 1, qh, ())),
                "wl_shm" => c.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => c.wm_base = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(c: &mut Self, surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            c.configured = true;
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Client: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore xdg_wm_base::XdgWmBase);
delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);

/// What the host saw, until `done` says so (or two seconds pass).
fn wait(seen: &mpsc::Receiver<Seen>, mut done: impl FnMut(&Seen) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match seen.recv_timeout(left) {
            Ok(s) if done(&s) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[test]
fn subsurfaces() {
    let dir = tempfile::tempdir().unwrap();
    // Only this test touches XDG_RUNTIME_DIR, which the compositor reads.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
    let (tx, seen) = mpsc::channel();
    std::thread::spawn(move || {
        let options = Options { screen: (400, 300), ..Options::default() };
        let _ = macwl_compositor::run_with(Box::new(Recorder(tx)), options);
    });
    let socket = dir.path().join("wayland-mac");
    while !socket.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-mac") };

    let conn = Connection::connect_to_env().unwrap();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut c = Client::default();
    queue.roundtrip(&mut c).unwrap();

    // One pool: a 200x100 window, then a 30x20 subsurface.
    let size = (200 * 100 + 30 * 20) * 4;
    let name = format!("/macwl-sub-{}", std::process::id());
    let fd = shm::open(&name, shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR, Mode::RUSR | Mode::WUSR).unwrap();
    shm::unlink(&name).unwrap();
    ftruncate(&fd, size as u64).unwrap();
    let pool = c.shm.as_ref().unwrap().create_pool(fd.as_fd(), size, &qh, ());
    let big = pool.create_buffer(0, 200, 100, 800, wl_shm::Format::Argb8888, &qh, ());
    let small = pool.create_buffer(200 * 100 * 4, 30, 20, 120, wl_shm::Format::Argb8888, &qh, ());

    let top = c.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = c.wm_base.as_ref().unwrap().get_xdg_surface(&top, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    top.commit();
    while !c.configured {
        queue.blocking_dispatch(&mut c).unwrap();
    }
    top.attach(Some(&big), 0, 0);
    top.commit();
    queue.roundtrip(&mut c).unwrap();
    let mut top_id = 0;
    assert!(wait(&seen, |s| matches!(s, Seen::Frame { id, size: (200, 100) } if { top_id = *id; true })));
    // Centred on the 400x300 screen.
    let top_layer = Layer { id: top_id, x: 100, y: 100 };
    assert!(wait(&seen, |s| matches!(s, Seen::Layout(l) if l == &[top_layer])));

    // A synchronized subsurface: nothing until its parent commits.
    let child = c.compositor.as_ref().unwrap().create_surface(&qh, ());
    let sub = c.subcompositor.as_ref().unwrap().get_subsurface(&child, &top, &qh, ());
    sub.set_position(20, 10);
    child.attach(Some(&small), 0, 0);
    child.commit();
    queue.roundtrip(&mut c).unwrap();
    assert!(!wait(&seen, |s| matches!(s, Seen::Frame { size: (30, 20), .. })), "drawn before its parent's commit");
    top.commit();
    queue.roundtrip(&mut c).unwrap();
    let mut child_id = 0;
    assert!(wait(&seen, |s| matches!(s, Seen::Frame { id, size: (30, 20) } if { child_id = *id; true })));
    let child_layer = Layer { id: child_id, x: 120, y: 110 };
    assert!(wait(&seen, |s| matches!(s, Seen::Layout(l) if l == &[top_layer, child_layer])));

    // Below its parent, moved (both with the parent's commit).
    sub.place_below(&top);
    sub.set_position(-5, 0);
    top.commit();
    queue.roundtrip(&mut c).unwrap();
    let moved = Layer { id: child_id, x: 95, y: 100 };
    assert!(wait(&seen, |s| matches!(s, Seen::Layout(l) if l == &[moved, top_layer])));

    // Desynchronized: its commits show at once.
    sub.set_desync();
    child.attach(Some(&small), 0, 0);
    child.commit();
    queue.roundtrip(&mut c).unwrap();
    assert!(wait(&seen, |s| matches!(s, Seen::Frame { id, .. } if *id == child_id)));

    // No buffer, not shown.
    child.attach(None, 0, 0);
    child.commit();
    queue.roundtrip(&mut c).unwrap();
    assert!(wait(&seen, |s| matches!(s, Seen::Layout(l) if l == &[top_layer])));
}
