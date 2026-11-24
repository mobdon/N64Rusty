//! One config file per run. It says everything about the run, so the same
//! file gives the same run: which ROM, how many controllers, the key
//! bindings, an input script to replay or a file to record to, a frame limit
//! and the debug port. Unknown keys are errors, so a typo cannot be ignored.
//! See `run.example.toml` in this crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    /// The player's own ROM.
    pub rom: Option<PathBuf>,
    /// Controllers plugged in, 1 to 4. Higher ports always read as neutral.
    pub controllers: u8,
    /// Frames per second for pacing in the window.
    pub rate_hz: f64,
    /// Stop after this many frames.
    pub frames: Option<u64>,
    /// Input script to play instead of live input.
    pub replay: Option<PathBuf>,
    /// Write the input of every frame here when the run ends.
    pub record: Option<PathBuf>,
    /// Listen on 127.0.0.1 at this port (0 picks a free one) for a test
    /// driver; the game waits for it before every frame.
    pub debug_port: Option<u16>,
    /// Read gamepads (window runs only).
    pub gamepads: bool,
    /// `[keys.1]` to `[keys.4]`: input name to key name. Leave out for defaults.
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            rom: None,
            controllers: 1,
            rate_hz: 60.0,
            frames: None,
            replay: None,
            record: None,
            debug_port: None,
            gamepads: true,
            keys: BTreeMap::new(),
        }
    }
}

impl RunConfig {
    /// Read a config file. Relative paths in it are relative to the file.
    pub fn load(path: &Path) -> Result<RunConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cfg = RunConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        for p in [&mut cfg.rom, &mut cfg.replay, &mut cfg.record]
            .into_iter()
            .flatten()
        {
            *p = dir.join(&*p);
        }
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<RunConfig, String> {
        let cfg: RunConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        if !(1..=4).contains(&cfg.controllers) {
            return Err("controllers must be 1 to 4".into());
        }
        if !(cfg.rate_hz.is_finite() && cfg.rate_hz > 0.0) {
            return Err("rate_hz must be above 0".into());
        }
        crate::keymap::KeyMap::from_config(&cfg.keys)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_file_parses() {
        let cfg = RunConfig::parse(include_str!("../run.example.toml")).unwrap();
        assert_eq!(cfg.controllers, 1);
        assert!(cfg.keys.contains_key("1"));
    }

    #[test]
    fn empty_file_gives_defaults() {
        let cfg = RunConfig::parse("").unwrap();
        assert_eq!(
            (cfg.controllers, cfg.rate_hz, cfg.gamepads),
            (1, 60.0, true)
        );
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        for bad in [
            "frame = 10",
            "controllers = 5",
            "rate_hz = 0.0",
            "[keys.1]\nSTICK_UP = \"ArrowUp\"",
        ] {
            assert!(RunConfig::parse(bad).is_err(), "{bad}");
        }
    }
}
