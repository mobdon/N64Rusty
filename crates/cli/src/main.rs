//! `n64rusty`: command-line front end for the N64 recompiler.
//!
//! Everything it writes for a game goes under `local/<sha256>/`, which is
//! git-ignored: generated Rust, reports and the per-game config.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use n64rusty_rom::{Rom, BOOT_SEGMENT_ROM, BOOT_SEGMENT_SIZE};
use n64rusty_translator::discover::{
    data_pointers, discover, function_starts, overlay_table, Image,
};
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

/// Reads a table of hex numbers, `cols` per line, from a file written by
/// headless runs. A missing file is an empty table; `#` starts a comment.
fn read_table(path: &Path, cols: usize) -> Result<Vec<Vec<u32>>, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let row: Result<Vec<u32>, _> = line
            .split_whitespace()
            .map(|w| u32::from_str_radix(w.trim_start_matches("0x"), 16))
            .collect();
        match row {
            Ok(r) if r.len() == cols => out.push(r),
            _ => return Err(format!("{}: bad line `{line}`", path.display())),
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

/// Every cartridge copy, indexed by where each one starts and ends, so
/// following a run of back-to-back copies is one lookup per step (games make
/// tens of thousands of copies).
struct Copies {
    n: usize,
    by_start: HashMap<(u32, u32), (u32, u32, u32)>,
    by_end: HashMap<(u32, u32), (u32, u32, u32)>,
}

impl Copies {
    fn new(copies: &[(u32, u32, u32)]) -> Self {
        let (mut by_start, mut by_end) = (HashMap::new(), HashMap::new());
        // The first copy in the list wins, as a front-to-back search would.
        for &c in copies.iter().filter(|c| c.2 > 0) {
            by_start.entry((c.0, c.1)).or_insert(c);
            by_end
                .entry((c.0.wrapping_add(c.2), c.1.wrapping_add(c.2)))
                .or_insert(c);
        }
        Copies {
            n: copies.len(),
            by_start,
            by_end,
        }
    }

    /// The run of back-to-back copies (each one starting in ROM and RAM where
    /// the one before ends) that `c` belongs to, as one `(rom, ram, len)`.
    fn run_of(&self, c: (u32, u32, u32)) -> (u32, u32, u32) {
        let (mut lo, mut hi) = (c, c);
        // Bounded, so a wrapped-around chain cannot loop forever.
        for _ in 0..self.n {
            match self.by_end.get(&(lo.0, lo.1)).filter(|_| lo.2 > 0) {
                Some(&p) => lo = p,
                None => break,
            }
        }
        for _ in 0..self.n {
            let end = (hi.0.wrapping_add(hi.2), hi.1.wrapping_add(hi.2));
            match self.by_start.get(&end).filter(|_| hi.2 > 0) {
                Some(&n) => hi = n,
                None => break,
            }
        }
        (lo.0, lo.1, hi.0.wrapping_add(hi.2).wrapping_sub(lo.0))
    }
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
    let mut hints: Vec<u32> = names.keys().copied().collect();
    hints.extend(data_pointers(&img));
    // Code the game copies in at run time, seen by earlier headless runs.
    let dma = read_table(&local.join("dma.txt"), 3)?;
    let missing = read_table(&local.join("missing.txt"), 4)?;
    // Boot-image code reached only through pointers, found missing in runs.
    let mut entries = vec![rom.entry()];
    entries.extend(missing.iter().filter(|m| m[3] == 0).map(|m| m[0]));
    let mut found = discover(&img, &entries, &hints, code_end);

    // Boot-image functions inside a range that a copy from elsewhere in the
    // cartridge overwrites are stale: the bytes there at run time differ.
    let boot_rom = BOOT_SEGMENT_ROM as u32;
    let overwritten = |a: u32| {
        dma.iter().any(|d| {
            let (r, ram, len) = (d[0], d[1], d[2]);
            let same = r.wrapping_sub(boot_rom) == ram.wrapping_sub(img.base);
            !same && a >= ram && a < ram.wrapping_add(len)
        })
    };
    let before = found.functions.len();
    found.functions.retain(|f| !overwritten(f.addr));
    let stale = before - found.functions.len();
    // One overlay per distinct copy, with every address seen missing there.
    let mut regions: BTreeMap<(u32, u32, u32), Vec<u32>> = BTreeMap::new();
    for m in missing.iter().filter(|m| m[3] != 0) {
        regions.entry((m[1], m[2], m[3])).or_default().push(m[0]);
    }
    // Any other copy that is laid out like code is code too: a game loads
    // each scene's code this way, and waiting for a run to call into it
    // costs a crash per scene.
    for d in &dma {
        let (r, ram, len) = (d[0], d[1], d[2]);
        let start = r as usize;
        let end = (start + len as usize).min(rom.bytes().len());
        let boot_copy = r.wrapping_sub(boot_rom) == ram.wrapping_sub(img.base);
        if len == 0 || start >= end || boot_copy || regions.contains_key(&(r, ram, len)) {
            continue;
        }
        let oimg = Image {
            base: ram,
            bytes: &rom.bytes()[start..end],
        };
        if !function_starts(&oimg).is_empty() {
            regions.insert((r, ram, len), Vec::new());
        }
    }
    // Code the game's own load tables describe but no run has copied yet,
    // so a mode nobody has played yet still has its code. A copy seen in a
    // run that maps the same start the same way wins: it has the real
    // length and pieces.
    let mut planned = 0;
    for (r, ram, len) in overlay_table(rom.bytes()) {
        let in_boot = r.wrapping_sub(boot_rom) < BOOT_SEGMENT_SIZE as u32;
        let seen = dma.iter().any(|d| {
            d[2] > 0
                && r.wrapping_sub(d[0]) < d[2]
                && r.wrapping_sub(d[0]) == ram.wrapping_sub(d[1])
        });
        if !in_boot && !seen && !regions.contains_key(&(r, ram, len)) {
            regions.insert((r, ram, len), Vec::new());
            planned += 1;
        }
    }
    // Games often copy one long piece of code in several back-to-back
    // copies. Discovery looks at the whole run (so jump tables and calls
    // that cross a copy's edge resolve); each copy then gets the functions
    // that start inside it.
    let copies: Vec<(u32, u32, u32)> = dma.iter().map(|d| (d[0], d[1], d[2])).collect();
    let index = Copies::new(&copies);
    let mut runs: BTreeMap<(u32, u32, u32), Vec<u32>> = BTreeMap::new();
    for (&c, entries) in &regions {
        runs.entry(index.run_of(c)).or_default().extend(entries);
    }
    // Which run each copy belongs to, worked out once.
    let copy_runs: Vec<_> = copies
        .iter()
        .chain(regions.keys())
        .copied()
        .filter(|c| c.2 > 0)
        .map(|c| (c, index.run_of(c)))
        .collect();
    let mut overlays = Vec::new();
    for (&(r, ram, len), entries) in &runs {
        let start = r as usize;
        let end = (start + len as usize).min(rom.bytes().len());
        if start >= end {
            return Err(format!("overlay at rom {r:#x} is outside the ROM"));
        }
        let oimg = Image {
            base: ram,
            bytes: &rom.bytes()[start..end],
        };
        // Overlay code is mostly reached through pointers its own code sets
        // up, so also start at every function its layout shows.
        let mut entries = entries.clone();
        entries.extend(function_starts(&oimg));
        let d = discover(
            &oimg,
            &entries,
            &data_pointers(&oimg),
            Some(ram.wrapping_add(len)),
        );
        let mut chunks: Vec<(u32, u32, u32)> = copy_runs
            .iter()
            .filter(|(_, run)| *run == (r, ram, len))
            .map(|&(c, _)| c)
            .collect();
        chunks.sort_unstable();
        chunks.dedup();
        for (cr, cram, clen) in chunks {
            let mut part = d.clone();
            part.functions
                .retain(|f| f.addr >= cram && f.addr < cram.wrapping_add(clen));
            overlays.push(((cr, cram, clen), part));
        }
    }

    let mut failed = Vec::new();
    let mut translate_one = |f: &n64rusty_translator::Function| {
        let mut f = f.clone();
        f.name = names.get(&f.addr).cloned();
        match translate_function(&f) {
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
        }
    };
    const HEADER: &str = "// Generated by n64rusty. Do not edit; never commit.\n\n#![allow(clippy::all, non_snake_case)]\n\n";
    // One file per 64 KiB of guest address space.
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for f in &found.functions {
        let file = files
            .entry(format!("r_{:04x}", f.addr >> 16))
            .or_insert_with(|| HEADER.to_string());
        file.push_str(&translate_one(f));
        file.push('\n');
    }
    // One file per overlay; the same ROM bytes copied to two RAM places are
    // two overlays.
    for ((r, ram, _), d) in &overlays {
        let mut file = HEADER.to_string();
        for f in &d.functions {
            file.push_str(&translate_one(f));
            file.push('\n');
        }
        files.insert(format!("ovl_{r:07x}_{ram:08x}"), file);
    }

    // Each file of generated code is its own crate under `parts/`, so a
    // change in one address block rebuilds only that crate and the small top
    // crate that registers everything. Parts call each other through `ctx`,
    // so they depend only on the runtime.
    let src_dir = out.join("src");
    let parts_dir = out.join("parts");
    let short = &rom.sha256[..12];
    let rt = runtime.display().to_string();
    let mut lib = format!(
        "//! Recompiled boot segment of ROM {sha}.\n//! Generated by n64rusty {ver}. Do not edit; never commit.\n\n#![allow(clippy::all, non_snake_case)]\n\npub const ROM_SHA256: &str = \"{sha}\";\npub const ENTRY: u32 = {entry:#010x};\n\n",
        sha = rom.sha256,
        ver = env!("CARGO_PKG_VERSION"),
        entry = rom.entry(),
    );
    let mut register = String::from("/// Registers every recompiled function.\npub fn register(ctx: &mut ::n64rusty_runtime::Ctx) {\n");
    let mut deps = String::new();
    let mut changed = 0;
    for (module, body) in &files {
        let dir = parts_dir.join(module);
        let part_toml = format!(
            "# Generated by n64rusty. Never commit.\n[package]\nname = \"{module}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\nn64rusty-runtime = {{ path = {rt:?} }}\n"
        );
        write_if_changed(&dir.join("Cargo.toml"), &part_toml)?;
        if write_if_changed(&dir.join("lib.rs"), body)? {
            changed += 1;
        }
        let _ = writeln!(deps, "{module} = {{ path = \"parts/{module}\" }}");
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
    for ((r, ram, len), d) in &overlays {
        let _ = writeln!(
            register,
            "    ctx.register_overlay({r:#x}, {ram:#010x}, {len:#x}, &["
        );
        for f in &d.functions {
            let _ = writeln!(
                register,
                "        ({:#010x}, ovl_{r:07x}_{ram:08x}::{}),",
                f.addr,
                emit::fn_name(f.addr)
            );
        }
        register.push_str("    ]);\n");
    }
    register.push_str("}\n");
    lib.push('\n');
    lib.push_str(&register);
    write_if_changed(&src_dir.join("lib.rs"), &lib)?;
    // Drop parts for address ranges that no longer have functions, and the
    // single-crate files of older versions.
    if let Ok(entries) = fs::read_dir(&parts_dir) {
        for e in entries.flatten() {
            if !files.contains_key(&*e.file_name().to_string_lossy()) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&src_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".rs") && name != "lib.rs" && name != "main.rs" {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let cargo = format!(
        "# Generated by n64rusty. Never commit.\n[package]\nname = \"n64rusty-game-{short}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nn64rusty-runtime = {{ path = {rt:?} }}\nn64rusty-runner = {{ path = {host:?} }}\n{deps}\n[profile.dev]\nopt-level = 1\n\n# Standalone: not part of the n64rusty workspace.\n[workspace]\n",
        host = runtime.join("../runner").canonicalize().map_err(|e| format!("runner crate next to {}: {e}", runtime.display()))?.display().to_string(),
    );
    let main_rs = format!(
        "// Generated by n64rusty. Do not edit; never commit.\n\nfn main() -> std::process::ExitCode {{\n    n64rusty_runner::main(n64rusty_runner::Game {{\n        register: n64rusty_game_{short}::register,\n        entry: n64rusty_game_{short}::ENTRY,\n        sha256: n64rusty_game_{short}::ROM_SHA256,\n    }})\n}}\n",
    );
    write_if_changed(&src_dir.join("main.rs"), &main_rs)?;
    write_if_changed(&out.join("Cargo.toml"), &cargo)?;
    // Generated code is never formatted: a format pass would rewrite every
    // file and rebuild everything.
    write_if_changed(&out.join("rustfmt.toml"), "disable_all_formatting = true\n")?;

    let config = local.join("config.toml");
    if !config.exists() {
        write_if_changed(
            &config,
            "# Per-game config for this ROM, named by its SHA-256. Local only.\n#\n# Hook addresses with role names, found by us, for example:\n# [[hook]]\n# addr = 0x80001234\n# role = \"thread_create\"\n",
        )?;
    }

    let mut report = format!(
        "rom sha256: {}\nentry: {:#010x}\nboot segment: {:#010x}..{:#010x}\ncode reached from entry ends at: {:#010x}\nfunctions: {}\njump tables: {}\nsymbols read: {}\nstale boot-image functions dropped: {}\noverlays: {} ({} from load tables, not yet seen in a run)\nnot translated: {}\n",
        rom.sha256,
        rom.entry(),
        img.base,
        img.base as usize + img.bytes.len(),
        found.text_end,
        found.functions.len(),
        found.jump_tables,
        names.len(),
        stale,
        overlays.len(),
        planned,
        failed.len(),
    );
    for ((r, ram, len), d) in &overlays {
        let _ = writeln!(
            report,
            "  overlay rom {r:#x} -> {ram:#010x} len {len:#x}: {} functions",
            d.functions.len()
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_to_back_copies_form_one_run() {
        let copies = [
            (0x1000, 0x8030_0000, 0x100),
            (0x1100, 0x8030_0100, 0x80),
            (0x1180, 0x8030_0180, 0x40),
            // Same ROM continues, but to another RAM place: not the same run.
            (0x11c0, 0x8040_0000, 0x40),
            (0x5000, 0x8030_0000, 0x0),
        ];
        let run = (0x1000, 0x8030_0000, 0x1c0);
        let index = Copies::new(&copies);
        for c in &copies[..3] {
            assert_eq!(index.run_of(*c), run);
        }
        assert_eq!(index.run_of(copies[3]), copies[3]);
        assert_eq!(index.run_of(copies[4]), copies[4]);
        // A copy not in the list still finds its neighbours.
        assert_eq!(index.run_of((0x0f00, 0x802f_ff00, 0x100)).2, 0x2c0);
    }

    #[test]
    fn reads_run_tables() {
        let dir = std::env::temp_dir().join(format!("n64rusty-table-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dma.txt");
        fs::write(
            &p,
            "# rom ram length\n0x00001000 0x80000400 0x100\n\n2000 80100000 40\n",
        )
        .unwrap();
        let t = read_table(&p, 3).unwrap();
        assert_eq!(
            t,
            vec![
                vec![0x1000, 0x8000_0400, 0x100],
                vec![0x2000, 0x8010_0000, 0x40]
            ]
        );
        assert!(read_table(&p, 4).is_err());
        assert!(read_table(&dir.join("absent.txt"), 3).unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
