//! The game window: live keyboard and gamepad input, paced frames.
//! Never used by tests; see [`crate::run_headless`] for the scripted path.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use gilrs::Gilrs;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::config::RunConfig;
use crate::menu::{Action, Menu};
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
    let mut app = App {
        session,
        cfg: cfg.clone(),
        pacer: Pacer::new(cfg.frame_rate, Instant::now()),
        redraw: None,
        window: None,
        menu: None,
        keys: HashSet::new(),
        gilrs,
        named: Vec::new(),
        error: None,
        #[cfg(target_os = "android")]
        pad: Default::default(),
        away: false,
    };
    crate::one_window::run(&mut app)?;
    if let Some(e) = app.error {
        return Err(e);
    }
    app.session
        .finish()
        .map_err(|e| format!("writing the recording: {e}"))
}

/// The refresh rate of the display `window` is on, if the system says.
pub fn display_rate(window: &Window) -> Option<f64> {
    let mhz = window
        .current_monitor()
        .and_then(|m| m.refresh_rate_millihertz());
    // shortcut: Android does not say through winit; 120 Hz is what phones
    // that play games run at. Ask the display (ADisplay) if a 60 Hz phone
    // ever stutters.
    if cfg!(target_os = "android") && mhz.is_none() {
        return Some(120.0);
    }
    Some(f64::from(mhz?) / 1000.0)
}

struct App<G> {
    session: Session<G>,
    cfg: RunConfig,
    pacer: Pacer,
    /// Draws between game frames, when the refresh rate is above the game's.
    redraw: Option<Pacer>,
    window: Option<Arc<Window>>,
    menu: Option<Menu>,
    keys: HashSet<KeyCode>,
    gilrs: Option<Gilrs>,
    /// Controllers already named on the console.
    named: Vec<gilrs::GamepadId>,
    error: Option<String>,
    /// The app is out of sight (Android: the home screen or the app list):
    /// no frames go to the game, so it waits, paused.
    away: bool,
    /// A phone's controller, on the first port.
    #[cfg(target_os = "android")]
    pad: crate::android::Controller,
}

impl<G: Guest> App<G> {
    /// Applies the window settings: redraw rate, and the guest's picture.
    fn view(&mut self) {
        let display = self.window.as_deref().and_then(display_rate);
        let rate = self.cfg.window.redraw_rate(self.cfg.frame_rate, display);
        self.redraw = rate.map(|r| Pacer::new(r, Instant::now()));
        let blend = rate.map(|_| self.cfg.frame_rate);
        self.session.guest.view(&self.cfg.window, blend);
        if let Some(m) = &self.menu {
            m.show(&self.cfg.window);
        }
    }

    /// Carries out a menu choice and saves window settings.
    fn act(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let w = &mut self.cfg.window;
        match action {
            // The game ends and the program goes on in this same window.
            Action::OpenRom(rom) => {
                crate::one_window::switch(window, crate::one_window::Next::Rom(rom));
                event_loop.exit();
                return;
            }
            Action::Screen(page) => {
                crate::one_window::switch(window, crate::one_window::Next::Screen(page));
                event_loop.exit();
                return;
            }
            Action::ToggleMod(_) | Action::ToggleModOption(..) => {
                // Mods install at start, so the game starts again.
                let (change, path) = match &action {
                    Action::ToggleMod(p) => (std::fs::rename(p, crate::menu::toggled(p)), p),
                    Action::ToggleModOption(p, name) => (
                        std::fs::read_to_string(p).and_then(|t| {
                            std::fs::write(p, crate::menu::flip_mod_option(&t, name))
                        }),
                        p,
                    ),
                    _ => unreachable!(),
                };
                match change {
                    Ok(()) => {
                        crate::one_window::switch(window, crate::one_window::Next::Again);
                        event_loop.exit();
                    }
                    Err(e) => eprintln!("warning: turning mod {} on or off: {e}", path.display()),
                }
                return;
            }
            Action::OpenModsFolder => {
                if let Some(dir) = crate::menu::mods_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
                return;
            }
            Action::Refresh(r) => w.refresh = r,
            Action::Widescreen(on) => w.widescreen = on,
            Action::ConsoleFilter(on) => w.console_filter = on,
            Action::Scale(s) => {
                w.scale = s;
                window.set_fullscreen(None);
                let _ =
                    window.request_inner_size(LogicalSize::new(320.0 * s as f64, 240.0 * s as f64));
            }
            Action::Fullscreen => {
                let full = window.fullscreen().is_none();
                window.set_fullscreen(full.then_some(Fullscreen::Borderless(None)));
                return;
            }
        }
        self.view();
        if let Err(e) = self.cfg.save_window() {
            eprintln!("warning: saving window settings: {e}");
        }
    }

    fn live_pads(&mut self) -> Pads {
        let mut pads = Pads::default();
        let gilrs = &mut self.gilrs;
        if let Some(g) = gilrs.as_mut() {
            while g.next_event().is_some() {} // keeps gilrs' cached state current
        }
        let connected: Vec<_> = gilrs.iter().flat_map(|g| g.gamepads()).collect();
        // Name each controller once, so a person can see it was found.
        for (n, (id, gp)) in connected.iter().enumerate() {
            if !self.named.contains(id) {
                self.named.push(*id);
                println!("controller gamepad{n}: {}", gp.name());
            }
        }
        #[cfg(target_os = "android")]
        {
            pads[0] = pads[0].merge(self.pad.pad());
        }
        for (pad, port) in pads.iter_mut().zip(&self.cfg.ports) {
            if let Some(map) = &port.keyboard {
                *pad = pad.merge(map.pad(&self.keys));
            }
            for &n in &port.gamepads {
                if let Some((_, gp)) = connected.get(n) {
                    *pad = pad.merge(port.pad.pad(|b| gp.is_pressed(b), |a| gp.value(a)));
                }
            }
        }
        pads
    }
}

impl<G: Guest> ApplicationHandler for App<G> {
    // Each run of the shared event loop starts here; `resumed` may not come
    // again on a later run.
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if cause == winit::event::StartCause::Init && crate::one_window::ready() {
            self.resumed(event_loop);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            // Back from the home screen on Android: a new surface to draw on.
            if cfg!(target_os = "android") {
                self.session.guest.attach_window(w.clone());
                self.view();
                self.away = false;
            }
            return;
        }
        let scale = self.cfg.window.scale as f64;
        let size = LogicalSize::new(320.0 * scale, 240.0 * scale);
        // The start screen's window, when the game was picked there.
        let window = match crate::one_window::take() {
            Some(w) => {
                w.set_title(&self.cfg.window.title);
                if w.fullscreen().is_none() && !w.is_maximized() {
                    let _ = w.request_inner_size(size);
                }
                Ok(w)
            }
            None => {
                let attrs = Window::default_attributes()
                    .with_title(self.cfg.window.title.clone())
                    .with_theme(Some(winit::window::Theme::Dark))
                    .with_window_icon(crate::launcher::icon())
                    .with_inner_size(size);
                event_loop.create_window(attrs).map(Arc::new)
            }
        };
        match window {
            Ok(window) => {
                self.menu = Menu::new(&window, &self.cfg.window);
                self.session.guest.attach_window(window.clone());
                self.window = Some(window);
                self.view();
            }
            Err(e) => {
                self.error = Some(format!("opening the window: {e}"));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        #[cfg(target_os = "android")]
        if self.pad.event(&event) {
            if self.pad.wants_start_screen() {
                self.act(Action::Screen("games"), event_loop);
            }
            return;
        }
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
                if state == ElementState::Pressed && key == KeyCode::F11 {
                    self.act(Action::Fullscreen, event_loop);
                } else if state == ElementState::Pressed {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
            }
            // Key releases are lost while unfocused; never leave a key stuck down.
            WindowEvent::Focused(focused) => {
                // Key releases are lost while unfocused; never leave a key
                // stuck down. A phone pauses the game while it is away.
                if !focused {
                    self.keys.clear();
                }
                if cfg!(target_os = "android") {
                    self.away = !focused;
                }
            }
            _ => {}
        }
    }

    fn suspended(&mut self, _: &ActiveEventLoop) {
        self.away = true;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.away {
            // Paused: wait for the app to come back, keeping the game's
            // pace from jumping ahead to catch up afterwards.
            self.pacer = Pacer::new(self.cfg.frame_rate, Instant::now());
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if let (Some(e), Some(w)) = (crate::one_window::take_over(), self.window.clone()) {
            crate::one_window::switch(w, crate::one_window::Next::Crashed(e));
            event_loop.exit();
            return;
        }
        // The game asked for another one: stopped as a menu choice stops it.
        if let (Some(n), Some(w)) = (crate::one_window::take_request(), self.window.clone()) {
            crate::one_window::switch(w, n);
            event_loop.exit();
            return;
        }
        while let Some(a) = self.menu.as_ref().and_then(Menu::poll) {
            self.act(a, event_loop);
        }
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
        let mut next = self.pacer.next();
        if let Some(r) = &mut self.redraw {
            if r.due(Instant::now()) {
                self.session.guest.redraw();
            }
            next = next.min(r.next());
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(next));
    }
}
