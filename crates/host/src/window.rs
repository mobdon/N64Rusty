//! The game window: live keyboard and gamepad input, paced frames.
//! Never used by tests; see [`crate::run_headless`] for the scripted path.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use gilrs::Gilrs;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::config::RunConfig;
use crate::mapping::gamepad_pad;
use crate::pad::Pads;
use crate::session::{Guest, Pacer, Session, Tick};

/// Opens a window and runs the game at the configured frame rate until the
/// window is closed, the frame limit is reached or the debug port quits.
/// With `headless = true` in the config it opens nothing and runs
/// [`crate::run_headless`] instead.
pub fn run_windowed<G: Guest>(guest: G, cfg: &RunConfig) -> Result<G, String> {
    if cfg.headless {
        return crate::run_headless(guest, cfg);
    }
    let session = Session::new(guest, cfg)?;
    if let Some(addr) = session.debug_addr() {
        eprintln!("debug port listening on {addr}");
    }
    let gilrs = Gilrs::new()
        .map_err(|e| eprintln!("gamepads unavailable: {e}"))
        .ok();
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    let mut app = App {
        session,
        cfg: cfg.clone(),
        pacer: Pacer::new(cfg.frame_rate, Instant::now()),
        window: None,
        keys: HashSet::new(),
        gilrs,
        error: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    if let Some(e) = app.error {
        return Err(e);
    }
    app.session
        .finish()
        .map_err(|e| format!("writing the recording: {e}"))
}

struct App<G> {
    session: Session<G>,
    cfg: RunConfig,
    pacer: Pacer,
    window: Option<Arc<Window>>,
    keys: HashSet<KeyCode>,
    gilrs: Option<Gilrs>,
    error: Option<String>,
}

impl<G: Guest> App<G> {
    fn live_pads(&mut self) -> Pads {
        let mut pads = Pads::default();
        let gilrs = &mut self.gilrs;
        if let Some(g) = gilrs.as_mut() {
            while g.next_event().is_some() {} // keeps gilrs' cached state current
        }
        let connected: Vec<_> = gilrs.iter().flat_map(|g| g.gamepads()).collect();
        for (pad, port) in pads.iter_mut().zip(&self.cfg.ports) {
            if let Some(map) = &port.keyboard {
                *pad = pad.merge(map.pad(&self.keys));
            }
            for &n in &port.gamepads {
                if let Some((_, gp)) = connected.get(n) {
                    *pad = pad.merge(gamepad_pad(|b| gp.is_pressed(b), |a| gp.value(a)));
                }
            }
        }
        pads
    }
}

impl<G: Guest> ApplicationHandler for App<G> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let scale = self.cfg.window.scale as f64;
        let attrs = Window::default_attributes()
            .with_title(self.cfg.window.title.clone())
            .with_inner_size(LogicalSize::new(320.0 * scale, 240.0 * scale));
        match event_loop.create_window(attrs) {
            Ok(window) => {
                let window = Arc::new(window);
                self.session.guest.attach_window(window.clone());
                self.window = Some(window);
            }
            Err(e) => {
                self.error = Some(format!("opening the window: {e}"));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key),
                        state,
                        ..
                    },
                ..
            } => {
                if state == ElementState::Pressed {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
            }
            // Key releases are lost while unfocused; never leave a key stuck down.
            WindowEvent::Focused(false) => self.keys.clear(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.pacer.due(Instant::now()) {
            let live = self.live_pads();
            if self.session.tick(&live, false) == Tick::Done {
                event_loop.exit();
                return;
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.pacer.next()));
    }
}
