//! Builds the player's private data into it: with `N64RUSTY_PLAYER_DATA`
//! set to a folder (its `routines.txt` and `games/<sha256>/config.toml` routine lists), the
//! player carries them, so a friend needs only the program and a ROM.
//! Without it the player carries nothing. The folder never goes in git.

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
}
