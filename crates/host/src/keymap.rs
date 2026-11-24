//! Keyboard bindings. Keys are named as winit's `KeyCode` prints them
//! (`KeyW`, `Space`, `Enter`, `Digit1`), which are physical positions, so
//! WASD stays WASD on any keyboard layout. Arrow keys are rejected.

use std::collections::{BTreeMap, HashSet};

use crate::pad::{button_by_name, Pads, STICK_MAX};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Button(u16),
    Stick(i8, i8),
}

/// Which key drives which input on which port.
#[derive(Clone, Debug)]
pub struct KeyMap {
    binds: Vec<(String, usize, Target)>,
}

/// Port 1 defaults. Every one can be changed in the run config.
const DEFAULT: [(&str, &str); 19] = [
    ("STICK_UP", "KeyW"),
    ("STICK_LEFT", "KeyA"),
    ("STICK_DOWN", "KeyS"),
    ("STICK_RIGHT", "KeyD"),
    ("A", "KeyJ"),
    ("B", "KeyK"),
    ("Z", "Space"),
    ("START", "Enter"),
    ("L", "KeyQ"),
    ("R", "KeyE"),
    ("C_UP", "KeyT"),
    ("C_LEFT", "KeyF"),
    ("C_DOWN", "KeyG"),
    ("C_RIGHT", "KeyH"),
    ("D_UP", "Digit1"),
    ("D_DOWN", "Digit2"),
    ("D_LEFT", "Digit3"),
    ("D_RIGHT", "Digit4"),
    // Half deflection, for walking.
    ("STICK_HALF", "ShiftLeft"),
];

fn target(name: &str) -> Option<Target> {
    Some(match name.to_ascii_uppercase().as_str() {
        "STICK_UP" => Target::Stick(0, 1),
        "STICK_DOWN" => Target::Stick(0, -1),
        "STICK_LEFT" => Target::Stick(-1, 0),
        "STICK_RIGHT" => Target::Stick(1, 0),
        "STICK_HALF" => Target::Stick(0, 0),
        n => Target::Button(button_by_name(n)?),
    })
}

impl Default for KeyMap {
    fn default() -> Self {
        let port1 = DEFAULT
            .iter()
            .map(|&(t, k)| (t.to_string(), k.to_string()))
            .collect();
        Self::from_config(&BTreeMap::from([("1".to_string(), port1)])).expect("defaults are valid")
    }
}

impl KeyMap {
    /// Build from the run config's `[keys.N]` tables (port 1 to 4, input name
    /// to key name). An empty table set gives the defaults.
    pub fn from_config(
        keys: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<KeyMap, String> {
        if keys.is_empty() {
            return Ok(KeyMap::default());
        }
        let mut binds = Vec::new();
        for (port, table) in keys {
            let p = match port.parse::<usize>() {
                Ok(p @ 1..=4) => p - 1,
                _ => return Err(format!("[keys.{port}]: port must be 1 to 4")),
            };
            for (input, key) in table {
                let t = target(input)
                    .ok_or_else(|| format!("[keys.{port}]: unknown input `{input}`"))?;
                if key.starts_with("Arrow") {
                    return Err(format!(
                        "[keys.{port}] {input} = \"{key}\": arrow keys are not used, movement is WASD"
                    ));
                }
                binds.push((key.clone(), p, t));
            }
        }
        Ok(KeyMap { binds })
    }

    /// Controller state for the keys held right now.
    pub fn pads(&self, held: &HashSet<String>) -> Pads {
        let mut pads = Pads::default();
        let mut stick = [(0i8, 0i8, false); 4];
        for (key, port, t) in &self.binds {
            if !held.contains(key) {
                continue;
            }
            match *t {
                Target::Button(b) => pads[*port].buttons |= b,
                Target::Stick(0, 0) => stick[*port].2 = true,
                Target::Stick(dx, dy) => {
                    stick[*port].0 += dx;
                    stick[*port].1 += dy;
                }
            }
        }
        for (pad, (dx, dy, half)) in pads.iter_mut().zip(stick) {
            // Opposite keys cancel; each axis is clamped to one step.
            let scale = if half { STICK_MAX / 2 } else { STICK_MAX };
            pad.x = dx.signum() * scale;
            pad.y = dy.signum() * scale;
        }
        pads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::{button, Pad};

    fn held(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn defaults_use_wasd_and_no_arrow_keys() {
        let map = KeyMap::default();
        assert!(map.binds.iter().all(|(k, _, _)| !k.starts_with("Arrow")));
        let p = map.pads(&held(&["KeyW", "KeyD", "KeyJ"]))[0];
        assert_eq!(
            p,
            Pad {
                buttons: button::A,
                x: STICK_MAX,
                y: STICK_MAX
            }
        );
        assert_eq!(map.pads(&held(&["ArrowUp"]))[0], Pad::default());
    }

    #[test]
    fn opposite_keys_cancel_and_shift_halves() {
        let map = KeyMap::default();
        assert_eq!(map.pads(&held(&["KeyA", "KeyD"]))[0].x, 0);
        assert_eq!(map.pads(&held(&["KeyS", "ShiftLeft"]))[0].y, -STICK_MAX / 2);
    }

    #[test]
    fn config_rejects_arrows_bad_ports_and_unknown_inputs() {
        let one = |port: &str, input: &str, key: &str| {
            let t = BTreeMap::from([(input.to_string(), key.to_string())]);
            KeyMap::from_config(&BTreeMap::from([(port.to_string(), t)]))
        };
        assert!(one("1", "STICK_UP", "ArrowUp").is_err());
        assert!(one("5", "A", "KeyJ").is_err());
        assert!(one("1", "JUMP", "KeyJ").is_err());
        let map = one("2", "start", "KeyP").unwrap();
        assert_eq!(map.pads(&held(&["KeyP"]))[1].buttons, button::START);
    }
}
