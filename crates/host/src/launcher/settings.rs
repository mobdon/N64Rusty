//! The window choices on disk: `settings.toml` next to the program, shared
//! by every game, and each game's own `run.toml`.

use std::path::{Path, PathBuf};

use crate::config::RunConfig;

/// Settings next to the program, shared by every game.
fn settings_path() -> Option<PathBuf> {
    Some(crate::home()?.join("settings.toml"))
}

/// The shared settings, and why they could not be read when that failed.
pub(super) fn load_settings() -> (RunConfig, Option<String>) {
    match settings_path() {
        Some(path) => load_settings_from(&path),
        None => (RunConfig::default(), None),
    }
}

/// [`load_settings`] from `path`. A missing file is the first run, not an
/// error; any other failure gives the defaults and says why, and the
/// choices are still saved to `path`.
fn load_settings_from(path: &Path) -> (RunConfig, Option<String>) {
    let (mut cfg, error) = match RunConfig::load(path) {
        Ok(cfg) => (cfg, None),
        Err(_) if !path.exists() => (RunConfig::default(), None),
        Err(e) => (
            RunConfig::default(),
            Some(format!("{e}; using the default settings")),
        ),
    };
    cfg.path = Some(path.to_path_buf());
    (cfg, error)
}

/// Saves the window choices to `settings.toml` and to each game's own
/// `run.toml` (whose other keys, such as HUD areas, stay). A failure for
/// one file does not stop the others; every failure is returned, one per
/// line.
pub(super) fn save_everywhere(cfg: &RunConfig) -> Result<(), String> {
    let mut failed = Vec::new();
    if let Err(e) = cfg.save_window() {
        failed.push(e);
    }
    if let Some(games) = cfg
        .path
        .as_deref()
        .and_then(Path::parent)
        .map(|d| d.join("games"))
    {
        save_to_games(cfg, &games, &mut failed);
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("\n"))
    }
}

/// Copies the window choices into `games/*/run.toml`, adding each failure
/// to `failed`. A game folder without its own settings has nothing to
/// change; a missing `games` folder has no games.
fn save_to_games(cfg: &RunConfig, games: &Path, failed: &mut Vec<String>) {
    let entries = match std::fs::read_dir(games) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return failed.push(format!("{}: {e}", games.display())),
    };
    for game in entries {
        let game = match game {
            Ok(game) => game,
            Err(e) => {
                failed.push(format!("{}: {e}", games.display()));
                continue;
            }
        };
        let path = game.path().join("run.toml");
        if !path.exists() {
            continue;
        }
        let saved = RunConfig::load(&path).and_then(|mut own| {
            own.window.widescreen = cfg.window.widescreen;
            own.window.console_filter = cfg.window.console_filter;
            own.window.scale = cfg.window.scale;
            own.window.refresh = cfg.window.refresh;
            own.save_window()
        });
        if let Err(e) = saved {
            failed.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("n64rusty-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_settings_file_is_the_first_run() {
        let dir = fresh_dir("settings-missing");
        let path = dir.join("settings.toml");
        let (cfg, error) = load_settings_from(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(error, None);
        assert_eq!(cfg.path, Some(path));
    }

    #[test]
    fn a_broken_settings_file_is_reported() {
        let dir = fresh_dir("settings-broken");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "[window\n").unwrap();
        let (cfg, error) = load_settings_from(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        let error = error.expect("the broken file was not reported");
        assert!(error.contains("settings.toml"), "{error}");
        assert_eq!(cfg.window, RunConfig::default().window);
        assert_eq!(cfg.path, Some(path));
    }

    #[test]
    fn every_game_is_saved_and_every_failure_reported() {
        let dir = fresh_dir("settings-everywhere");
        let games = dir.join("games");
        let run = |g: &str| games.join(g).join("run.toml");
        for g in ["a-broken", "b-read-only", "c-good", "d-no-settings"] {
            std::fs::create_dir_all(games.join(g)).unwrap();
        }
        std::fs::write(run("a-broken"), "[window\n").unwrap();
        std::fs::write(run("b-read-only"), "frames = 1\n").unwrap();
        let mut ro = std::fs::metadata(run("b-read-only")).unwrap().permissions();
        ro.set_readonly(true);
        std::fs::set_permissions(run("b-read-only"), ro.clone()).unwrap();
        std::fs::write(run("c-good"), "frames = 2\n").unwrap();
        let mut cfg = RunConfig {
            path: Some(dir.join("settings.toml")),
            ..RunConfig::default()
        };
        cfg.window.widescreen = false;

        let result = save_everywhere(&cfg);
        let good = RunConfig::load(&run("c-good"));
        let shared = RunConfig::load(&dir.join("settings.toml"));
        let no_settings = run("d-no-settings").exists();
        #[allow(clippy::permissions_set_readonly_false)]
        ro.set_readonly(false);
        std::fs::set_permissions(run("b-read-only"), ro).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let err = result.expect_err("the failures were not reported");
        assert!(err.contains("a-broken"), "{err}");
        assert!(err.contains("b-read-only"), "{err}");
        assert!(!err.contains("c-good"), "{err}");
        assert!(
            !good.unwrap().window.widescreen,
            "a later game was not saved"
        );
        assert!(!shared.unwrap().window.widescreen);
        assert!(!no_settings, "a game without settings got some");
    }
}
