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
const BUTTON_START: u32 = 108;
const BUTTON_SELECT: u32 = 109;

/// Touch ids of the axes the patched winit passes on.
const RIGHT_STICK: u64 = u64::MAX;
const TRIGGERS: u64 = u64::MAX - 1;
const HAT: u64 = u64::MAX - 2;

/// The console button for a controller button, as on a computer (see
/// `mapping::PadMap::default`): A is A, B is B, X and Y are C-down and
/// C-up, LT and RB are Z, LB is L, RT is R.
fn n64_button(code: u32) -> u16 {
    match code {
        BUTTON_A => button::A,
        BUTTON_B => button::B,
        BUTTON_X => button::C_DOWN,
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

/// Stick values below this count as centred.
const DEAD: f64 = 0.15;
/// How far the right stick, a trigger or the D-pad axis goes before it
/// counts as a press.
const PRESS: f64 = 0.5;

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
        assert_eq!(n64_button(BUTTON_X), button::C_DOWN);
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
