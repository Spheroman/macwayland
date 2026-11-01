//! What a window is told it is: maximized to the screen, fullscreen only
//! when it asks (and back when it asks again).

use std::time::Duration;

use macwl_compositor::{BufferData, Host, Layer, Options, Rect};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct Nothing;

impl Host for Nothing {
    fn frame(&mut self, _: u32, _: &[u8], _: &BufferData, _: &[Rect]) {}
    fn layout(&mut self, _: &[Layer]) {}
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Each configure: its size and states.
    configures: Vec<((i32, i32), Vec<xdg_toplevel::State>)>,
    pending: Option<((i32, i32), Vec<xdg_toplevel::State>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(c: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_compositor" => c.compositor = Some(registry.bind(name, 4, qh, ())),
                "xdg_wm_base" => c.wm_base = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(c: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { width, height, states } = event {
            let states = states
                .chunks_exact(4)
                .filter_map(|s| xdg_toplevel::State::try_from(u32::from_ne_bytes(s.try_into().unwrap())).ok())
                .collect();
            c.pending = Some(((width, height), states));
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(c: &mut Self, surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            c.configures.extend(c.pending.take());
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore xdg_wm_base::XdgWmBase);

#[test]
fn windows_are_maximized_and_fullscreen_on_request() {
    let dir = tempfile::tempdir().unwrap();
    // Only this test touches XDG_RUNTIME_DIR, which the compositor reads.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
    std::thread::spawn(|| {
        let options = Options { screen: (400, 300), ..Options::default() };
        let _ = macwl_compositor::run_with(Box::new(Nothing), options);
    });
    while !dir.path().join("wayland-mac").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-mac") };

    let conn = Connection::connect_to_env().unwrap();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut c = Client::default();
    queue.roundtrip(&mut c).unwrap();

    let surface = c.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = c.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut c).unwrap();
    use xdg_toplevel::State::*;
    let has = |c: &Client, state| c.configures.last().unwrap().1.contains(&state);
    assert_eq!(c.configures.last().unwrap().0, (400, 300));
    assert!(has(&c, Maximized) && has(&c, Activated) && !has(&c, Fullscreen));

    toplevel.set_fullscreen(None);
    queue.roundtrip(&mut c).unwrap();
    assert!(has(&c, Fullscreen) && has(&c, Maximized));
    toplevel.unset_fullscreen();
    queue.roundtrip(&mut c).unwrap();
    assert!(!has(&c, Fullscreen) && has(&c, Maximized));
    toplevel.unset_maximized();
    queue.roundtrip(&mut c).unwrap();
    assert!(has(&c, Maximized), "stays maximized");
    assert_eq!(c.configures.len(), 4);
}
