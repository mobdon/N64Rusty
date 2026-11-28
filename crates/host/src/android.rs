//! Android: the app's files folder, its event loop, and controllers.
//!
//! winit hands an Android controller's buttons over as unidentified keys
//! with Android key codes (the D-pad of some as arrow keys), and its left
//! stick as touch movement in the range -1 to 1. The right stick, the
//! triggers and a D-pad that is an axis come the same way, from the
//! project's patched winit (vendor/winit), with their own touch ids.

use std::path::PathBuf;
use std::sync::OnceLock;

use winit::event::{ElementState, KeyEvent, TouchPhase, WindowEvent};
use winit::keyboard::{KeyCode, NativeKeyCode, PhysicalKey};
/// The app's activity, which `android_main` is handed.
pub use winit::platform::android::activity::AndroidApp;
use winit::platform::android::activity::WindowManagerFlags;

use crate::mapping::{DEADZONE as DEAD, PRESS};
use crate::pad::{button, Pad, STICK_MAX};

static APP: OnceLock<AndroidApp> = OnceLock::new();
static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Called first by the app: its activity and its files folder.
pub fn start(app: AndroidApp, home: PathBuf) {
    // No status bar over the picture (it overlaps it on the cover screen),
    // and no dimming during long play without touching the screen.
    app.set_window_flags(
        WindowManagerFlags::FULLSCREEN | WindowManagerFlags::KEEP_SCREEN_ON,
        WindowManagerFlags::empty(),
    );
    let _ = APP.set(app);
    let _ = HOME.set(home);
}

/// A folder only the app can see, where it may run code from: the shared
/// files folder (ROMs, mods) does not allow that.
pub fn private_dir() -> Option<PathBuf> {
    APP.get()?.internal_data_path()
}

/// `ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT`: the rate is what the
/// pictures need; the display may run at a multiple of it.
const FRAME_RATE_COMPATIBILITY_DEFAULT: i8 = 0;

/// `ANativeWindow_setFrameRate(window, frameRate, compatibility)`, from
/// Android 11 (API 30) on.
type SetFrameRate = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8) -> i32;

/// Asks the display to run at `rate` pictures a second (the game's rate,
/// or the redraw rate when blending) while `window` is shown, so a 120 Hz
/// screen can drop to 60 and save battery. Called after each attach of
/// the window (its surface), as the request belongs to that surface. Older Android has no such call: it is
/// looked up at run time, since linking it would stop the app loading
/// there, and the request is then skipped.
pub fn ask_frame_rate(window: &winit::window::Window, rate: f64) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AndroidNdk(native) = handle.as_raw() else {
        return;
    };
    // SAFETY: plain C strings. libandroid.so is already loaded (winit's
    // activity needs it), so RTLD_NOLOAD only takes a reference to it,
    // given back below.
    let lib = unsafe {
        libc::dlopen(
            c"libandroid.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_NOLOAD,
        )
    };
    if lib.is_null() {
        return;
    }
    // SAFETY: `lib` is a live handle and the name a C string.
    let sym = unsafe { libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr()) };
    if !sym.is_null() {
        // SAFETY: the symbol is ANativeWindow_setFrameRate, of this type;
        // the window pointer stays valid while `handle` borrows the window.
        let status = unsafe {
            let set = std::mem::transmute::<*mut std::ffi::c_void, SetFrameRate>(sym);
            set(
                native.a_native_window.as_ptr(),
                rate as f32,
                FRAME_RATE_COMPATIBILITY_DEFAULT,
            )
        };
        if status != 0 {
            eprintln!("warning: asking the display for {rate} Hz failed: error {status}");
        }
    }
    // SAFETY: the reference taken above; the library stays loaded.
    unsafe { libc::dlclose(lib) };
}

pub(crate) fn app() -> AndroidApp {
    APP.get().expect("android_main sets the app first").clone()
}

pub(crate) fn home() -> Option<PathBuf> {
    HOME.get().cloned()
}

// Android key codes (android.view.KeyEvent).
const DPAD_UP: u32 = 19;
const DPAD_DOWN: u32 = 20;
const DPAD_LEFT: u32 = 21;
const DPAD_RIGHT: u32 = 22;
const BUTTON_A: u32 = 96;
const BUTTON_B: u32 = 97;
// Maps to nothing (see `n64_button`); named for the tests.
#[allow(dead_code)]
const BUTTON_X: u32 = 99;
const BUTTON_Y: u32 = 100;
const BUTTON_L1: u32 = 102;
const BUTTON_R1: u32 = 103;
const BUTTON_L2: u32 = 104;
const BUTTON_R2: u32 = 105;
const BUTTON_START: u32 = 108;
const BUTTON_SELECT: u32 = 109;

/// Touch ids of the axes the patched winit passes on.
const RIGHT_STICK: u64 = u64::MAX;
const TRIGGERS: u64 = u64::MAX - 1;
const HAT: u64 = u64::MAX - 2;

/// The console button for a controller button: A is A, B is B, LT and RB
/// are Z, LB is L, RT is R, Y is C-up (first-person view); X does nothing:
/// the right stick is the C-buttons (the owner's choice).
fn n64_button(code: u32) -> u16 {
    match code {
        BUTTON_A => button::A,
        BUTTON_B => button::B,
        BUTTON_Y => button::C_UP,
        BUTTON_L2 | BUTTON_R1 => button::Z,
        BUTTON_L1 => button::L,
        BUTTON_R2 => button::R,
        BUTTON_START => button::START,
        DPAD_UP => button::D_UP,
        DPAD_DOWN => button::D_DOWN,
        DPAD_LEFT => button::D_LEFT,
        DPAD_RIGHT => button::D_RIGHT,
        _ => 0,
    }
}

/// The start screen's key for a controller button.
fn menu_key(code: u32) -> Option<KeyCode> {
    Some(match code {
        DPAD_UP => KeyCode::KeyW,
        DPAD_DOWN => KeyCode::KeyS,
        DPAD_LEFT => KeyCode::KeyA,
        DPAD_RIGHT => KeyCode::KeyD,
        BUTTON_A | BUTTON_START => KeyCode::Enter,
        BUTTON_L1 => KeyCode::KeyQ,
        BUTTON_R1 => KeyCode::KeyE,
        _ => return None,
    })
}

// Stick values below `DEAD` count as centred (per axis here). The right
// stick, a trigger or the D-pad axis counts as a press from `PRESS`.

/// One Android controller as the window sees it.
#[derive(Default)]
pub struct Controller {
    buttons: u16,
    left: (f64, f64),
    right: (f64, f64),
    triggers: (f64, f64),
    hat: (f64, f64),
    /// The direction last turned into a start screen key.
    menu_dir: Option<KeyCode>,
    /// The View button was pressed: the start screen, in place of
    /// the menu bar a computer has.
    start_screen: bool,
}

impl Controller {
    /// Whether View asked for the start screen since last asked.
    pub fn wants_start_screen(&mut self) -> bool {
        std::mem::take(&mut self.start_screen)
    }

    /// Takes in a window event; true if it was the controller's.
    pub fn event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key,
                        state,
                        ..
                    },
                ..
            } => {
                let Some(code) = android_code(physical_key) else {
                    return false;
                };
                // Not Back: Android also sends it for a controller's B.
                if code == BUTTON_SELECT && *state == ElementState::Pressed {
                    self.start_screen = true;
                }
                let b = n64_button(code);
                if *state == ElementState::Pressed {
                    self.buttons |= b;
                } else {
                    self.buttons &= !b;
                }
                true
            }
            WindowEvent::Touch(t) if is_stick(t.phase, t.location.x, t.location.y) => {
                let at = (t.location.x, t.location.y);
                match t.id {
                    RIGHT_STICK => self.right = at,
                    TRIGGERS => self.triggers = at,
                    HAT => self.hat = at,
                    _ => self.left = at,
                }
                true
            }
            _ => false,
        }
    }

    /// The controller as a console pad.
    pub fn pad(&self) -> Pad {
        let axis = |v: f64| {
            if v.abs() < DEAD {
                0
            } else {
                (v.clamp(-1.0, 1.0) * f64::from(STICK_MAX)) as i8
            }
        };
        // Pushed one way: (negative, positive) buttons.
        let dir = |v: f64, (minus, plus): (u16, u16)| {
            if v <= -PRESS {
                minus
            } else if v >= PRESS {
                plus
            } else {
                0
            }
        };
        let (rx, ry) = self.right;
        let (hx, hy) = self.hat;
        let (lt, rt) = self.triggers;
        let buttons = self.buttons
            | dir(rx, (button::C_LEFT, button::C_RIGHT))
            | dir(ry, (button::C_UP, button::C_DOWN))
            | dir(hx, (button::D_LEFT, button::D_RIGHT))
            | dir(hy, (button::D_UP, button::D_DOWN))
            | dir(lt, (0, button::Z))
            | dir(rt, (0, button::R));
        Pad {
            buttons,
            x: axis(self.left.0),
            // Android's Y grows downwards; the console's upwards.
            y: axis(-self.left.1),
        }
    }

    /// A start screen key for this event: a pressed button, or the left
    /// stick or D-pad newly pushed one way.
    pub fn menu_key(&mut self, event: &WindowEvent) -> Option<KeyCode> {
        match event {
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => android_code(physical_key).and_then(menu_key),
            WindowEvent::Touch(t)
                if is_stick(t.phase, t.location.x, t.location.y)
                    && !matches!(t.id, RIGHT_STICK | TRIGGERS) =>
            {
                let (x, y) = (t.location.x, t.location.y);
                let dir = if y <= -PRESS {
                    Some(KeyCode::KeyW)
                } else if y >= PRESS {
                    Some(KeyCode::KeyS)
                } else if x <= -PRESS {
                    Some(KeyCode::KeyA)
                } else if x >= PRESS {
                    Some(KeyCode::KeyD)
                } else {
                    None
                };
                let new = dir.filter(|d| self.menu_dir != Some(*d));
                self.menu_dir = dir;
                new
            }
            _ => None,
        }
    }
}

/// The Android key code of a controller key: buttons arrive unidentified,
/// the D-pad as arrow keys.
fn android_code(key: &PhysicalKey) -> Option<u32> {
    match key {
        PhysicalKey::Unidentified(NativeKeyCode::Android(code)) => Some(*code),
        PhysicalKey::Code(KeyCode::ArrowUp) => Some(DPAD_UP),
        PhysicalKey::Code(KeyCode::ArrowDown) => Some(DPAD_DOWN),
        PhysicalKey::Code(KeyCode::ArrowLeft) => Some(DPAD_LEFT),
        PhysicalKey::Code(KeyCode::ArrowRight) => Some(DPAD_RIGHT),
        _ => None,
    }
}

/// A joystick's movement arrives as touch movement within -1 to 1; a finger
/// on the screen is at pixel positions.
fn is_stick(phase: TouchPhase, x: f64, y: f64) -> bool {
    phase == TouchPhase::Moved && x.abs() <= 1.0 && y.abs() <= 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_buttons_map_as_on_a_computer() {
        assert_eq!(n64_button(BUTTON_A), button::A);
        assert_eq!(n64_button(BUTTON_B), button::B);
        assert_eq!(n64_button(BUTTON_X), 0);
        assert_eq!(n64_button(BUTTON_Y), button::C_UP);
        assert_eq!(n64_button(BUTTON_R1), button::Z);
        assert_eq!(n64_button(12345), 0);
    }

    #[test]
    fn the_right_stick_triggers_and_d_pad_press_buttons() {
        let mut c = Controller {
            right: (-0.9, 0.0),
            triggers: (1.0, 0.2),
            hat: (0.0, 1.0),
            ..Default::default()
        };
        assert_eq!(c.pad().buttons, button::C_LEFT | button::Z | button::D_DOWN);
        c.right = (0.0, -0.8);
        c.triggers = (0.0, 0.9);
        assert_eq!(
            c.pad().buttons & (button::C_UP | button::R),
            button::C_UP | button::R
        );
    }

    #[test]
    fn the_stick_is_told_apart_from_a_finger() {
        assert!(is_stick(TouchPhase::Moved, 0.4, -1.0));
        assert!(!is_stick(TouchPhase::Moved, 400.0, 300.0));
        assert!(!is_stick(TouchPhase::Started, 0.0, 0.0));
    }
}
