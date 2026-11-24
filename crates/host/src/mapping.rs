//! Host devices to controller state: keyboard keys and gamepads.

use std::collections::HashSet;

use gilrs::{Axis, Button};
use serde::de::{value::StrDeserializer, Deserialize};
use winit::keyboard::KeyCode;

use crate::pad::{button, Pad, STICK_MAX};

/// What a key does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binding {
    Button(u16),
    StickUp,
    StickDown,
    StickLeft,
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
        let (mut up, mut down, mut left, mut right, mut half) = (false, false, false, false, false);
        for (key, binding) in &self.0 {
            if !held.contains(key) {
                continue;
            }
            match binding {
                Binding::Button(b) => pad.buttons |= b,
                Binding::StickUp => up = true,
                Binding::StickDown => down = true,
                Binding::StickLeft => left = true,
                Binding::StickRight => right = true,
                Binding::Half => half = true,
            }
        }
        // shortcut: diagonals reach the square's corner, not the stick's octagonal gate; fine for menus and movement, revisit if a game reads the stick's magnitude.
        let reach = if half { STICK_MAX / 2 } else { STICK_MAX };
        // Opposite keys held together cancel out, like a centred stick.
        pad.x = (right as i8 - left as i8) * reach;
        pad.y = (up as i8 - down as i8) * reach;
        pad
    }
}

/// Controller state from a gamepad, read through `pressed` and `axis` so it
/// can be tested without a device. Layout: south face button A, west B,
/// left trigger Z, shoulders L and R, right stick the C buttons.
pub fn gamepad_pad(pressed: impl Fn(Button) -> bool, axis: impl Fn(Axis) -> f32) -> Pad {
    let mut buttons = 0;
    for (from, to) in [
        (Button::South, button::A),
        (Button::West, button::B),
        (Button::LeftTrigger2, button::Z),
        (Button::Start, button::START),
        (Button::DPadUp, button::D_UP),
        (Button::DPadDown, button::D_DOWN),
        (Button::DPadLeft, button::D_LEFT),
        (Button::DPadRight, button::D_RIGHT),
        (Button::LeftTrigger, button::L),
        (Button::RightTrigger, button::R),
        (Button::RightTrigger2, button::R),
    ] {
        if pressed(from) {
            buttons |= to;
        }
    }
    let (cx, cy) = (axis(Axis::RightStickX), axis(Axis::RightStickY));
    for (on, bit) in [
        (cy > 0.5, button::C_UP),
        (cy < -0.5, button::C_DOWN),
        (cx < -0.5, button::C_LEFT),
        (cx > 0.5, button::C_RIGHT),
    ] {
        if on {
            buttons |= bit;
        }
    }
    let stick = |v: f32| {
        let v = if v.abs() < 0.1 {
            0.0
        } else {
            v.clamp(-1.0, 1.0)
        };
        (v * STICK_MAX as f32).round() as i8
    };
    Pad {
        buttons,
        x: stick(axis(Axis::LeftStickX)),
        y: stick(axis(Axis::LeftStickY)),
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
        let pad = gamepad_pad(
            |b| matches!(b, Button::South | Button::LeftTrigger2),
            |a| match a {
                Axis::LeftStickX => 1.0,
                Axis::LeftStickY => -0.05,
                Axis::RightStickY => 0.9,
                _ => 0.0,
            },
        );
        assert_eq!(
            pad,
            Pad {
                buttons: button::A | button::Z | button::C_UP,
                x: STICK_MAX,
                y: 0
            }
        );
    }
}
