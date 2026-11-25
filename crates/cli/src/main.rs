//! `n64rusty`: command-line front end for the N64 recompiler.
//!
//! Everything it writes for a game goes under `local/<sha256>/`, which is
//! git-ignored: generated Rust, reports and the per-game config.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use n64rusty_rom::signature::{masked, starts_function, words};
use n64rusty_rom::{Rom, BOOT_SEGMENT_ROM, BOOT_SEGMENT_SIZE};
use n64rusty_translator::discover::{
    data_pointers, discover, discover_layout, function_starts, overlay_table, Discovery, Image,
};
use n64rusty_translator::{emit, translate_function};

const USAGE: &str = "\
n64rusty: a general N64 recompiler (MIPS machine code to Rust)

USAGE:
    n64rusty <COMMAND> [OPTIONS]

COMMANDS:
    info <rom>                 Print byte order, SHA-256, header and boot details
    translate <rom> [OPTIONS]  Find functions in the boot segment and write Rust
    match <ref-rom> <rom> --symbols <ref-map>...
                               Print a symbol map for <rom>: each routine named
                               in the reference map whose code (addresses
                               masked) is found once in <rom>'s boot segment
                               or in code saved from its runs' RAM
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
        Some("match") => match_routines(&args[1..]),
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

/// The `missing.txt` rom column of code the game wrote into RAM itself.
const WRITTEN: u32 = u32::MAX;

/// The code in each RAM image a run saved under `images/<ram>_<call>.bin`
/// (see the runner's `record_run`), found from the call that missed, as one
/// module each.
/// Discovery starts from that call; every function start the layout of the
/// image shows is taken too, unless it lands inside one already found. The image may also hold other code loaded nearby:
/// harmless, since each function only runs while its bytes are in RAM, and
/// what decodes as data is dropped.
fn written_images(local: &Path) -> Result<Vec<(String, Discovery)>, String> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(local.join("images")) else {
        return Ok(out);
    };
    let mut paths: Vec<PathBuf> = dir.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let hex: Vec<u32> = stem
            .split('_')
            .filter_map(|h| u32::from_str_radix(h, 16).ok())
            .collect();
        let [base, called] = hex[..] else {
            continue;
        };
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let img = Image {
            base,
            bytes: &bytes,
        };
        let end = base.wrapping_add(bytes.len() as u32);
        // Only the call that saved this image: other misses in the same
        // range may have been into other code loaded there at other times.
        let entries = vec![called];
        let mut layout = function_starts(&img);
        layout.extend(data_pointers(&img));
        out.push((
            format!("img_{stem}"),
            discover_layout(&img, &entries, &[], &layout, Some(end)),
        ));
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
    for m in missing.iter().filter(|m| m[3] != 0 && m[1] != WRITTEN) {
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
        // The boot image read in place is already translated; the same
        // bytes loaded somewhere else are an overlay like any other.
        let boot_in_place = r.wrapping_sub(boot_rom) < BOOT_SEGMENT_SIZE as u32
            && r.wrapping_sub(boot_rom) == ram.wrapping_sub(img.base);
        let seen = dma.iter().any(|d| {
            d[2] > 0
                && r.wrapping_sub(d[0]) < d[2]
                && r.wrapping_sub(d[0]) == ram.wrapping_sub(d[1])
        });
        // The same test copies seen in runs get: laid out like code.
        let end = (r as usize + len as usize).min(rom.bytes().len());
        let code = function_starts(&Image {
            base: ram,
            bytes: &rom.bytes()[r as usize..end],
        });
        if !boot_in_place && !seen && !code.is_empty() && !regions.contains_key(&(r, ram, len)) {
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
        // A scene's entry often sits first in its code and is a leaf with
        // no frame to spot, so the start of the copy is a function too.
        entries.push(ram);
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

    // Code the game wrote into RAM itself, saved by runs that called into it.
    let images = written_images(&local)?;

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
    // Image functions are checked against RAM by their bytes, so one found in
    // several saved images is emitted once.
    let mut image_fns: Vec<(String, u32, u32, u64)> = Vec::new();
    let mut seen_fns = std::collections::HashSet::new();
    for (module, d) in &images {
        let mut file = HEADER.to_string();
        for f in &d.functions {
            let bytes: Vec<u8> = f.words.iter().flat_map(|w| w.to_be_bytes()).collect();
            let hash = n64rusty_runtime::code_hash(&bytes);
            if seen_fns.insert((f.addr, hash)) {
                file.push_str(&translate_one(f));
                file.push('\n');
                image_fns.push((module.clone(), f.addr, bytes.len() as u32, hash));
            }
        }
        files.insert(module.clone(), file);
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
    for (module, addr, len, hash) in &image_fns {
        let _ = writeln!(
            register,
            "    ctx.register_image_fn({addr:#010x}, {len:#x}, {hash:#018x}, {module}::{});",
            emit::fn_name(*addr)
        );
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

/// Finds each reference routine `(addr, name, len in words)` in `target`
/// (the words of a boot segment loaded at `base`), comparing code with
/// [`masked`]. A routine found exactly once is named; so is one that a named
/// routine calls from the same place in both, when its code matches there.
fn find_routines(
    reference: &[u32],
    ref_base: u32,
    routines: &[(u32, String, usize)],
    target: &[u32],
    base: u32,
) -> Vec<(u32, String)> {
    let mref: Vec<u32> = reference.iter().map(|&w| masked(w)).collect();
    let mtgt: Vec<u32> = target.iter().map(|&w| masked(w)).collect();
    let code = |addr: u32, len: usize| {
        let at = ((addr - ref_base) / 4) as usize;
        mref.get(at..at + len)
    };
    let fits = |c: &[u32], t: u32| {
        let i = (t.wrapping_sub(base) / 4) as usize;
        t >= base && mtgt.get(i..i + c.len()) == Some(c)
    };
    let mut index: HashMap<&[u32], Vec<usize>> = HashMap::new();
    for i in 0..mtgt.len().saturating_sub(2) {
        index.entry(&mtgt[i..i + 3]).or_default().push(i);
    }
    let by_addr: HashMap<u32, usize> = routines.iter().enumerate().map(|(k, r)| (r.0, k)).collect();
    let mut found: BTreeMap<usize, u32> = BTreeMap::new();
    for (k, (addr, _, len)) in routines.iter().enumerate() {
        let Some(c) = code(*addr, *len).filter(|_| *len >= 3) else {
            continue;
        };
        let hits: Vec<usize> = index
            .get(&c[..3])
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| starts_function(target, i) && mtgt.get(i..i + len) == Some(c))
            .collect();
        if let [i] = hits[..] {
            found.insert(k, base + 4 * i as u32);
        }
    }
    // Follow calls out of what is named until nothing new turns up.
    let mut work: Vec<usize> = found.keys().copied().collect();
    while let Some(k) = work.pop() {
        let (addr, _, len) = &routines[k];
        let (ra, ta) = (
            ((addr - ref_base) / 4) as usize,
            ((found[&k] - base) / 4) as usize,
        );
        for j in 0..*len {
            let (rw, tw) = (reference[ra + j], target.get(ta + j).copied().unwrap_or(0));
            if rw >> 26 != 3 || tw >> 26 != 3 {
                continue;
            }
            let (rt, tt) = (
                0x8000_0000 | (rw & 0x03FF_FFFF) << 2,
                0x8000_0000 | (tw & 0x03FF_FFFF) << 2,
            );
            let Some(&callee) = by_addr.get(&rt) else {
                continue;
            };
            let (_, _, clen) = &routines[callee];
            if found.contains_key(&callee) || *clen == 0 {
                continue;
            }
            if code(rt, *clen).is_some_and(|c| fits(c, tt)) {
                found.insert(callee, tt);
                work.push(callee);
            }
        }
    }
    // One target address gets one name: drop any found twice.
    let mut count: HashMap<u32, usize> = HashMap::new();
    for a in found.values() {
        *count.entry(*a).or_default() += 1;
    }
    found
        .into_iter()
        .filter(|(_, a)| count[a] == 1)
        .map(|(k, a)| (a, routines[k].1.clone()))
        .collect()
}

fn match_routines(args: &[String]) -> Result<(), String> {
    let mut paths = Vec::new();
    let mut symbols = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--symbols" {
            symbols.push(PathBuf::from(it.next().ok_or("--symbols needs a value")?));
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    let [ref_path, path] = &paths[..] else {
        return Err("match needs a reference ROM and a ROM".into());
    };
    let (reference, rom) = (load(ref_path)?, load(path)?);
    let ref_words = words(reference.boot_segment());
    let (ref_base, ref_end) = (
        reference.entry(),
        reference.entry() + 4 * ref_words.len() as u32,
    );
    let mut syms = Vec::new();
    for s in &symbols {
        syms.extend(read_symbols(s)?);
    }
    syms.retain(|(a, _)| (ref_base..ref_end).contains(a));
    syms.sort();
    // A routine runs to the next named address; padding at its end is not code.
    let mut routines = Vec::new();
    for (i, (a, n)) in syms.iter().enumerate() {
        let next = syms[i + 1..]
            .iter()
            .map(|s| s.0)
            .find(|&b| b > *a)
            .unwrap_or(ref_end);
        let at = ((a - ref_base) / 4) as usize;
        let mut len = ((next - a) / 4) as usize;
        while len > 0 && ref_words[at + len - 1] == 0 {
            len -= 1;
        }
        routines.push((*a, n.clone(), len.min(0x1000)));
    }
    let mut found = find_routines(
        &ref_words,
        ref_base,
        &routines,
        &words(rom.boot_segment()),
        rom.entry(),
    );
    // Also the code runs saved from RAM (`images/`), for library routines
    // the game keeps in code it unpacks itself.
    let images = PathBuf::from("local").join(&rom.sha256).join("images");
    let mut saved: Vec<PathBuf> = fs::read_dir(&images)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    saved.sort();
    for p in saved {
        let stem = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let Some(base) = stem
            .split('_')
            .next()
            .and_then(|h| u32::from_str_radix(h, 16).ok())
        else {
            continue;
        };
        let bytes = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        for (a, n) in find_routines(&ref_words, ref_base, &routines, &words(&bytes), base) {
            if found.iter().all(|(fa, fname)| *fa != a && *fname != n) {
                found.push((a, n));
            }
        }
    }
    found.sort();
    eprintln!(
        "{} of {} reference routines found",
        found.len(),
        routines.len()
    );
    for (a, n) in found {
        println!("{n} = {a:#010X};");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_relocated_routine_once() {
        const B: u32 = 0x8000_0400;
        let jal = |t: u32| 0x0C00_0000 | (t & 0x0FFF_FFFF) >> 2;
        // f: lui a0,X; jal g; addiu a0,a0,X; jr ra | g: lw v0,X(a0); jr ra; nop
        let g = [0x8C82_0010, 0x03E0_0008, 0];
        let mut reference = vec![0x3C04_8001, jal(B + 16), 0x2484_0010, 0x03E0_0008];
        reference.extend(g);
        let routines = [(B, "f".to_string(), 4), (B + 16, "g".to_string(), 2)];
        // Moved and relinked; g's shape is also found once more before f.
        let mut target = vec![0x8C82_0020, 0x03E0_0008, 0, 0];
        target.extend([0x3C04_8002, jal(B + 32), 0x2484_0020, 0x03E0_0008]);
        target.extend([0x8C82_0040, 0x03E0_0008, 0]);
        let r = find_routines(&reference, B, &routines, &target, B);
        assert_eq!(
            r,
            vec![(B + 16, "f".to_string()), (B + 32, "g".to_string())]
        );
        // f found twice: neither is named.
        target.extend([0x3C04_8002, jal(B + 32), 0x2484_0020, 0x03E0_0008]);
        assert!(find_routines(&reference, B, &routines, &target, B).is_empty());
    }

    #[test]
    fn matches_only_at_routine_starts() {
        // A 3-word routine whose shape is also the tail of a longer one.
        let reference = [0x8C82_0010, 0x03E0_0008, 0];
        let routines = [(0x8000_0400, "g".to_string(), 3)];
        let target = [
            0x2484_0001,
            0x8C82_0010,
            0x03E0_0008,
            0,
            0x03E0_0008,
            0,
            0x8C82_0010,
            0x03E0_0008,
            0,
        ];
        let r = find_routines(&reference, 0x8000_0400, &routines, &target, 0x8000_0400);
        assert_eq!(r, vec![(0x8000_0418, "g".to_string())]);
    }

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
