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
//!                                   # (0, the PC default: the display's rate;
//!                                   # left out on Android: once per game frame)
//! max_render_height = 720           # draw at most this many lines, scaled up
//!                                   # (left out, the default: the window's size)
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

/// Everything that decides how a run goes (see the module docs for the
/// file it is read from).
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    /// The file this config was loaded from, where window choices made in
    /// the menu are saved.
    pub path: Option<PathBuf>,
    /// Stop after this many frames; `None` runs until closed or `quit`.
    pub frames: Option<u64>,
    /// Host frames per second.
    pub frame_rate: f64,
    /// No window: scripted input only.
    pub headless: bool,
    /// An input script to play back.
    pub replay: Option<PathBuf>,
    /// Where to write this run's input as a script.
    pub record: Option<PathBuf>,
    /// Listen here for [`crate::debug`] commands.
    pub debug_port: Option<SocketAddr>,
    /// The window's settings.
    pub window: WindowConfig,
    /// Where each of the four ports' input comes from.
    pub ports: [PortConfig; 4],
    /// The program has a start screen (the player): the Game menu can go
    /// back to it, and a crash returns to it. Set by the program, never
    /// read from the file.
    pub start_screen: bool,
}

/// The `[window]` table: how the game is shown.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WindowConfig {
    /// The window's title.
    pub title: String,
    /// The window's size as a multiple of 320x240.
    pub scale: u32,
    /// Fill a window wider than 4:3 with more of the scene instead of
    /// black bars. On by default.
    pub widescreen: bool,
    /// Draw the window this many times per second, with pictures blended
    /// between game frames; the game itself keeps `frame_rate`. 0 (the
    /// default on PC) is the display's own rate; None (the default on
    /// Android, see [`DEFAULT_REFRESH`]) draws once per game frame.
    pub refresh: Option<f64>,
    /// Textures as the console filters them, soft when stretched to the
    /// window. Off (the default) keeps magnified texels crisp.
    pub console_filter: bool,
    /// Draw the window's picture at most this many lines high (at least
    /// [`MIN_RENDER_HEIGHT`]), scaled up to the window: less GPU work on a
    /// large screen, at the cost of sharpness. Unset (the default) draws at
    /// the window's own resolution.
    pub max_render_height: Option<u32>,
    /// Per-game HUD areas kept at the window's edges in widescreen:
    /// `[x0, y0, x1, y1, side]` in the game's 4:3 pixels, side -1 (left) or
    /// 1 (right). Screen-space pieces wholly inside one move with it.
    pub hud: Vec<[f32; 5]>,
    /// Use `hud` only while the 32-bit RAM word at the first address holds
    /// the second value (a game mode meaning "in play"); always if unset.
    pub hud_when: Option<[u32; 2]>,
}

impl WindowConfig {
    /// How often to draw the window when that is above the game's
    /// `frame_rate` (by more than rounding: 60 for a 59.94 game is not),
    /// given the display's rate if known.
    pub fn redraw_rate(&self, frame_rate: f64, display: Option<f64>) -> Option<f64> {
        let r = match self.refresh? {
            0.0 => display?,
            r => r,
        };
        (r > frame_rate + 1.0).then_some(r)
    }
}

/// The default `refresh`: the display's rate on PC; once per game frame
/// on Android, where drawing more often than the game makes pictures
/// only costs battery.
pub const DEFAULT_REFRESH: Option<f64> = if cfg!(target_os = "android") {
    None
} else {
    Some(0.0)
};

/// The lowest `max_render_height`: the console's own picture height.
pub const MIN_RENDER_HEIGHT: u32 = 240;

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            title: "n64rusty".into(),
            scale: 2,
            widescreen: true,
            refresh: DEFAULT_REFRESH,
            console_filter: false,
            max_render_height: None,
            hud: Vec::new(),
            hud_when: None,
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
            path: None,
            frames: None,
            frame_rate: NTSC_RATE,
            headless: false,
            replay: None,
            record: None,
            debug_port: None,
            window: WindowConfig::default(),
            start_screen: false,
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
        let mut cfg =
            RunConfig::parse(&text, base).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.path = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Writes the window settings back into the file this config came from,
    /// keeping its other keys (comments are lost). A missing file is created;
    /// a file that cannot be read is left alone and the error returned.
    pub fn save_window(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut doc: toml::Table = toml::from_str(&text).map_err(|e| e.to_string())?;
        let window = doc
            .entry("window")
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let Some(w) = window.as_table_mut() else {
            return Err("window is not a table".into());
        };
        let c = &self.window;
        w.insert("widescreen".into(), c.widescreen.into());
        w.insert("console_filter".into(), c.console_filter.into());
        w.insert("scale".into(), i64::from(c.scale).into());
        match c.refresh {
            Some(r) => w.insert("refresh".into(), r.into()),
            None => w.remove("refresh"),
        };
        let out = toml::to_string(&doc).map_err(|e| e.to_string())?;
        std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
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
        // The game's RAM is read as a 32-bit word there each frame.
        if let Some([a, _]) = cfg.window.hud_when {
            if a % 4 != 0 {
                return Err(format!(
                    "window hud_when address {a:#x} is not a multiple of 4"
                ));
            }
        }
        for (n, entry) in cfg.window.hud.iter().enumerate() {
            check_hud(entry).map_err(|e| format!("window hud entry {} {entry:?}: {e}", n + 1))?;
        }
        if let Some(h) = cfg.window.max_render_height {
            if h < MIN_RENDER_HEIGHT {
                return Err(format!(
                    "window max_render_height {h} is below {MIN_RENDER_HEIGHT}"
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

/// HUD sides: the window's left edge, and its right.
const HUD_SIDES: [f32; 2] = [-1.0, 1.0];

/// Checks one `hud` entry `[x0, y0, x1, y1, side]`: the renderer moves the
/// pieces inside that box to the window's left (-1) or right (1) edge, so
/// the box must be a real one and the side one of those two.
fn check_hud(&[x0, y0, x1, y1, side]: &[f32; 5]) -> Result<(), String> {
    if ![x0, y0, x1, y1, side].iter().all(|v| v.is_finite()) {
        return Err("every value must be a finite number".into());
    }
    if x0 > x1 || y0 > y1 {
        return Err("x0 must not be past x1, nor y0 past y1".into());
    }
    if !HUD_SIDES.contains(&side) {
        return Err(format!("side {side} is not -1 (left) or 1 (right)"));
    }
    Ok(())
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
        // Off unless asked for: the window's own resolution.
        assert_eq!(WindowConfig::default().max_render_height, None);
    }

    #[test]
    fn hud_entries_on_either_side_and_of_no_size_are_fine() {
        let text = "[window]\nhud = [[0, 0, 150, 60, -1], [200, 10, 200, 10, 1]]";
        let cfg = RunConfig::parse(text, Path::new("")).unwrap();
        assert_eq!(cfg.window.hud.len(), 2);
    }

    #[test]
    fn hud_when_must_be_word_aligned() {
        let text = "[window]\nhud_when = [0x80001236, 0]";
        assert!(RunConfig::parse(text, Path::new("")).is_err());
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
            max_render_height = 720
            hud = [[0, 0, 150, 60, -1]]
            hud_when = [0x80001234, 0]
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
        assert_eq!(cfg.window.max_render_height, Some(720));
        assert_eq!(cfg.window.hud, vec![[0.0, 0.0, 150.0, 60.0, -1.0]]);
        assert_eq!(cfg.window.hud_when, Some([0x8000_1234, 0]));
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
            ("[window]\nmax_render_height = 100", "below 240"),
            ("[window]\nhud = [[0, 0, nan, 60, -1]]", "hud entry 1"),
            ("[window]\nhud = [[0, 0, 150, inf, 1]]", "finite"),
            ("[window]\nhud = [[0, 0, 150, 60, -inf]]", "finite"),
            (
                "[window]\nhud = [[0, 0, 1, 1, 1], [151, 0, 150, 60, 1]]",
                "hud entry 2",
            ),
            ("[window]\nhud = [[0, 61, 150, 60, 1]]", "y0 past y1"),
            ("[window]\nhud = [[0, 0, 150, 60, 0]]", "side 0 is not"),
            ("[window]\nhud = [[0, 0, 150, 60, 0.5]]", "side 0.5 is not"),
            ("[window]\nmax_render_height = -1", "invalid value"),
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

    #[test]
    fn window_choices_save_back_and_redraw_rates() {
        let path = std::env::temp_dir().join(format!("run-{}.toml", std::process::id()));
        std::fs::write(&path, "frames = 9\n").unwrap();
        let mut cfg = RunConfig::load(&path).unwrap();
        cfg.window.widescreen = false;
        cfg.window.refresh = Some(120.0);
        cfg.save_window().unwrap();
        let back = RunConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.frames, Some(9));
        assert_eq!(back.window, cfg.window);
        let w = |r| WindowConfig {
            refresh: r,
            ..WindowConfig::default()
        };
        assert_eq!(w(Some(120.0)).redraw_rate(NTSC_RATE, None), Some(120.0));
        assert_eq!(w(Some(60.0)).redraw_rate(NTSC_RATE, None), None);
        assert_eq!(
            w(Some(0.0)).redraw_rate(NTSC_RATE, Some(165.0)),
            Some(165.0)
        );
        assert_eq!(w(Some(0.0)).redraw_rate(NTSC_RATE, Some(60.0)), None);
        assert_eq!(w(None).redraw_rate(NTSC_RATE, Some(165.0)), None);
        let default = WindowConfig::default().refresh;
        if cfg!(target_os = "android") {
            assert_eq!(default, None);
        } else {
            assert_eq!(default, Some(0.0));
        }
    }

    #[test]
    fn saving_window_creates_a_missing_file() {
        let path = std::env::temp_dir().join(format!("run-missing-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = RunConfig {
            path: Some(path.clone()),
            ..RunConfig::default()
        };
        cfg.save_window().unwrap();
        let back = RunConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.window, cfg.window);
    }

    #[test]
    fn saving_window_leaves_an_unreadable_file_alone() {
        let path = std::env::temp_dir().join(format!("run-bad-utf8-{}.toml", std::process::id()));
        let bytes = b"frames = 9\n\xff\n";
        std::fs::write(&path, bytes).unwrap();
        let cfg = RunConfig {
            path: Some(path.clone()),
            ..RunConfig::default()
        };
        let err = cfg.save_window().unwrap_err();
        let kept = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert_eq!(kept, bytes);

        let dir = std::env::temp_dir().join(format!("run-dir-{}.toml", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = RunConfig {
            path: Some(dir.clone()),
            ..RunConfig::default()
        };
        let result = cfg.save_window();
        let still_dir = dir.is_dir();
        std::fs::remove_dir(&dir).unwrap();
        assert!(result.is_err());
        assert!(still_dir);
    }
}
