//! The game window: winit, paced at the run's frame rate, reading keys and
//! gamepads. The graphics side gets the window each frame (an `Arc`, so it
//! can create a wgpu surface from it). Tests never open it; they use
//! [`crate::run_headless`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
pub use winit::window::Window;
use winit::window::WindowId;

use crate::config::RunConfig;
use crate::keymap::KeyMap;
use crate::pacer::Pacer;
use crate::pad::Pads;
use crate::session::Session;

/// Open the window and run until it closes or the run ends. `game` runs once
/// per frame with the frame number, its input and the window. Returns the
/// number of frames run.
pub fn run_window(
    cfg: &RunConfig,
    game: impl FnMut(u64, &Pads, &Arc<Window>),
) -> Result<u64, String> {
    let mut app = App {
        session: Some(Session::new(cfg)?),
        keys: KeyMap::from_config(&cfg.keys)?,
        held: HashSet::new(),
        #[cfg(feature = "gamepad")]
        gamepads: if cfg.gamepads {
            crate::gamepad::Gamepads::new()
        } else {
            None
        },
        pacer: Pacer::new(cfg.rate_hz),
        deadline: Instant::now(),
        window: None,
        game,
        error: None,
    };
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    if let Some(e) = app.error {
        return Err(e);
    }
    app.session.take().map_or(Ok(0), Session::finish)
}

struct App<F> {
    session: Option<Session>,
    keys: KeyMap,
    held: HashSet<String>,
    #[cfg(feature = "gamepad")]
    gamepads: Option<crate::gamepad::Gamepads>,
    pacer: Pacer,
    deadline: Instant,
    window: Option<Arc<Window>>,
    game: F,
    error: Option<String>,
}

impl<F: FnMut(u64, &Pads, &Arc<Window>)> ApplicationHandler for App<F> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("n64rusty")
            .with_inner_size(LogicalSize::new(640.0, 480.0));
        match el.create_window(attrs) {
            Ok(w) => self.window = Some(Arc::new(w)),
            Err(e) => {
                self.error = Some(e.to_string());
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            // Keys released while unfocused never arrive; drop them all.
            WindowEvent::Focused(false) => self.held.clear(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let name = format!("{code:?}");
                    if event.state == ElementState::Pressed {
                        self.held.insert(name);
                    } else {
                        self.held.remove(&name);
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let (Some(window), Some(session)) = (&self.window, &mut self.session) else {
            return;
        };
        let now = Instant::now();
        if now >= self.deadline {
            #[allow(unused_mut)]
            let mut live = self.keys.pads(&self.held);
            #[cfg(feature = "gamepad")]
            if let Some(g) = &mut self.gamepads {
                g.add_to(&mut live);
            }
            match session.next_frame(live) {
                Ok(Some(pads)) => {
                    (self.game)(session.frame() - 1, &pads, window);
                    window.request_redraw();
                    self.deadline = self.pacer.tick(now);
                }
                Ok(None) => return el.exit(),
                Err(e) => {
                    self.error = Some(e);
                    return el.exit();
                }
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.deadline));
    }
}
