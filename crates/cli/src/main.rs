//! `n64rusty`: command-line front end for the N64 recompiler.
//!
//! Everything it writes for a game goes under `local/<sha256>/`, which is
//! git-ignored: generated Rust, reports and the per-game config.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use n64rusty_rom::Rom;
use n64rusty_translator::discover::{discover, Image};
use n64rusty_translator::{emit, translate_function};

const USAGE: &str = "\
n64rusty: a general N64 recompiler (MIPS machine code to Rust)

USAGE:
    n64rusty <COMMAND> [OPTIONS]

COMMANDS:
    info <rom>                 Print byte order, SHA-256, header and boot details
    translate <rom> [OPTIONS]  Find functions in the boot segment and write Rust
    version                    Print tool and runtime versions
    help                       Print this message

TRANSLATE OPTIONS:
    --symbols <file>   Symbol map to read function addresses from (repeatable).
                       Lines look like `name = 0x80001234;`. Read from your
                       own local checkout; never copy it into this repository.
    --out <dir>        Output crate (default: local/<sha256>/out)
    --runtime <dir>    Path to the n64rusty-runtime crate (default: this checkout's)
    --code-end <addr>  End of code in the boot segment, for symbols and
                       pointers past the code reached from the entry point
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("version") => {
            println!(
                "n64rusty {} (translator {}, runtime {})",
                env!("CARGO_PKG_VERSION"),
                n64rusty_translator::VERSION,
                n64rusty_runtime::VERSION
            );
            Ok(())
        }
        Some("info") => match args.get(1) {
            Some(p) => info(Path::new(p)),
            None => Err("info needs a ROM path".into()),
        },
        Some("translate") => translate(&args[1..]),
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command: {other}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn load(path: &Path) -> Result<Rom, String> {
    let bytes = fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Rom::from_bytes(bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn info(path: &Path) -> Result<(), String> {
    let rom = load(path)?;
    println!("byte order:   {}", rom.order);
    println!("sha256:       {}", rom.sha256);
    println!("size:         {:#x}", rom.bytes().len());
    println!("name:         {}", rom.name());
    println!("game code:    {} rev {}", rom.game_code(), rom.revision());
    println!("boot code:    {:?}", rom.cic);
    println!("header entry: {:#010x}", rom.header_entry());
    println!("entry:        {:#010x}", rom.entry());
    println!("local folder: local/{}/", rom.sha256);
    Ok(())
}

/// Reads `name = 0xADDR;` lines. Anything else is skipped.
fn read_symbols(path: &Path) -> Result<Vec<(u32, String)>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        let value = rest.trim().trim_end_matches(';').trim();
        let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        else {
            continue;
        };
        if let Ok(a) = u32::from_str_radix(hex, 16) {
            out.push((a, name.trim().to_string()));
        }
    }
    Ok(out)
}

/// Writes `contents` only if the file differs, so unchanged generated files
/// keep their timestamps and do not trigger rebuilds.
fn write_if_changed(path: &Path, contents: &str) -> Result<bool, String> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(true)
}

fn translate(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut symbols = Vec::new();
    let mut out = None;
    let mut runtime = None;
    let mut code_end = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--symbols" => symbols.push(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--runtime" => runtime = Some(PathBuf::from(value()?)),
            "--code-end" => {
                let v = value()?;
                let hex = v.trim_start_matches("0x");
                code_end =
                    Some(u32::from_str_radix(hex, 16).map_err(|e| format!("--code-end {v}: {e}"))?);
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => rom_path = Some(PathBuf::from(s)),
        }
    }
    let rom_path = rom_path.ok_or("translate needs a ROM path")?;
    let rom = load(&rom_path)?;
    let local = PathBuf::from("local").join(&rom.sha256);
    let out = out.unwrap_or_else(|| local.join("out"));
    let runtime = match runtime {
        Some(p) => p,
        None => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../runtime")),
    };
    let runtime = runtime
        .canonicalize()
        .map_err(|e| format!("runtime crate at {}: {e}", runtime.display()))?;

    let mut names = BTreeMap::new();
    for p in &symbols {
        for (a, n) in read_symbols(p)? {
            names.entry(a).or_insert(n);
        }
    }

    let img = Image {
        base: rom.entry(),
        bytes: rom.boot_segment(),
    };
    let hints: Vec<u32> = names.keys().copied().collect();
    let found = discover(&img, &[rom.entry()], &hints, code_end);

    // One file per 64 KiB of guest address space.
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut failed = Vec::new();
    for f in &found.functions {
        let mut f = f.clone();
        f.name = names.get(&f.addr).cloned();
        let src = match translate_function(&f) {
            Ok(s) => s,
            Err(e) => {
                failed.push(e.to_string());
                format!(
                    "/// Guest function at {a:#010x}: not translated ({e}).\npub fn {n}(ctx: &mut ::n64rusty_runtime::Ctx) {{\n    panic!(\"function {a:#010x} was not translated: {e}\");\n}}\n",
                    a = f.addr,
                    n = emit::fn_name(f.addr),
                    e = e.msg,
                )
            }
        };
        let file = files.entry(f.addr >> 16).or_insert_with(|| {
            "// Generated by n64rusty. Do not edit; never commit.\n\n".to_string()
        });
        file.push_str(&src);
        file.push('\n');
    }

    let src_dir = out.join("src");
    let mut lib = format!(
        "//! Recompiled boot segment of ROM {sha}.\n//! Generated by n64rusty {ver}. Do not edit; never commit.\n\n#![allow(clippy::all, non_snake_case)]\n\npub const ROM_SHA256: &str = \"{sha}\";\npub const ENTRY: u32 = {entry:#010x};\n\n",
        sha = rom.sha256,
        ver = env!("CARGO_PKG_VERSION"),
        entry = rom.entry(),
    );
    let mut register = String::from("/// Registers every recompiled function.\npub fn register(ctx: &mut ::n64rusty_runtime::Ctx) {\n");
    let mut wanted = Vec::new();
    let mut changed = 0;
    for (range, body) in &files {
        let module = format!("r_{range:04x}");
        if write_if_changed(&src_dir.join(format!("{module}.rs")), body)? {
            changed += 1;
        }
        wanted.push(format!("{module}.rs"));
        let _ = writeln!(lib, "pub mod {module};");
    }
    for f in &found.functions {
        let _ = writeln!(
            register,
            "    ctx.register({:#010x}, r_{:04x}::{});",
            f.addr,
            f.addr >> 16,
            emit::fn_name(f.addr)
        );
    }
    register.push_str("}\n");
    lib.push('\n');
    lib.push_str(&register);
    wanted.push("lib.rs".into());
    write_if_changed(&src_dir.join("lib.rs"), &lib)?;
    // Drop files for address ranges that no longer have functions.
    if let Ok(entries) = fs::read_dir(&src_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".rs") && !wanted.contains(&name) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let cargo = format!(
        "# Generated by n64rusty. Never commit.\n[package]\nname = \"n64rusty-game-{short}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nn64rusty-runtime = {{ path = {rt:?} }}\n\n# Standalone: not part of the n64rusty workspace.\n[workspace]\n",
        short = &rom.sha256[..12],
        rt = runtime.display().to_string(),
    );
    write_if_changed(&out.join("Cargo.toml"), &cargo)?;

    let config = local.join("config.toml");
    if !config.exists() {
        write_if_changed(
            &config,
            "# Per-game config for this ROM, named by its SHA-256. Local only.\n#\n# Hook addresses with role names, found by us, for example:\n# [[hook]]\n# addr = 0x80001234\n# role = \"thread_create\"\n",
        )?;
    }

    let mut report = format!(
        "rom sha256: {}\nentry: {:#010x}\nboot segment: {:#010x}..{:#010x}\ncode reached from entry ends at: {:#010x}\nfunctions: {}\njump tables: {}\nsymbols read: {}\nnot translated: {}\n",
        rom.sha256,
        rom.entry(),
        img.base,
        img.base as usize + img.bytes.len(),
        found.text_end,
        found.functions.len(),
        found.jump_tables,
        names.len(),
        failed.len(),
    );
    for f in &failed {
        let _ = writeln!(report, "  {f}");
    }
    write_if_changed(&local.join("report.txt"), &report)?;

    print!("{report}");
    println!(
        "wrote {} ({} of {} files changed)",
        out.display(),
        changed,
        files.len()
    );
    Ok(())
}
