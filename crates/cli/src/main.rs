//! `n64rusty`: command-line front end for the N64 recompiler.
//!
//! No commands do real work yet. Later steps add ROM loading (all three byte
//! orders), hashing, and translation into a local, git-ignored output folder.

use std::process::ExitCode;

const USAGE: &str = "\
n64rusty: a general N64 recompiler (MIPS machine code to Rust)

USAGE:
    n64rusty <COMMAND>

COMMANDS:
    version    Print tool and runtime versions
    help       Print this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") => {
            println!(
                "n64rusty {} (translator {}, runtime {})",
                env!("CARGO_PKG_VERSION"),
                n64rusty_translator::VERSION,
                n64rusty_runtime::VERSION
            );
            ExitCode::SUCCESS
        }
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
