//! Android: the app's files folder, its event loop, and controllers.
//!
//! winit hands an Android controller's buttons over as unidentified keys
//! with Android key codes, and its left stick as touch movement in the
//! range -1 to 1 (the joystick's X and Y axes). The right stick and analog
//! triggers do not reach the app, so C-buttons sit on spare buttons.

use std::path::PathBuf;
use std::sync::OnceLock;

use winit::event::{ElementState, KeyEvent, TouchPhase, WindowEvent};
use winit::keyboard::{KeyCode, NativeKeyCode, PhysicalKey};
pub use winit::platform::android::activity::AndroidApp;

use crate::pad::{button, Pad, STICK_MAX};

static APP: OnceLock<AndroidApp> = OnceLock::new();
static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Called first by the app: its activity and its files folder.
pub fn start(app: AndroidApp, home: PathBuf) {
    let _ = APP.set(app);
    let _ = HOME.set(home);
}

/// A folder only the app can see, where it may run code from: the shared
/// files folder (ROMs, mods) does not allow that.
pub fn private_dir() -> Option<PathBuf> {
    APP.get()?.internal_data_path()
}

pub(crate) fn app() -> AndroidApp {
    APP.get().expect("android_main sets the app first").clone()
}

pub(crate) fn home() -> Option<PathBuf> {
    HOME.get().cloned()
}

// Android key codes (android.view.KeyEvent).
const BACK: u32 = 4;
const DPAD_UP: u32 = 19;
const DPAD_DOWN: u32 = 20;
const DPAD_LEFT: u32 = 21;
const DPAD_RIGHT: u32 = 22;
const BUTTON_A: u32 = 96;
const BUTTON_B: u32 = 97;
const BUTTON_X: u32 = 99;
const BUTTON_Y: u32 = 100;
const BUTTON_L1: u32 = 102;
const BUTTON_R1: u32 = 103;
const BUTTON_L2: u32 = 104;
const BUTTON_R2: u32 = 105;
const BUTTON_THUMBL: u32 = 106;
const BUTTON_THUMBR: u32 = 107;
const BUTTON_START: u32 = 108;
const BUTTON_SELECT: u32 = 109;

/// The console button for a controller button, laid out as on an Xbox
/// style pad: A is A, X is B, LT is Z, RB and RT are R; the C-buttons go on
/// Y (left), B (down), LB (right) and the right stick click (up).
fn n64_button(code: u32) -> u16 {
    match code {
        BUTTON_A => button::A,
        BUTTON_X => button::B,
        BUTTON_L2 => button::Z,
        BUTTON_START => button::START,
        DPAD_UP => button::D_UP,
        DPAD_DOWN => button::D_DOWN,
        DPAD_LEFT => button::D_LEFT,
        DPAD_RIGHT => button::D_RIGHT,
        BUTTON_THUMBL | BUTTON_SELECT => button::L,
        BUTTON_R1 | BUTTON_R2 => button::R,
        BUTTON_Y => button::C_LEFT,
        BUTTON_B | BACK => button::C_DOWN,
        BUTTON_L1 => button::C_RIGHT,
        BUTTON_THUMBR => button::C_UP,
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

/// Stick values below this count as centred.
const DEAD: f64 = 0.15;

/// One Android controller as the window sees it.
#[derive(Default)]
pub struct Controller {
    buttons: u16,
    x: f64,
    y: f64,
    /// The stick direction last turned into a start screen key.
    menu_dir: Option<KeyCode>,
}

impl Controller {
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
                let b = n64_button(code);
                if *state == ElementState::Pressed {
                    self.buttons |= b;
                } else {
                    self.buttons &= !b;
                }
                true
            }
            WindowEvent::Touch(t) if is_stick(t.phase, t.location.x, t.location.y) => {
                (self.x, self.y) = (t.location.x, t.location.y);
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
        Pad {
            buttons: self.buttons,
            x: axis(self.x),
            // Android's Y grows downwards; the console's upwards.
            y: axis(-self.y),
        }
    }

    /// A start screen key for this event: a pressed button, or the stick
    /// newly pushed one way.
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
            WindowEvent::Touch(t) if is_stick(t.phase, t.location.x, t.location.y) => {
                let (x, y) = (t.location.x, t.location.y);
                let dir = if y < -0.5 {
                    Some(KeyCode::KeyW)
                } else if y > 0.5 {
                    Some(KeyCode::KeyS)
                } else if x < -0.5 {
                    Some(KeyCode::KeyA)
                } else if x > 0.5 {
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
    fn controller_buttons_map_to_console_buttons() {
        assert_eq!(n64_button(BUTTON_A), button::A);
        assert_eq!(n64_button(BUTTON_X), button::B);
        assert_eq!(n64_button(BUTTON_L2), button::Z);
        assert_eq!(n64_button(12345), 0);
    }

    #[test]
    fn the_stick_is_told_apart_from_a_finger() {
        assert!(is_stick(TouchPhase::Moved, 0.4, -1.0));
        assert!(!is_stick(TouchPhase::Moved, 400.0, 300.0));
        assert!(!is_stick(TouchPhase::Started, 0.0, 0.0));
    }
}
