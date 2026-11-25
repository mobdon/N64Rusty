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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
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

#[cfg(windows)]
fn error_box(text: &str) {
    rfd::MessageDialog::new()
        .set_title("n64rusty-play could not start the game")
        .set_description(text)
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[cfg(not(windows))]
fn error_box(_text: &str) {}

fn play(args: &mut Vec<String>, dir: &Path, plain: bool) -> ExitCode {
    // Its Game menu can go back to the start screen.
    n64rusty_host::menu::START_SCREEN.store(true, std::sync::atomic::Ordering::Relaxed);
    let recent = dir.join("recent.txt");
    if args.first().is_none_or(|a| a.starts_with("--")) {
        let recent = n64rusty_host::menu::recent_roms();
        if args.is_empty() && !recent.is_empty() {
            // The start screen has its own window, and a program gets one
            // window loop: the game plays in a fresh start of the program.
            let Some(rom) = n64rusty_host::launcher::pick(&recent) else {
                return ExitCode::SUCCESS;
            };
            let started =
                std::env::current_exe().and_then(|exe| Command::new(exe).arg(&rom).spawn());
            return match started {
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: starting {}: {e}", rom.display());
                    ExitCode::from(2)
                }
            };
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
    let rom = match std::fs::read(&args[0])
        .map_err(|e| e.to_string())
        .and_then(|b| Rom::from_bytes(b).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}: {e}", args[0]);
            return ExitCode::from(2);
        }
    };
    let local = dir.join("games").join(&rom.sha256);
    if let Err(e) = routines(args, dir, &rom, &local) {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }
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
    main_args(game, None, std::mem::take(args))
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
        let names = read_names(&maps);
        let list = format!("{FROM_MAP}\n{}", routine_list(&names));
        std::fs::write(&config, &list).map_err(|e| err(&config, e))?;
        println!("wrote the routine list to {}", config.display());
        let wanted = read_names(std::slice::from_ref(&config))
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
    let mut roms: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["z64", "n64", "v64"].contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    roms.sort();
    roms.into_iter().next()
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
