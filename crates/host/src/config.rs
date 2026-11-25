//! One config file per run: everything that decides how a run goes, so a
//! run can be repeated from its file alone.
//!
//! ```toml
//! frames = 600                      # stop after this many frames
//! frame_rate = 59.94                # host frames per second
//! headless = true                   # no window: scripted input only
//! replay = "intro.txt"              # input script to play back
//! record = "take.txt"               # write this run's input here
//! debug_port = "127.0.0.1:6464"     # step frames from a test or tool
//!
//! [window]
//! title = "n64rusty"
//! scale = 2                         # window is 320x240 times this
//! widescreen = true                 # fill a wide window, showing more
//! refresh = 144                     # draw this often, blending frames
//!                                   # (0, the default: the display's rate)
//!
//! [[port]]                          # ports 1 to 4, in order
//! sources = ["keyboard", "gamepad0"]
//! keys = { KeyJ = "A", KeyK = "B" } # replaces the default layout
//! pad = { A = "A", X = "B", RB = "Z" } # gamepad buttons (Xbox names)
//! deadzone = 0.15                   # left stick travel that counts as centred
//! sensitivity = 1.0                 # left stick scale
//! ```
//!
//! Relative paths are relative to the config file. Unknown keys are errors,
//! so a typo cannot silently change a run.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::mapping::{parse_key, parse_pad_button, Binding, KeyMap, PadMap};

/// NTSC field rate. The video interface's own timing replaces this once
/// the renderer reads it.
pub const NTSC_RATE: f64 = 60.0 / 1.001;

#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    pub frames: Option<u64>,
    pub frame_rate: f64,
    pub headless: bool,
    pub replay: Option<PathBuf>,
    pub record: Option<PathBuf>,
    pub debug_port: Option<SocketAddr>,
    pub window: WindowConfig,
    pub ports: [PortConfig; 4],
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WindowConfig {
    pub title: String,
    pub scale: u32,
    /// Fill a window wider than 4:3 with more of the scene instead of
    /// black bars. On by default.
    pub widescreen: bool,
    /// Draw the window this many times per second, with pictures blended
    /// between game frames; the game itself keeps `frame_rate`. 0 (the
    /// default) is the display's own rate; None draws once per game frame.
    pub refresh: Option<f64>,
    /// Textures as the console filters them, soft when stretched to the
    /// window. Off (the default) keeps magnified texels crisp.
    pub console_filter: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            title: "n64rusty".into(),
            scale: 2,
            widescreen: true,
            refresh: Some(0.0),
            console_filter: false,
        }
    }
}

/// Where one port's input comes from. Several sources are merged.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PortConfig {
    /// The keyboard layout, if the keyboard drives this port.
    pub keyboard: Option<KeyMap>,
    /// Gamepads, by connection order (0 is the first connected).
    pub gamepads: Vec<usize>,
    /// The gamepad layout for this port's gamepads.
    pub pad: PadMap,
}

impl Default for RunConfig {
    fn default() -> Self {
        let pad = |n| PortConfig {
            gamepads: vec![n],
            ..PortConfig::default()
        };
        RunConfig {
            frames: None,
            frame_rate: NTSC_RATE,
            headless: false,
            replay: None,
            record: None,
            debug_port: None,
            window: WindowConfig::default(),
            ports: [
                PortConfig {
                    keyboard: Some(KeyMap::default()),
                    gamepads: vec![0],
                    pad: PadMap::default(),
                },
                pad(1),
                pad(2),
                pad(3),
            ],
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    frames: Option<u64>,
    frame_rate: Option<f64>,
    #[serde(default)]
    headless: bool,
    replay: Option<PathBuf>,
    record: Option<PathBuf>,
    debug_port: Option<SocketAddr>,
    #[serde(default)]
    window: WindowConfig,
    port: Option<Vec<RawPort>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPort {
    #[serde(default)]
    sources: Vec<String>,
    keys: Option<BTreeMap<String, String>>,
    pad: Option<BTreeMap<String, String>>,
    deadzone: Option<f32>,
    sensitivity: Option<f32>,
}

impl RunConfig {
    /// Reads a run's config file.
    pub fn load(path: &Path) -> Result<RunConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new(""));
        RunConfig::parse(&text, base).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses config text; relative paths are taken from `base`.
    pub fn parse(text: &str, base: &Path) -> Result<RunConfig, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut cfg = RunConfig {
            frames: raw.frames,
            headless: raw.headless,
            replay: raw.replay.map(|p| base.join(p)),
            record: raw.record.map(|p| base.join(p)),
            debug_port: raw.debug_port,
            window: raw.window,
            ..RunConfig::default()
        };
        if let Some(rate) = raw.frame_rate {
            if !(rate.is_finite() && (1.0..=1000.0).contains(&rate)) {
                return Err(format!("frame_rate {rate} is not between 1 and 1000"));
            }
            cfg.frame_rate = rate;
        }
        if let Some(r) = cfg.window.refresh {
            if !(r == 0.0 || r.is_finite() && (1.0..=1000.0).contains(&r)) {
                return Err(format!(
                    "window refresh {r} is not 0 (the display's rate) or between 1 and 1000"
                ));
            }
        }
        if !(1..=16).contains(&cfg.window.scale) {
            return Err(format!("window scale {} is not 1 to 16", cfg.window.scale));
        }
        // The debug port drives the whole program: never offer it to the network.
        if cfg.debug_port.is_some_and(|a| !a.ip().is_loopback()) {
            return Err("debug_port must be a loopback address such as 127.0.0.1".into());
        }
        if let Some(ports) = raw.port {
            if ports.len() > 4 {
                return Err(format!(
                    "{} [[port]] tables; there are 4 ports",
                    ports.len()
                ));
            }
            cfg.ports = Default::default();
            for (n, raw) in ports.into_iter().enumerate() {
                cfg.ports[n] = port(raw).map_err(|e| format!("port {}: {e}", n + 1))?;
            }
        }
        Ok(cfg)
    }
}

fn port(raw: RawPort) -> Result<PortConfig, String> {
    let mut port = PortConfig::default();
    for source in &raw.sources {
        if source == "keyboard" {
            port.keyboard = Some(KeyMap::default());
        } else if let Some(n) = source.strip_prefix("gamepad").and_then(|n| n.parse().ok()) {
            port.gamepads.push(n);
        } else {
            return Err(format!(
                "unknown source `{source}` (use keyboard or gamepad0..)"
            ));
        }
    }
    if let Some(keys) = raw.keys {
        if port.keyboard.is_none() {
            return Err("keys given but the keyboard is not a source".into());
        }
        let map = keys
            .iter()
            .map(|(k, b)| Ok((parse_key(k)?, Binding::parse(b)?)))
            .collect::<Result<_, String>>()?;
        port.keyboard = Some(KeyMap(map));
    }
    if (raw.pad.is_some() || raw.deadzone.is_some() || raw.sensitivity.is_some())
        && port.gamepads.is_empty()
    {
        return Err("pad settings given but no gamepad is a source".into());
    }
    if let Some(buttons) = raw.pad {
        port.pad.buttons = buttons
            .iter()
            .map(|(k, b)| Ok((parse_pad_button(k)?, Binding::parse(b)?)))
            .collect::<Result<_, String>>()?;
    }
    if let Some(d) = raw.deadzone {
        if !(0.0..0.9).contains(&d) {
            return Err(format!("deadzone {d} is not from 0 to 0.9"));
        }
        port.pad.deadzone = d;
    }
    if let Some(k) = raw.sensitivity {
        if !(0.1..=4.0).contains(&k) {
            return Err(format!("sensitivity {k} is not from 0.1 to 4"));
        }
        port.pad.sensitivity = k;
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::button;
    use winit::keyboard::KeyCode;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(
            RunConfig::parse("", Path::new("")).unwrap(),
            RunConfig::default()
        );
    }

    #[test]
    fn example_file_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("run.example.toml");
        let cfg = RunConfig::load(&path).unwrap();
        assert_eq!(cfg.ports[0].keyboard, Some(KeyMap::default()));
    }

    #[test]
    fn full_file() {
        let text = r#"
            frames = 600
            frame_rate = 50
            headless = true
            replay = "in.txt"
            record = "out.txt"
            debug_port = "127.0.0.1:6464"
            [window]
            scale = 3
            widescreen = true
            refresh = 144
            [[port]]
            sources = ["keyboard"]
            keys = { KeyJ = "A", KeyW = "STICK_UP" }
            [[port]]
            sources = ["gamepad0", "gamepad2"]
            pad = { RB = "A", lt = "STICK_UP" }
            deadzone = 0.25
            sensitivity = 1.5
        "#;
        let cfg = RunConfig::parse(text, Path::new("runs")).unwrap();
        assert_eq!(cfg.frames, Some(600));
        assert_eq!(cfg.frame_rate, 50.0);
        assert_eq!(cfg.replay, Some(PathBuf::from("runs").join("in.txt")));
        assert_eq!(cfg.window.scale, 3);
        assert!(cfg.window.widescreen);
        assert_eq!(cfg.window.refresh, Some(144.0));
        assert_eq!(
            cfg.ports[0].keyboard,
            Some(KeyMap(vec![
                (KeyCode::KeyJ, Binding::Button(button::A)),
                (KeyCode::KeyW, Binding::StickUp)
            ]))
        );
        assert_eq!(cfg.ports[1].gamepads, vec![0, 2]);
        assert_eq!(
            cfg.ports[1].pad,
            PadMap {
                buttons: vec![
                    (gilrs::Button::RightTrigger, Binding::Button(button::A)),
                    (gilrs::Button::LeftTrigger2, Binding::StickUp),
                ],
                deadzone: 0.25,
                sensitivity: 1.5,
            }
        );
        assert_eq!(cfg.ports[2], PortConfig::default());
    }

    #[test]
    fn rejects_mistakes() {
        for (bad, why) in [
            ("frmaes = 3", "unknown field"),
            ("debug_port = \"0.0.0.0:6464\"", "loopback"),
            ("frame_rate = 0", "between"),
            ("[window]\nrefresh = inf", "between"),
            ("[window]\nrefresh = 0.5", "between"),
            ("[[port]]\nsources = [\"mouse\"]", "unknown source"),
            (
                "[[port]]\nsources = [\"keyboard\"]\ndeadzone = 0.1",
                "no gamepad",
            ),
            (
                "[[port]]\nsources = [\"gamepad0\"]\npad = { Guide = \"A\" }",
                "unknown pad button",
            ),
            (
                "[[port]]\nsources = [\"gamepad0\"]\nsensitivity = 9",
                "sensitivity",
            ),
            (
                "[[port]]\nsources = [\"gamepad0\"]\nkeys = { KeyJ = \"A\" }",
                "not a source",
            ),
            (
                "[[port]]\nsources = [\"keyboard\"]\nkeys = { Jay = \"A\" }",
                "unknown key",
            ),
            (
                "[[port]]\nsources = [\"keyboard\"]\nkeys = { KeyJ = \"JUMP\" }",
                "unknown binding",
            ),
        ] {
            let err = RunConfig::parse(bad, Path::new("")).unwrap_err();
            assert!(err.contains(why), "{bad:?} gave {err}");
        }
    }
}
