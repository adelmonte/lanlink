//! `lanlink border`: a click-through coloured frame around the screen, shown
//! while input is being forwarded. Uses wlr-layer-shell (niri, sway, Hyprland...).
//! Runs as its own process so a missing Wayland session can't hurt the daemon.

use crate::config::Config;
use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};

struct Border {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    color: u32,
    width: u32,
    exit: bool,
}

pub fn run() -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let conn = Connection::connect_to_env().context("no Wayland session")?;
    let (globals, mut queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("compositor has no layer-shell")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;

    let surface = compositor.create_surface(&qh);
    // Empty input region: clicks and focus go straight through.
    let region = Region::new(&compositor)?;
    surface.set_input_region(Some(region.wl_region()));
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("lanlink-border"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(0, 0);
    layer.commit();

    let (r, g, b) = cfg.border_rgb();
    let mut app = Border {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        pool: SlotPool::new(4096, &shm)?,
        shm,
        layer,
        color: 0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32,
        width: cfg.border_width.max(1),
        exit: false,
    };
    while !app.exit {
        queue.blocking_dispatch(&mut app)?;
    }
    Ok(())
}

impl Border {
    fn draw(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let Ok((buffer, canvas)) = self.pool.create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888) else {
            return;
        };
        let bw = self.width;
        let color = self.color.to_le_bytes();
        for (i, px) in canvas.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let edge = x < bw || y < bw || x >= w.saturating_sub(bw) || y >= h.saturating_sub(bw);
            px.copy_from_slice(if edge { &color } else { &[0, 0, 0, 0] });
        }
        let s = self.layer.wl_surface();
        s.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(s).is_ok() {
            self.layer.commit();
        }
    }
}

impl CompositorHandler for Border {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Border {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Border {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, c: LayerSurfaceConfigure, _: u32) {
        self.draw(c.new_size.0, c.new_size.1);
    }
}

impl ShmHandler for Border {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Border {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(Border);
smithay_client_toolkit::delegate_dispatch2!(Border);
