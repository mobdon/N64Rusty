//! Host devices to controller state: keyboard keys and gamepads.

use std::collections::HashSet;

use gilrs::{Axis, Button};
use serde::de::{value::StrDeserializer, Deserialize};
use winit::keyboard::KeyCode;

use crate::pad::{button, Pad, STICK_MAX};

/// What a key does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binding {
    /// Holds these console button bits (see [`button`]).
    Button(u16),
    /// Pushes the stick fully up.
    StickUp,
    /// Pushes the stick fully down.
    StickDown,
    /// Pushes the stick fully left.
    StickLeft,
    /// Pushes the stick fully right.
    StickRight,
    /// Held with a stick key: half tilt, for walking rather than running.
    Half,
}

impl Binding {
    /// Parses a button name (see [`button::NAMES`]) or one of `STICK_UP`,
    /// `STICK_DOWN`, `STICK_LEFT`, `STICK_RIGHT` and `HALF`.
    pub fn parse(name: &str) -> Result<Binding, String> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "STICK_UP" => Binding::StickUp,
            "STICK_DOWN" => Binding::StickDown,
            "STICK_LEFT" => Binding::StickLeft,
            "STICK_RIGHT" => Binding::StickRight,
            "HALF" => Binding::Half,
            other => Binding::Button(
                button::by_name(other).ok_or_else(|| format!("unknown binding `{name}`"))?,
            ),
        })
    }
}

/// Left stick travel (0 to 1) that counts as centred, unless a config
/// sets its own; the Android controller uses it too.
pub(crate) const DEADZONE: f64 = 0.15;
/// How far the right stick goes (0 to 1) before it presses a C button;
/// the Android controller's axes use it too.
pub(crate) const PRESS: f64 = 0.5;

/// What a set of held bindings adds up to: button bits, stick keys and
/// whether `Half` is held.
#[derive(Default)]
struct Held {
    buttons: u16,
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    half: bool,
}

impl Held {
    fn of<'a>(bindings: impl IntoIterator<Item = &'a Binding>) -> Held {
        let mut h = Held::default();
        for binding in bindings {
            match binding {
                Binding::Button(b) => h.buttons |= b,
                Binding::StickUp => h.up = true,
                Binding::StickDown => h.down = true,
                Binding::StickLeft => h.left = true,
                Binding::StickRight => h.right = true,
                Binding::Half => h.half = true,
            }
        }
        h
    }
}

/// Parses a physical key by its winit name, such as `KeyW` or `Space`.
/// Arrow keys are refused: movement is WASD.
pub fn parse_key(name: &str) -> Result<KeyCode, String> {
    if name.starts_with("Arrow") {
        return Err(format!(
            "`{name}`: arrow keys are not used, movement is WASD"
        ));
    }
    KeyCode::deserialize(StrDeserializer::<serde::de::value::Error>::new(name))
        .map_err(|_| format!("unknown key `{name}` (use names like KeyW, Space, Enter)"))
}

/// Keyboard layout for one port. Keys are physical positions, so the
/// layout stays put on non-QWERTY keyboards.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyMap(pub Vec<(KeyCode, Binding)>);

impl Default for KeyMap {
    /// WASD moves the stick; the arrow keys are deliberately unused.
    fn default() -> Self {
        use Binding::*;
        use KeyCode::*;
        KeyMap(vec![
            (KeyW, StickUp),
            (KeyS, StickDown),
            (KeyA, StickLeft),
            (KeyD, StickRight),
            (ShiftLeft, Half),
            (KeyJ, Button(button::A)),
            (KeyK, Button(button::B)),
            (Space, Button(button::Z)),
            (Enter, Button(button::START)),
            (KeyQ, Button(button::L)),
            (KeyE, Button(button::R)),
            (KeyT, Button(button::C_UP)),
            (KeyG, Button(button::C_DOWN)),
            (KeyF, Button(button::C_LEFT)),
            (KeyH, Button(button::C_RIGHT)),
            (Numpad8, Button(button::C_UP)),
            (Numpad5, Button(button::C_DOWN)),
            (Numpad4, Button(button::C_LEFT)),
            (Numpad6, Button(button::C_RIGHT)),
            (Digit1, Button(button::D_UP)),
            (Digit2, Button(button::D_DOWN)),
            (Digit3, Button(button::D_LEFT)),
            (Digit4, Button(button::D_RIGHT)),
        ])
    }
}

impl KeyMap {
    /// Controller state with these keys held.
    pub fn pad(&self, held: &HashSet<KeyCode>) -> Pad {
        let mut pad = Pad::default();
        let bound = self.0.iter().filter(|(key, _)| held.contains(key));
        let Held {
            buttons,
            up,
            down,
            left,
            right,
            half,
        } = Held::of(bound.map(|(_, binding)| binding));
        pad.buttons = buttons;
        // shortcut: diagonals reach the square's corner, not the stick's octagonal gate; fine for menus and movement, revisit if a game reads the stick's magnitude.
        let reach = if half { STICK_MAX / 2 } else { STICK_MAX };
        // Opposite keys held together cancel out, like a centred stick.
        pad.x = (right as i8 - left as i8) * reach;
        pad.y = (up as i8 - down as i8) * reach;
        pad
    }
}

/// Parses a gamepad button by its Xbox name: `A`, `B`, `X`, `Y`, `LB`,
/// `RB`, `LT`, `RT`, `Start`, `Back`, `LS`, `RS` (stick clicks) and
/// `DPadUp`, `DPadDown`, `DPadLeft`, `DPadRight`. Other pads use the same
/// positions (`A` is the bottom face button).
pub fn parse_pad_button(name: &str) -> Result<Button, String> {
    Ok(match name.to_ascii_uppercase().as_str() {
        "A" => Button::South,
        "B" => Button::East,
        "X" => Button::West,
        "Y" => Button::North,
        "LB" => Button::LeftTrigger,
        "RB" => Button::RightTrigger,
        "LT" => Button::LeftTrigger2,
        "RT" => Button::RightTrigger2,
        "START" => Button::Start,
        "BACK" => Button::Select,
        "LS" => Button::LeftThumb,
        "RS" => Button::RightThumb,
        "DPADUP" => Button::DPadUp,
        "DPADDOWN" => Button::DPadDown,
        "DPADLEFT" => Button::DPadLeft,
        "DPADRIGHT" => Button::DPadRight,
        _ => {
            return Err(format!(
                "unknown pad button `{name}` (use A, B, X, Y, LB, RB, LT, RT, Start, Back, LS, RS, DPadUp..)"
            ))
        }
    })
}

/// Gamepad layout for one port: buttons, and how the sticks feel.
#[derive(Clone, Debug, PartialEq)]
pub struct PadMap {
    /// What each gamepad button does.
    pub buttons: Vec<(Button, Binding)>,
    /// Left stick travel (0 to 1) that still counts as centred.
    pub deadzone: f32,
    /// Left stick scale: above 1 reaches full tilt before the stick's edge.
    pub sensitivity: f32,
}

impl Default for PadMap {
    /// Xbox layout: A is A, B is B, X and Y are C-down and C-up, LT and RB
    /// are Z, LB is L, RT is R. The left stick is the stick, the right
    /// stick the four C buttons, the d-pad the d-pad.
    fn default() -> Self {
        use Binding::Button as B;
        PadMap {
            buttons: vec![
                (Button::South, B(button::A)),
                (Button::East, B(button::B)),
                (Button::West, B(button::C_DOWN)),
                (Button::North, B(button::C_UP)),
                (Button::LeftTrigger2, B(button::Z)),
                (Button::RightTrigger, B(button::Z)),
                (Button::LeftTrigger, B(button::L)),
                (Button::RightTrigger2, B(button::R)),
                (Button::Start, B(button::START)),
                (Button::DPadUp, B(button::D_UP)),
                (Button::DPadDown, B(button::D_DOWN)),
                (Button::DPadLeft, B(button::D_LEFT)),
                (Button::DPadRight, B(button::D_RIGHT)),
            ],
            deadzone: DEADZONE as f32,
            sensitivity: 1.0,
        }
    }
}

impl PadMap {
    /// Controller state from a gamepad, read through `pressed` and `axis`
    /// so it can be tested without a device.
    pub fn pad(&self, pressed: impl Fn(Button) -> bool, axis: impl Fn(Axis) -> f32) -> Pad {
        let mut pad = Pad::default();
        let bound = self.buttons.iter().filter(|(from, _)| pressed(*from));
        let Held {
            buttons,
            up,
            down,
            left,
            right,
            half,
        } = Held::of(bound.map(|(_, to)| to));
        pad.buttons = buttons;
        let (cx, cy) = (axis(Axis::RightStickX), axis(Axis::RightStickY));
        let press = PRESS as f32;
        for (on, bit) in [
            (cy > press, button::C_UP),
            (cy < -press, button::C_DOWN),
            (cx < -press, button::C_LEFT),
            (cx > press, button::C_RIGHT),
        ] {
            if on {
                pad.buttons |= bit;
            }
        }
        // Radial deadzone, then the rest of the travel spread over the full
        // range, so small tilts still register and diagonals stay round.
        let (x, y) = (axis(Axis::LeftStickX), axis(Axis::LeftStickY));
        let len = (x * x + y * y).sqrt();
        let (x, y) = if len <= self.deadzone || len == 0.0 {
            (0.0, 0.0)
        } else {
            let k =
                (len - self.deadzone) / (1.0 - self.deadzone).max(1e-3) * self.sensitivity / len;
            (x * k, y * k)
        };
        let reach = if half { STICK_MAX / 2 } else { STICK_MAX } as f32;
        let to_n64 = |v: f32| (v.clamp(-1.0, 1.0) * reach).round() as i8;
        pad.x = to_n64(x);
        pad.y = to_n64(y);
        // Pad buttons bound to the stick push it all the way (or halfway).
        if up || down || left || right {
            pad.x = to_n64((right as i8 - left as i8) as f32);
            pad.y = to_n64((up as i8 - down as i8) as f32);
        }
        pad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(keys: &[KeyCode]) -> HashSet<KeyCode> {
        keys.iter().copied().collect()
    }

    #[test]
    fn wasd_moves_and_arrows_do_nothing() {
        let map = KeyMap::default();
        assert_eq!(
            map.pad(&held(&[KeyCode::KeyW])),
            Pad {
                buttons: 0,
                x: 0,
                y: STICK_MAX
            }
        );
        let walk = map.pad(&held(&[KeyCode::KeyA, KeyCode::ShiftLeft]));
        assert_eq!((walk.x, walk.y), (-STICK_MAX / 2, 0));
        let arrows = [
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
        ];
        assert_eq!(map.pad(&held(&arrows)), Pad::default());
        let all = [KeyCode::KeyW, KeyCode::KeyA, KeyCode::KeyS, KeyCode::KeyD];
        assert_eq!(map.pad(&held(&all)), Pad::default());
        let up_left = map.pad(&held(&[KeyCode::KeyW, KeyCode::KeyS, KeyCode::KeyA]));
        assert_eq!((up_left.x, up_left.y), (-STICK_MAX, 0));
        assert!(map.0.iter().all(|(k, _)| !arrows.contains(k)));
    }

    #[test]
    fn keys_and_bindings_parse() {
        assert_eq!(parse_key("KeyW"), Ok(KeyCode::KeyW));
        assert!(parse_key("Nope").is_err());
        assert!(parse_key("ArrowUp").unwrap_err().contains("WASD"));
        assert_eq!(Binding::parse("cleft"), Ok(Binding::Button(button::C_LEFT)));
        assert_eq!(Binding::parse("stick_up"), Ok(Binding::StickUp));
        assert!(Binding::parse("jump").is_err());
    }

    #[test]
    fn gamepad_layout() {
        let pad = PadMap::default().pad(
            |b| {
                matches!(
                    b,
                    Button::South | Button::East | Button::West | Button::LeftTrigger2
                )
            },
            |a| match a {
                Axis::LeftStickX => 1.0,
                Axis::RightStickY => 0.9,
                _ => 0.0,
            },
        );
        assert_eq!(
            pad,
            Pad {
                buttons: button::A | button::B | button::C_DOWN | button::Z | button::C_UP,
                x: STICK_MAX,
                y: 0
            }
        );
    }

    #[test]
    fn gamepad_rebinds_deadzone_and_sensitivity() {
        assert_eq!(parse_pad_button("rb"), Ok(Button::RightTrigger));
        assert!(parse_pad_button("Guide").is_err());
        let map = PadMap {
            buttons: vec![(Button::East, Binding::Button(button::B))],
            deadzone: 0.2,
            sensitivity: 2.0,
        };
        let pad = |x: f32, pressed: bool| {
            map.pad(
                |b| pressed && b == Button::East,
                |a| if a == Axis::LeftStickX { x } else { 0.0 },
            )
        };
        assert_eq!(pad(0.15, false).x, 0, "inside the deadzone");
        // Past the deadzone, doubled: (0.4 - 0.2) / 0.8 * 2 = half tilt.
        assert_eq!(pad(0.4, false).x, STICK_MAX / 2);
        assert_eq!(pad(-1.0, false).x, -STICK_MAX, "clamped");
        assert_eq!(pad(0.0, true).buttons, button::B);
        assert_eq!(pad(0.0, false).buttons, 0, "the default layout is replaced");
        let walk = PadMap {
            buttons: vec![(Button::LeftTrigger, Binding::Half)],
            ..PadMap::default()
        };
        let x = walk
            .pad(
                |b| b == Button::LeftTrigger,
                |a| (a == Axis::LeftStickX) as u8 as f32,
            )
            .x;
        assert_eq!(x, STICK_MAX / 2, "HALF halves the stick");
    }
}
