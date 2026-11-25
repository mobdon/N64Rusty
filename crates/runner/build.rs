//! Builds the player's private data into it: with `N64RUSTY_PLAYER_DATA`
//! set to a folder (its `routines.txt` and `games/<sha256>/config.toml` routine lists), the
//! player carries them, so a friend needs only the program and a ROM.
//! Without it the player carries nothing. The folder never goes in git.
//!
//! With `N64RUSTY_MODS` set to a folder, every `.rs` file in it is built in
//! as a mod (see `mods` in lib.rs). The folder never goes in git either.

use std::fmt::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=N64RUSTY_PLAYER_DATA");
    let mut out = String::from("pub const FILES: &[(&str, &str)] = &[\n");
    if let Some(dir) = std::env::var_os("N64RUSTY_PLAYER_DATA") {
        let dir = Path::new(&dir);
        println!("cargo:rerun-if-changed={}", dir.display());
        let mut files = vec!["routines.txt".to_string()];
        let games =
            std::fs::read_dir(dir.join("games")).expect("N64RUSTY_PLAYER_DATA has no games folder");
        for game in games.flatten() {
            let name = game.file_name().to_string_lossy().into_owned();
            files.push(format!("games/{name}/config.toml"));
        }
        files.sort();
        for f in files {
            let p = dir.join(&f);
            if p.is_file() {
                println!("cargo:rerun-if-changed={}", p.display());
                writeln!(
                    out,
                    "    ({f:?}, include_str!({:?})),",
                    p.canonicalize().unwrap()
                )
                .unwrap();
            }
        }
    }
    out.push_str("];\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("player_data.rs");
    std::fs::write(dest, out).unwrap();
    mods();
}

/// Writes `mods.rs`: each mod file as a module, and one `install` that
/// runs them all in file-name order.
fn mods() {
    println!("cargo:rerun-if-env-changed=N64RUSTY_MODS");
    let mut files = Vec::new();
    if let Some(dir) = std::env::var_os("N64RUSTY_MODS") {
        println!("cargo:rerun-if-changed={}", Path::new(&dir).display());
        for e in std::fs::read_dir(&dir)
            .expect("N64RUSTY_MODS is not a folder")
            .flatten()
        {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "rs") {
                println!("cargo:rerun-if-changed={}", p.display());
                files.push(p.canonicalize().unwrap());
            }
        }
    }
    files.sort();
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        writeln!(out, "#[path = {:?}]\nmod m{i};", f.display().to_string()).unwrap();
    }
    writeln!(out, "pub const ANY: bool = {};", !files.is_empty()).unwrap();
    out.push_str("pub fn install(_ctx: &mut Ctx, _rom: &mut Vec<u8>, _sha256: &str) {\n");
    for i in 0..files.len() {
        writeln!(out, "    m{i}::install(_ctx, _rom, _sha256);").unwrap();
    }
    out.push_str("}\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("mods.rs");
    std::fs::write(dest, out).unwrap();
}
