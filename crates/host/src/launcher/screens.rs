//! The screens' window: opening it (or taking the shared one), showing the
//! canvas scaled up in whole pixels, and turning keys, the pointer, taps and
//! an Android controller into [`Ui`] actions.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Theme, Window, WindowId};

use super::icon::icon;
use super::layout::{H, W};
use super::palette::BG;
use super::ui::{Hit, Ui};

/// A new window is this many times the canvas.
const START_SCALE: f64 = 3.0;
/// The window's title.
const TITLE: &str = "N64Rusty";

/// The canvas's place in the window: window width and height, whole-pixel
/// scale, and the left and top offsets that centre it.
struct Fit {
    ww: usize,
    wh: usize,
    k: usize,
    ox: usize,
    oy: usize,
}

/// The screens in their window.
pub(super) struct Screens {
    ui: Ui,
    /// Closing the message closes the program (an error screen).
    message_only: bool,
    pub(super) window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    cursor: Option<(isize, isize)>,
    pub(super) picked: Option<PathBuf>,
    /// A window opened and could be drawn in.
    pub(super) shown: bool,
    /// The player closed the window.
    pub(super) closed: bool,
    #[cfg(target_os = "android")]
    pad: crate::android::Controller,
    /// On Android the screen draws through the GPU, as the game does: a
    /// window the CPU has drawn into cannot be the GPU's afterwards.
    #[cfg(target_os = "android")]
    gpu: Option<(n64rusty_gfx::Renderer, n64rusty_gfx::WindowTarget)>,
}

impl Screens {
    /// The screens for `ui`; with `message_only`, closing its message ends
    /// the program's screens.
    pub(super) fn new(ui: Ui, message_only: bool) -> Screens {
        Screens {
            ui,
            message_only,
            window: None,
            surface: None,
            cursor: None,
            picked: None,
            shown: false,
            closed: false,
            #[cfg(target_os = "android")]
            pad: Default::default(),
            #[cfg(target_os = "android")]
            gpu: None,
        }
    }

    /// Stops drawing into the window, so the next screen can.
    pub(super) fn let_go(&mut self) {
        self.surface = None;
        #[cfg(target_os = "android")]
        {
            self.gpu = None;
        }
    }

    /// Canvas scale and offset for the window's size.
    fn fit(&self) -> Fit {
        let size = self
            .window
            .as_ref()
            .map(|w| w.inner_size())
            .unwrap_or_default();
        let (ww, wh) = (size.width.max(1) as usize, size.height.max(1) as usize);
        let k = (ww / W).min(wh / H).max(1);
        Fit {
            ww,
            wh,
            k,
            ox: ww.saturating_sub(W * k) / 2,
            oy: wh.saturating_sub(H * k) / 2,
        }
    }

    fn activate(&mut self, event_loop: &ActiveEventLoop) {
        if self.ui.message.take().is_some() {
            if self.message_only {
                event_loop.exit();
            }
            return;
        }
        if let Some(rom) = self.ui.choose() {
            self.picked = Some(rom);
            event_loop.exit();
        }
    }

    fn redraw(&mut self) {
        #[cfg(target_os = "android")]
        if self.gpu.is_some() {
            return self.redraw_gpu();
        }
        let Fit { ww, wh, k, ox, oy } = self.fit();
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(w), Some(h)) = (NonZeroU32::new(ww as u32), NonZeroU32::new(wh as u32)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        let canvas = self.ui.draw();
        buf.fill(BG);
        for y in 0..(H * k).min(wh) {
            let row = &canvas[(y / k) * W..(y / k + 1) * W];
            let out = &mut buf[(oy + y) * ww..(oy + y + 1) * ww];
            for x in 0..(W * k).min(ww) {
                out[ox + x] = row[x / k];
            }
        }
        let _ = buf.present();
    }

    /// Draws through the GPU (Android).
    #[cfg(target_os = "android")]
    fn redraw_gpu(&mut self) {
        let Fit { ww, wh, k, ox, oy } = self.fit();
        let Some((renderer, target)) = self.gpu.as_mut() else {
            return;
        };
        let rgba: Vec<u8> = self
            .ui
            .draw()
            .iter()
            .flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8, 255])
            .collect();
        let rect = (ox as u32, oy as u32, (W * k) as u32, (H * k) as u32);
        let r = renderer.present_pixels(
            &rgba,
            (W as u32, H as u32),
            target,
            ww as u32,
            wh as u32,
            rect,
        );
        if let Err(e) = r {
            eprintln!("warning: start screen not shown: {e}");
        }
    }

    /// The shared window, or a new one; `None` (after saying why) when none
    /// could open.
    fn open_window(event_loop: &ActiveEventLoop) -> Option<Arc<Window>> {
        if let Some(w) = crate::one_window::take() {
            return Some(w);
        }
        let attrs = Window::default_attributes()
            .with_title(TITLE)
            .with_theme(Some(Theme::Dark))
            .with_window_icon(icon())
            .with_inner_size(LogicalSize::new(
                W as f64 * START_SCALE,
                H as f64 * START_SCALE,
            ));
        match event_loop.create_window(attrs) {
            Ok(w) => Some(Arc::new(w)),
            Err(e) => {
                eprintln!("error: the start screen's window did not open: {e}");
                None
            }
        }
    }

    /// Starts drawing into `window` with the CPU; exits when it cannot.
    #[cfg(not(target_os = "android"))]
    fn start_drawing(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()));
        match surface {
            Ok(s) => {
                self.surface = Some(s);
                self.shown = true;
            }
            Err(e) => {
                eprintln!("error: the start screen cannot draw: {e}");
                event_loop.exit();
            }
        }
    }

    /// Starts drawing into `window` with the GPU; exits when it cannot.
    #[cfg(target_os = "android")]
    fn start_drawing(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let gpu = n64rusty_gfx::Renderer::new().and_then(|r| {
            let t = r.window_target(window.clone())?;
            Ok((r, t))
        });
        match gpu {
            Ok(g) => {
                self.gpu = Some(g);
                self.shown = true;
            }
            Err(e) => {
                eprintln!("error: the start screen cannot draw: {e}");
                event_loop.exit();
            }
        }
    }

    /// A key press; true if the screen changed.
    fn key(&mut self, event_loop: &ActiveEventLoop, key: KeyCode) -> bool {
        match key {
            KeyCode::Escape if self.ui.message.is_some() => self.activate(event_loop),
            KeyCode::Escape => event_loop.exit(),
            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => self.activate(event_loop),
            _ if self.ui.message.is_none() => self.ui.key(key),
            _ => return false,
        }
        true
    }

    /// The pointer moved to `position`; true if the selection changed.
    fn point(&mut self, position: winit::dpi::PhysicalPosition<f64>) -> bool {
        let Fit { k, ox, oy, .. } = self.fit();
        let x = (position.x as isize - ox as isize) / k as isize;
        let y = (position.y as isize - oy as isize) / k as isize;
        self.cursor = Some((x, y));
        match self.ui.hit(x, y) {
            Some(Hit::Item(i)) if self.ui.message.is_none() && i != self.ui.sel => {
                self.ui.sel = i;
                true
            }
            _ => false,
        }
    }

    /// A click (or a tap, which picks an item at once); true if the screen
    /// changed.
    fn click(&mut self, event_loop: &ActiveEventLoop, tap: bool) -> bool {
        match self.cursor.and_then(|(x, y)| self.ui.hit(x, y)) {
            _ if self.ui.message.is_some() => self.activate(event_loop),
            Some(Hit::Tab(page)) => self.ui.switch(page),
            Some(Hit::Item(i)) if i == self.ui.sel || tap => {
                self.ui.sel = i;
                self.activate(event_loop)
            }
            _ => return false,
        }
        true
    }

    fn changed(&self, changed: bool) {
        if changed {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

impl ApplicationHandler for Screens {
    // Each run of the shared event loop starts here; `resumed` may not come
    // again on a later run.
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if cause == winit::event::StartCause::Init && crate::one_window::ready() {
            self.resumed(event_loop);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Back from the home screen on Android: draw on the new surface.
        #[cfg(target_os = "android")]
        if let Some(w) = self.window.take() {
            self.let_go();
            crate::one_window::keep(w);
        }
        if self.window.is_some() {
            return;
        }
        let Some(window) = Self::open_window(event_loop) else {
            event_loop.exit();
            return;
        };
        window.set_title(TITLE);
        self.start_drawing(event_loop, &window);
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        // A phone: its controller drives the keys, a finger the mouse.
        #[cfg(target_os = "android")]
        if let Some(key) = self.pad.menu_key(&event) {
            let changed = self.key(event_loop, key);
            return self.changed(changed);
        }
        #[cfg(target_os = "android")]
        if let WindowEvent::Touch(t) = &event {
            let changed = match t.phase {
                winit::event::TouchPhase::Started => self.point(t.location),
                winit::event::TouchPhase::Ended => {
                    self.point(t.location);
                    self.click(event_loop, true)
                }
                _ => false,
            };
            return self.changed(changed);
        }
        let changed = match event {
            WindowEvent::CloseRequested => {
                self.closed = true;
                event_loop.exit();
                true
            }
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => {
                self.redraw();
                false
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.key(event_loop, key),
            WindowEvent::CursorMoved { position, .. } => self.point(position),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.click(event_loop, false),
            _ => false,
        };
        self.changed(changed);
    }
}
