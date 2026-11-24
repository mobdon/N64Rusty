//! Gamepads through gilrs: gamepad N feeds port N, on top of the keyboard.

use gilrs::{Axis, Button, Gamepad, Gilrs};

use crate::pad::{button, Pad, Pads, STICK_MAX};

/// Stick values inside this are treated as centred, so a worn stick does not drift.
pub const DEADZONE: f32 = 0.15;

pub struct Gamepads(Gilrs);

impl Gamepads {
    /// `None` when the platform has no gamepad support.
    pub fn new() -> Option<Gamepads> {
        Gilrs::new().ok().map(Gamepads)
    }

    /// Add what each connected gamepad says to its port.
    pub fn add_to(&mut self, pads: &mut Pads) {
        while self.0.next_event().is_some() {}
        for (pad, (_, gp)) in pads.iter_mut().zip(self.0.gamepads()) {
            *pad = pad.merge(read(&gp));
        }
    }
}

const BUTTONS: [(Button, u16); 13] = [
    (Button::South, button::A),
    (Button::West, button::B),
    (Button::LeftTrigger2, button::Z),
    (Button::RightTrigger2, button::Z),
    (Button::Start, button::START),
    (Button::LeftTrigger, button::L),
    (Button::RightTrigger, button::R),
    (Button::DPadUp, button::D_UP),
    (Button::DPadDown, button::D_DOWN),
    (Button::DPadLeft, button::D_LEFT),
    (Button::DPadRight, button::D_RIGHT),
    (Button::North, button::C_UP),
    (Button::East, button::C_RIGHT),
];

fn read(gp: &Gamepad) -> Pad {
    let mut buttons = BUTTONS
        .iter()
        .filter(|(b, _)| gp.is_pressed(*b))
        .fold(0, |a, (_, n)| a | n);
    // The right stick is the C buttons.
    let (rx, ry) = (gp.value(Axis::RightStickX), gp.value(Axis::RightStickY));
    for (on, b) in [
        (ry > 0.5, button::C_UP),
        (ry < -0.5, button::C_DOWN),
        (rx < -0.5, button::C_LEFT),
        (rx > 0.5, button::C_RIGHT),
    ] {
        if on {
            buttons |= b;
        }
    }
    Pad {
        buttons,
        x: stick_axis(gp.value(Axis::LeftStickX)),
        y: stick_axis(gp.value(Axis::LeftStickY)),
    }
}

/// Map a gamepad axis (-1 to 1, up and right positive) to the controller's range.
pub fn stick_axis(v: f32) -> i8 {
    if v.abs() < DEADZONE {
        0
    } else {
        (v.clamp(-1.0, 1.0) * STICK_MAX as f32).round() as i8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_axis_has_a_deadzone_and_clamps() {
        assert_eq!(stick_axis(0.1), 0);
        assert_eq!(stick_axis(-0.5), -40);
        assert_eq!(stick_axis(1.7), STICK_MAX);
        assert_eq!(stick_axis(-1.0), -STICK_MAX);
    }
}
