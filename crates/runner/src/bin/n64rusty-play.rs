//! Plays any N64 ROM with nothing built for it: every function runs in the
//! reference interpreter, and system routines are replaced as usual.
//!
//! Double-click it and it plays the ROM given (dropped on it), else shows a
//! start screen of the ROMs played before, else plays one in its own folder,
//! else asks for one. Files
//! live next to the program: `games/<sha256>/` per game (routine list,
//! saves, an optional run.toml), `settings.toml` (window choices made in
//! the menu, for games without their own run.toml), `recent.txt` (the ROMs played, newest first, for the Game menu), and
//! `routines.txt`, the fingerprints
//! that find a game's system routines when it has no routine list yet.
//! With more arguments than the ROM, they work as for a generated game
//! program (`--frames`, `--capture`, `--window`, ...); `--symbols map.txt`
//! writes the game's routine list from a symbol map and adds its routines'
//! fingerprints to `routines.txt`.

// No console window on Windows: messages go to the console it was started
// from, else to n64rusty-play.log next to the program.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use n64rusty_host::launcher::Page;
use n64rusty_host::one_window::Next;
use n64rusty_rom::{signature, Rom};
use n64rusty_runner::{main_args, os, read_names, routine_list, Game};

/// Files built into the player (see build.rs): fingerprints and routine
/// lists for games the fingerprints alone cannot set up.
mod built {
    include!(concat!(env!("OUT_DIR"), "/player_data.rs"));
    pub fn get(path: &str) -> Option<&'static str> {
        FILES.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

/// First line of a routine list written from `--symbols`: kept over the
/// built-in one.
const FROM_MAP: &str = "// from a symbol map";

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect();
    start(args, &n64rusty_host::home().unwrap_or_default())
}

/// The player with `args`, its files in `dir`. The Android app (which
/// builds this file into itself) calls it with none.
pub fn start(mut args: Vec<String>, dir: &Path) -> ExitCode {
    let dir = dir.to_path_buf();
    // Only a ROM (or nothing): a friend's double-click or drop.
    let plain = args.len() <= 1;
    let log = console(&dir);
    let code = play(&mut args, &dir, plain);
    if code != ExitCode::SUCCESS {
        if let Some(log) = log {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(8).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            error_box(&format!(
                "{}\n\nFull log: {}",
                tail.join("\n"),
                log.display()
            ));
        }
    }
    code
}

/// Sends output to the console the program was started from, or to a log
/// file next to it when there is none (a double-click). Returns the log.
#[cfg(windows)]
fn console(dir: &Path) -> Option<PathBuf> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    // SAFETY: plain Win32 calls with a handle this process owns from here on.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            return None;
        }
        let log = dir.join("n64rusty-play.log");
        let h = std::fs::File::create(&log).ok()?.into_raw_handle();
        SetStdHandle(STD_OUTPUT_HANDLE, h as _);
        SetStdHandle(STD_ERROR_HANDLE, h as _);
        Some(log)
    }
}

#[cfg(not(windows))]
fn console(_dir: &Path) -> Option<PathBuf> {
    None
}

/// The error on the 8-bit message screen, or in a plain Windows box when
/// that screen cannot open.
fn error_box(text: &str) {
    let shown = n64rusty_host::launcher::message("Could not start", text);
    #[cfg(windows)]
    if !shown {
        rfd::MessageDialog::new()
            .set_title("n64rusty-play could not start the game")
            .set_description(text)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
    #[cfg(not(windows))]
    let _ = shown;
}

fn play(args: &mut Vec<String>, dir: &Path, plain: bool) -> ExitCode {
    // A new copy of the player starts from the built-in window settings,
    // so the start screen and every game show them from the first run.
    let settings = dir.join("settings.toml");
    if let (false, Some(built)) = (settings.exists(), built::get("settings.toml")) {
        let _ = std::fs::write(&settings, built);
    }
    // Its Game menu can go back to the start screen.
    n64rusty_host::menu::START_SCREEN.store(true, std::sync::atomic::Ordering::Relaxed);
    if args.first().is_none_or(|a| a.starts_with("--")) {
        let recent = n64rusty_host::menu::recent_roms();
        let page = match args.as_slice() {
            [] => Some(Page::Games),
            [flag, page] if flag == "--screen" => Page::parse(page),
            _ => None,
        };
        if let Some(page) = page {
            // The game plays in the start screen's window, as a double-click
            // on that ROM would.
            let Some(rom) = n64rusty_host::launcher::pick(&games(dir), page) else {
                return ExitCode::SUCCESS;
            };
            *args = vec![rom.display().to_string()];
            return play_on(args, dir, true);
        }
        let remembered = recent.into_iter().next();
        let Some(rom) = remembered.or_else(|| find_rom(dir)).or_else(pick_rom) else {
            eprintln!(
                "No ROM given. Drop your .z64, .n64 or .v64 file on this program, or put it in {}",
                dir.display()
            );
            return ExitCode::from(2);
        };
        args.insert(0, rom.display().to_string());
    }
    play_on(args, dir, plain)
}

/// Plays the ROM `args` starts with, then wherever its menu goes next: the
/// program goes on in the same window, so screen capture never loses it.
fn play_on(args: &mut Vec<String>, dir: &Path, mut plain: bool) -> ExitCode {
    loop {
        let this = args.clone();
        let code = match played(args, dir, plain) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                // Command-line runs (scripts, tests) stop with the error.
                if !plain {
                    return ExitCode::from(2);
                }
                // A player sees why, then picks another game in the same
                // window.
                if !n64rusty_host::launcher::message("Can't play this one", &friendly(&e)) {
                    // No window could open: the plain error box shows it.
                    return ExitCode::from(2);
                }
                if !n64rusty_host::one_window::kept() {
                    // Closed instead of dismissed.
                    return ExitCode::SUCCESS;
                }
                let Some(rom) = n64rusty_host::launcher::pick(&games(dir), Page::Games) else {
                    return ExitCode::SUCCESS;
                };
                *args = vec![rom.display().to_string()];
                continue;
            }
        };
        let rom = match n64rusty_host::one_window::take_next() {
            None => return code,
            Some(Next::Again) => {
                *args = this;
                continue;
            }
            Some(Next::Rom(rom)) => rom,
            Some(Next::Crashed(e)) => {
                let text = format!(
                    "The game crashed, so it was stopped. Pick a game to play.\n\n{e}\n\nDetails: crash.txt next to the player."
                );
                if !n64rusty_host::launcher::message("Game crashed", &text)
                    || !n64rusty_host::one_window::kept()
                {
                    return ExitCode::SUCCESS;
                }
                let Some(rom) = n64rusty_host::launcher::pick(&games(dir), Page::Games) else {
                    return ExitCode::SUCCESS;
                };
                rom
            }
            Some(Next::Screen(page)) => {
                let Some(page) = Page::parse(page) else {
                    return code;
                };
                let Some(rom) = n64rusty_host::launcher::pick(&games(dir), page) else {
                    return ExitCode::SUCCESS;
                };
                rom
            }
        };
        *args = vec![rom.display().to_string()];
        plain = true;
    }
}

/// What a player is told when a game cannot start.
fn friendly(error: &str) -> String {
    if error.contains("no system routines found") {
        "This game has not been set up for N64Rusty yet, so it cannot run. \
         Pick another game."
            .into()
    } else {
        format!("This file could not be opened as an N64 game.\n\n{error}")
    }
}

#[test]
fn a_game_not_set_up_yet_gets_a_plain_message() {
    let e = "no system routines found in this ROM, so it cannot run yet (x has 3 fingerprints)";
    assert!(friendly(e).contains("not been set up"));
    assert!(friendly("game.z64: not an N64 ROM").contains("not an N64 ROM"));
}

/// The start screen's games: those played before, then every ROM in `dir`
/// (the player's folder), so a friend only drops ROMs next to it.
fn games(dir: &Path) -> Vec<PathBuf> {
    let mut games = n64rusty_host::menu::recent_roms();
    for rom in find_roms(dir) {
        if !games.contains(&rom) {
            games.push(rom);
        }
    }
    games
}

/// Plays the ROM `args` starts with. `Err` when it cannot start: not a ROM,
/// or a game the player cannot set up yet.
fn played(args: &mut Vec<String>, dir: &Path, plain: bool) -> Result<ExitCode, String> {
    let recent = dir.join("recent.txt");
    let rom = std::fs::read(&args[0])
        .map_err(|e| e.to_string())
        .and_then(|b| Rom::from_bytes(b).map_err(|e| e.to_string()))
        .map_err(|e| format!("{}: {e}", args[0]))?;
    let local = dir.join("games").join(&rom.sha256);
    routines(args, dir, &rom, &local)?;
    // Remembered once it is known to be a ROM the player can set up.
    if plain {
        let rom = std::path::absolute(&args[0]).unwrap_or_else(|_| PathBuf::from(&args[0]));
        let list = std::fs::read_to_string(&recent).unwrap_or_default();
        let _ = std::fs::write(&recent, remember(&rom.display().to_string(), &list));
    }
    if !args.iter().any(|a| a == "--local") {
        args.extend(["--local".into(), local.display().to_string()]);
    }
    if plain {
        args.push("--window".into());
    }
    // Menu choices are saved in the game's own run.toml, else in
    // settings.toml next to the program, shared by every game.
    // The folder the run uses: the given --local, else the game's.
    let used = args
        .windows(2)
        .find(|w| w[0] == "--local")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or(local);
    // Built-in game settings (widescreen HUD areas). They replace a run.toml
    // without them; menu choices saved there go back to the defaults once.
    if let Some(run) = built::get(&format!("games/{}/run.toml", rom.sha256)) {
        let path = used.join("run.toml");
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        if !have.contains("hud") {
            let _ = std::fs::write(&path, run);
        }
    }
    if !used.join("run.toml").exists() && !args.iter().any(|a| a == "--run") {
        let settings = dir.join("settings.toml");
        // A folder it cannot write to just plays with the defaults.
        if settings.exists() || std::fs::write(&settings, "").is_ok() {
            args.extend(["--run".into(), settings.display().to_string()]);
        }
    }
    // Like a console with the Expansion Pak: games that need it run, and
    // the others only see more free memory.
    if !args.iter().any(|a| a == "--expansion") {
        args.push("--expansion".into());
    }
    let game = Game {
        register: |_| {},
        entry: rom.entry(),
        sha256: rom.sha256.clone().leak(),
    };
    Ok(main_args(game, None, std::mem::take(args)))
}

/// Makes sure the game has a routine list: from `--symbols` maps (also
/// adding their fingerprints to `routines.txt`), or found with the
/// fingerprints when there is none yet.
fn routines(args: &[String], dir: &Path, rom: &Rom, local: &Path) -> Result<(), String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    std::fs::create_dir_all(local).map_err(|e| err(local, e))?;
    let config = local.join("config.toml");
    let prints = dir.join("routines.txt");
    let code = signature::words(rom.boot_segment());
    let maps: Vec<PathBuf> = args
        .windows(2)
        .filter(|w| w[0] == "--symbols")
        .map(|w| PathBuf::from(&w[1]))
        .collect();
    if !maps.is_empty() {
        let names = list_from_maps(&maps, &config)?;
        println!("wrote the routine list to {}", config.display());
        let wanted = read_names(std::slice::from_ref(&config))?
            .into_values()
            .collect();
        let mut lines: BTreeSet<String> = std::fs::read_to_string(&prints)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        lines.extend(
            signature::make(&code, rom.entry(), &names, &wanted)
                .lines()
                .map(str::to_string),
        );
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&prints, text).map_err(|e| err(&prints, e))?;
        return Ok(());
    }
    let have = std::fs::read_to_string(&config).ok();
    if let Some(list) = built::get(&format!("games/{}/config.toml", rom.sha256)) {
        // Also replaces a list an older player found by fingerprint.
        if !have.is_some_and(|h| h.starts_with(FROM_MAP)) {
            return std::fs::write(&config, list).map_err(|e| err(&config, e));
        }
        return Ok(());
    }
    if have.is_some() {
        return Ok(());
    }
    let known = std::fs::read_to_string(&prints).unwrap_or_default()
        + "\n"
        + built::get("routines.txt").unwrap_or_default();
    let found = signature::find(&known, &code, rom.entry());
    let wanted = os::HOOKS.len();
    println!(
        "found {} system routines by fingerprint ({} known kinds); saved in {}",
        found.len(),
        wanted,
        config.display()
    );
    if found.is_empty() {
        return Err(format!(
            "no system routines found in this ROM, so it cannot run yet ({} has {} fingerprints)",
            prints.display(),
            known.lines().count()
        ));
    }
    let list: String = found
        .iter()
        .map(|(a, n)| format!("{n} = {a:#010x}\n"))
        .collect();
    std::fs::write(&config, list).map_err(|e| err(&config, e))
}

/// Writes `config` with the routines named in the symbol `maps` and returns
/// every name the maps hold. Leaves `config` as it was when a map cannot be
/// read or the maps name none of the routines the runner needs, since the
/// `FROM_MAP` header would keep that empty list over the built-in one.
fn list_from_maps(maps: &[PathBuf], config: &Path) -> Result<BTreeMap<u32, String>, String> {
    let names = read_names(maps)?;
    let routines = routine_list(&names);
    if routines.is_empty() {
        return Err(format!(
            "the symbol maps name none of the routines the runner needs, so {} was left as it was",
            config.display()
        ));
    }
    std::fs::write(config, format!("{FROM_MAP}\n{routines}"))
        .map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(names)
}

#[cfg(test)]
mod list_from_maps_tests {
    use super::*;

    /// A fresh folder for one test, so tests running at once do not share files.
    fn folder(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("n64rusty-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const OLD_LIST: &str = "// from a symbol map\nosCreateThread = 0x80001000\n";

    #[test]
    fn an_unreadable_map_names_its_path_and_keeps_the_list() {
        let dir = folder("unreadable-map");
        let config = dir.join("config.toml");
        std::fs::write(&config, OLD_LIST).unwrap();
        let map = dir.join("typo.map");
        let e = list_from_maps(std::slice::from_ref(&map), &config).unwrap_err();
        let kept = std::fs::read_to_string(&config).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(e.contains(&map.display().to_string()), "{e}");
        assert_eq!(kept, OLD_LIST);
    }

    #[test]
    fn a_map_without_needed_routines_writes_nothing() {
        let dir = folder("empty-map");
        let config = dir.join("config.toml");
        std::fs::write(&config, OLD_LIST).unwrap();
        let map = dir.join("game.map");
        std::fs::write(&map, "some_game_function = 0x80002000;\n").unwrap();
        let e = list_from_maps(std::slice::from_ref(&map), &config).unwrap_err();
        let kept = std::fs::read_to_string(&config).unwrap();
        let fresh = list_from_maps(std::slice::from_ref(&map), &dir.join("new.toml"));
        let created = dir.join("new.toml").exists();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(e.contains("none of the routines"), "{e}");
        assert_eq!(kept, OLD_LIST);
        assert!(fresh.is_err());
        assert!(!created);
    }

    #[test]
    fn a_map_with_routines_writes_the_list() {
        let dir = folder("good-map");
        let config = dir.join("config.toml");
        let map = dir.join("game.map");
        std::fs::write(
            &map,
            "osCreateThread = 0x80001000;\nsome_game_function = 0x80002000;\n",
        )
        .unwrap();
        let names = list_from_maps(std::slice::from_ref(&map), &config).unwrap();
        let list = std::fs::read_to_string(&config).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(list, format!("{FROM_MAP}\nosCreateThread = 0x80001000\n"));
    }
}

/// The recent list with `rom` moved to the top, at most ten entries.
fn remember(rom: &str, list: &str) -> String {
    std::iter::once(rom)
        .chain(list.lines().filter(|l| *l != rom && !l.is_empty()))
        .take(10)
        .map(|l| format!("{l}\n"))
        .collect()
}

#[test]
fn recent_list_moves_a_rom_to_the_top() {
    assert_eq!(remember("b", "a\nb\nc\n"), "b\na\nc\n");
    assert_eq!(remember("x", ""), "x\n");
}

/// The first ROM file in `dir`, by name.
fn find_rom(dir: &Path) -> Option<PathBuf> {
    find_roms(dir).into_iter().next()
}

/// The ROM files in `dir`, by name.
fn find_roms(dir: &Path) -> Vec<PathBuf> {
    let Ok(files) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut roms: Vec<PathBuf> = files
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["z64", "n64", "v64"].contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    roms.sort();
    roms
}

#[test]
fn every_rom_in_the_folder_is_found() {
    let dir = std::env::temp_dir().join(format!("n64rusty-roms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["b.Z64", "a.n64", "c.v64", "notes.txt"] {
        std::fs::write(dir.join(f), "").unwrap();
    }
    let names: Vec<String> = find_roms(&dir)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(names, ["a.n64", "b.Z64", "c.v64"]);
}

#[cfg(windows)]
fn pick_rom() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Pick your N64 ROM")
        .add_filter("N64 ROM", &["z64", "n64", "v64"])
        .pick_file()
}

#[cfg(not(windows))]
fn pick_rom() -> Option<PathBuf> {
    None
}
