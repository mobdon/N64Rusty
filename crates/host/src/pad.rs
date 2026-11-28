//! Controller state in the console's own layout.

use std::fmt;

/// Button bits as the controller reports them (16-bit button word).
pub mod button {
    /// The A button.
    pub const A: u16 = 0x8000;
    /// The B button.
    pub const B: u16 = 0x4000;
    /// The Z trigger.
    pub const Z: u16 = 0x2000;
    /// The Start button.
    pub const START: u16 = 0x1000;
    /// D-pad up.
    pub const D_UP: u16 = 0x0800;
    /// D-pad down.
    pub const D_DOWN: u16 = 0x0400;
    /// D-pad left.
    pub const D_LEFT: u16 = 0x0200;
    /// D-pad right.
    pub const D_RIGHT: u16 = 0x0100;
    /// The L shoulder button.
    pub const L: u16 = 0x0020;
    /// The R shoulder button.
    pub const R: u16 = 0x0010;
    /// C-up.
    pub const C_UP: u16 = 0x0008;
    /// C-down.
    pub const C_DOWN: u16 = 0x0004;
    /// C-left.
    pub const C_LEFT: u16 = 0x0002;
    /// C-right.
    pub const C_RIGHT: u16 = 0x0001;

    /// Names used in scripts and config files, in button-word order.
    pub const NAMES: [(&str, u16); 14] = [
        ("A", A),
        ("B", B),
        ("Z", Z),
        ("START", START),
        ("DUP", D_UP),
        ("DDOWN", D_DOWN),
        ("DLEFT", D_LEFT),
        ("DRIGHT", D_RIGHT),
        ("L", L),
        ("R", R),
        ("CUP", C_UP),
        ("CDOWN", C_DOWN),
        ("CLEFT", C_LEFT),
        ("CRIGHT", C_RIGHT),
    ];

    /// Looks a button up by its script name, ignoring case.
    pub fn by_name(name: &str) -> Option<u16> {
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, b)| b)
    }
}

/// Largest stick value a host device produces. Real sticks reach roughly
/// this far; games are written for that range, not the full `i8`.
pub const STICK_MAX: i8 = 80;

/// One controller for one frame. `y` is positive for up, as on the console.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Pad {
    /// Held buttons, as [`button`] bits.
    pub buttons: u16,
    /// Stick left (negative) to right (positive).
    pub x: i8,
    /// Stick down (negative) to up (positive).
    pub y: i8,
}

/// All four controller ports for one frame.
pub type Pads = [Pad; 4];

impl Pad {
    /// Combines two sources for one port: buttons are or-ed, and each stick
    /// axis takes whichever source is pushed further.
    pub fn merge(self, other: Pad) -> Pad {
        let far = |a: i8, b: i8| {
            if b.unsigned_abs() > a.unsigned_abs() {
                b
            } else {
                a
            }
        };
        Pad {
            buttons: self.buttons | other.buttons,
            x: far(self.x, other.x),
            y: far(self.y, other.y),
        }
    }
}

/// Button set as names joined by `+`, or `-` for none, e.g. `A+START`.
pub struct ButtonNames(pub u16);

impl fmt::Display for ButtonNames {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("-");
        }
        let mut first = true;
        for (name, bit) in button::NAMES {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Parses the [`ButtonNames`] form back into a button word.
pub fn parse_buttons(text: &str) -> Result<u16, String> {
    if text == "-" {
        return Ok(0);
    }
    text.split('+').try_fold(0, |acc, name| {
        button::by_name(name)
            .map(|b| acc | b)
            .ok_or_else(|| format!("unknown button `{name}`"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_names_round_trip() {
        for word in [0, button::A | button::START, 0x3f3f, !0x00c0] {
            let text = ButtonNames(word).to_string();
            assert_eq!(parse_buttons(&text), Ok(word), "{text}");
        }
        assert_eq!(parse_buttons("a+cup"), Ok(button::A | button::C_UP));
        assert!(parse_buttons("A+X").is_err());
        assert!(parse_buttons("").is_err());
    }

    #[test]
    fn merge_takes_the_further_stick() {
        let a = Pad {
            buttons: button::A,
            x: 10,
            y: -80,
        };
        let b = Pad {
            buttons: button::B,
            x: -40,
            y: 5,
        };
        assert_eq!(
            a.merge(b),
            Pad {
                buttons: button::A | button::B,
                x: -40,
                y: -80
            }
        );
    }
}
