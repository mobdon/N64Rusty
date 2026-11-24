//! Controller state for one port, in the layout the controller itself reports.

/// Button bits, high byte first, as a standard controller reports them
/// (layout from the public n64brew documentation, see CREDITS.md).
pub mod button {
    pub const A: u16 = 0x8000;
    pub const B: u16 = 0x4000;
    pub const Z: u16 = 0x2000;
    pub const START: u16 = 0x1000;
    pub const D_UP: u16 = 0x0800;
    pub const D_DOWN: u16 = 0x0400;
    pub const D_LEFT: u16 = 0x0200;
    pub const D_RIGHT: u16 = 0x0100;
    pub const L: u16 = 0x0020;
    pub const R: u16 = 0x0010;
    pub const C_UP: u16 = 0x0008;
    pub const C_DOWN: u16 = 0x0004;
    pub const C_LEFT: u16 = 0x0002;
    pub const C_RIGHT: u16 = 0x0001;
}

/// Names used in config files and input scripts.
pub const BUTTON_NAMES: [(&str, u16); 14] = [
    ("A", button::A),
    ("B", button::B),
    ("Z", button::Z),
    ("START", button::START),
    ("D_UP", button::D_UP),
    ("D_DOWN", button::D_DOWN),
    ("D_LEFT", button::D_LEFT),
    ("D_RIGHT", button::D_RIGHT),
    ("L", button::L),
    ("R", button::R),
    ("C_UP", button::C_UP),
    ("C_DOWN", button::C_DOWN),
    ("C_LEFT", button::C_LEFT),
    ("C_RIGHT", button::C_RIGHT),
];

/// Full stick deflection used for digital input (keys) and as the scale for
/// gamepad sticks. An original stick reaches roughly this far on each axis.
pub const STICK_MAX: i8 = 80;

/// One controller: buttons plus the analog stick (up and right are positive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u16,
    pub x: i8,
    pub y: i8,
}

/// All four ports.
pub type Pads = [Pad; 4];

impl Pad {
    /// Combine two sources for the same port: buttons add up, and the stick
    /// comes from whichever source is deflected (`self` wins a tie).
    pub fn merge(self, other: Pad) -> Pad {
        let stick = if self.x != 0 || self.y != 0 {
            self
        } else {
            other
        };
        Pad {
            buttons: self.buttons | other.buttons,
            x: stick.x,
            y: stick.y,
        }
    }
}

pub fn button_by_name(name: &str) -> Option<u16> {
    BUTTON_NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, b)| b)
}

/// Parse `A+B+START`; `-` means no buttons.
pub fn parse_buttons(s: &str) -> Result<u16, String> {
    if s == "-" {
        return Ok(0);
    }
    s.split('+').try_fold(0, |acc, n| {
        button_by_name(n)
            .map(|b| acc | b)
            .ok_or_else(|| format!("unknown button `{n}`"))
    })
}

/// The inverse of [`parse_buttons`].
pub fn format_buttons(buttons: u16) -> String {
    let names: Vec<&str> = BUTTON_NAMES
        .iter()
        .filter(|(_, b)| buttons & b != 0)
        .map(|&(n, _)| n)
        .collect();
    if names.is_empty() {
        "-".into()
    } else {
        names.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_round_trip() {
        for b in [0, button::A | button::START, 0xFF3F] {
            assert_eq!(parse_buttons(&format_buttons(b)), Ok(b));
        }
        assert_eq!(parse_buttons("a+c_up"), Ok(button::A | button::C_UP));
        assert!(parse_buttons("A+X").is_err());
    }

    #[test]
    fn merge_ors_buttons_and_keeps_a_deflected_stick() {
        let keys = Pad {
            buttons: button::A,
            x: 0,
            y: 0,
        };
        let stick = Pad {
            buttons: button::B,
            x: -40,
            y: 12,
        };
        assert_eq!(
            keys.merge(stick),
            Pad {
                buttons: button::A | button::B,
                x: -40,
                y: 12
            }
        );
        assert_eq!(
            stick.merge(keys),
            Pad {
                buttons: button::A | button::B,
                x: -40,
                y: 12
            }
        );
    }
}
