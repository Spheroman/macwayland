//! Minimal xdg_shell client: one 640x480 wl_shm window with an animated gradient.
//! Built natively for macOS, so it exercises shm_open + fd passing on XNU.

use std::os::fd::AsFd;

use rustix::{
    fs::{Mode, ftruncate},
    mm::{MapFlags, ProtFlags, mmap},
    shm,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const W: i32 = 640;
const H: i32 = 480;

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    pixels: Option<&'static mut [u32]>,
    configured: bool,
    max_frames: u32,
    frame: u32,
    done: bool,
}

impl App {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let t = self.frame;
        let px = self.pixels.as_mut().unwrap();
        for y in 0..H as u32 {
            for x in 0..W as u32 {
                let r = (x * 255 / W as u32 + t * 2) & 0xff;
                let g = y * 255 / H as u32;
                let b = 255 - r;
                px[(y * W as u32 + x) as usize] = 0xff00_0000 | (r << 16) | (g << 8) | b;
            }
        }
        let surface = self.surface.as_ref().unwrap();
        surface.attach(self.buffer.as_ref(), 0, 0);
        surface.damage_buffer(0, 0, W, H);
        surface.frame(qh, ());
        surface.commit();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_compositor" => app.compositor = Some(registry.bind(name, 4, qh, ())),
                "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm_base = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if !app.configured {
                app.configured = true;
                println!("configured, drawing first frame");
                app.draw(qh);
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            app.frame += 1;
            if app.max_frames != 0 && app.frame >= app.max_frames {
                app.done = true;
            } else {
                app.draw(qh);
            }
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    // Usage: hello-client [frames]   (default 120; 0 = run until killed)
    let mut app = App {
        max_frames: std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(120),
        ..Default::default()
    };
    queue.roundtrip(&mut app)?;

    // macOS has no memfd_create; use a POSIX shm object, unlinked right away.
    // (macOS caps shm names at 31 chars.)
    let size = (W * H * 4) as usize;
    let name = format!("/macwl-{}", std::process::id());
    let fd = shm::open(&name, shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR, Mode::RUSR | Mode::WUSR)?;
    shm::unlink(&name)?;
    ftruncate(&fd, size as u64)?;
    let ptr = unsafe { mmap(std::ptr::null_mut(), size, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED, &fd, 0)? };
    app.pixels = Some(unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u32>(), (W * H) as usize) });

    let pool = app.shm.as_ref().expect("no wl_shm").create_pool(fd.as_fd(), size as i32, &qh, ());
    app.buffer = Some(pool.create_buffer(0, W, H, W * 4, wl_shm::Format::Xrgb8888, &qh, ()));

    let surface = app.compositor.as_ref().expect("no wl_compositor").create_surface(&qh, ());
    let xdg_surface = app.wm_base.as_ref().expect("no xdg_wm_base").get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("hello from macOS".into());
    surface.commit();
    app.surface = Some(surface);

    while !app.done {
        queue.blocking_dispatch(&mut app)?;
    }
    println!("rendered {} frames, exiting", app.frame);
    Ok(())
}
