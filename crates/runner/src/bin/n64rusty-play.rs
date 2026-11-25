//! Plays any N64 ROM with nothing built for it: every function runs in the
//! reference interpreter, and system routines are replaced as usual.
//!
//! Double-click it and it plays the ROM given (dropped on it), else the one
//! picked last time, else one in its own folder, else asks for one. Files
//! live next to the program: `games/<sha256>/` per game (routine list,
//! saves, an optional run.toml), `settings.toml` (window choices made in
//! the menu, for games without their own run.toml), `last-rom.txt`, and
//! `routines.txt`, the fingerprints
//! that find a game's system routines when it has no routine list yet.
//! With more arguments than the ROM, they work as for a generated game
//! program (`--frames`, `--capture`, `--window`, ...); `--symbols map.txt`
//! writes the game's routine list from a symbol map and adds its routines'
//! fingerprints to `routines.txt`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use n64rusty_rom::{signature, Rom};
use n64rusty_runner::{main_args, os, read_names, routine_list, Game};

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    // Only a ROM (or nothing): a friend's double-click or drop.
    let plain = args.len() <= 1;
    let code = play(&mut args, &dir, plain);
    if plain && code != ExitCode::SUCCESS {
        // Keep the console open so the message can be read.
        eprintln!("Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    code
}

fn play(args: &mut Vec<String>, dir: &Path, plain: bool) -> ExitCode {
    let last = dir.join("last-rom.txt");
    if args.first().is_none_or(|a| a.starts_with("--")) {
        let remembered = std::fs::read_to_string(&last)
            .ok()
            .map(|t| PathBuf::from(t.trim()))
            .filter(|p| p.is_file());
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
        let _ = std::fs::write(&last, &args[0]);
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
        let list = routine_list(&names);
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
    if config.exists() {
        return Ok(());
    }
    let known = std::fs::read_to_string(&prints).unwrap_or_default();
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
