//! Headless host for recompiled N64 code.
//!
//! A generated game crate's `main` hands its function table to [`main`],
//! which loads the player's ROM, sets up RAM the way the console's boot code
//! leaves it, and runs the game's entry point. Execution runs until the
//! first panic, which is reported with the most recent calls, so system
//! routines can be replaced one at a time. Nothing is drawn, no window is
//! opened and no sound is played.

pub mod devices;
mod interp;
mod jpeg;
pub mod live;
pub mod media;
pub mod os;
pub mod save;

/// Mods built in from the `N64RUSTY_MODS` folder (see build.rs); none in a
/// normal build. Each mod file has
/// `pub fn install(ctx: &mut Ctx, rom: &mut Vec<u8>, sha256: &str)`, run
/// once before the game starts: it checks the ROM hash itself, may set
/// overrides on the context (wrapping a game function with
/// [`Ctx::call_original`]) and may change the cartridge bytes the game
/// reads. With any mod built in, RAM has [`MOD_RAM`] more bytes above what
/// the game is told it has, for the mods' own data.
mod mods {
    use n64rusty_runtime::Ctx;
    include!(concat!(env!("OUT_DIR"), "/mods.rs"));
}

/// Bytes of RAM above the game's own for mods' data, when any are built in:
/// the last `MOD_RAM` bytes of `ctx.mem`.
pub const MOD_RAM: usize = 0x40_0000;

use std::collections::BTreeMap;
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use n64rusty_rom::{Cic, Rom};
use n64rusty_runtime::{sx32, Ctx, Io, Memory};

/// What a generated game crate provides.
pub struct Game {
    /// Registers every recompiled function.
    pub register: fn(&mut Ctx),
    /// Address the boot code starts the game at.
    pub entry: u32,
    /// SHA-256 of the ROM the code was generated from.
    pub sha256: &'static str,
}

/// RAM size without the expansion.
pub const RAM_SIZE: usize = 0x40_0000;
/// RAM size with the Expansion Pak (`--expansion`).
pub const RAM_SIZE_EXPANDED: usize = 0x80_0000;

/// Whether this run has the Expansion Pak plugged in.
static EXPANSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Builds a context with RAM laid out as the boot code leaves it.
pub fn boot(rom: &Rom, game: &Game) -> Ctx {
    let size = if EXPANSION.load(std::sync::atomic::Ordering::Relaxed) {
        RAM_SIZE_EXPANDED
    } else {
        RAM_SIZE
    };
    let mut mem = Memory::new(size + if mods::ANY { MOD_RAM } else { 0 });
    let seg = rom.boot_segment();
    let at = (rom.entry() & 0x1FFF_FFFF) as usize;
    let n = seg.len().min(size - at);
    mem.bytes_mut()[at..at + n].copy_from_slice(&seg[..n]);
    // Boot information the boot code stores in low RAM for the system
    // library: TV type (1 = NTSC), ROM base, reset type, boot code id,
    // version and memory size.
    let cic_id = match rom.cic {
        Cic::Cic6101 | Cic::Cic6102 => 2,
        Cic::Cic6103 => 3,
        Cic::Cic6105 => 5,
        Cic::Cic6106 => 6,
        Cic::Unknown(_) => 2,
    };
    mem.write_u32(0x8000_0300, 1);
    mem.write_u32(0x8000_0308, 0xB000_0000);
    mem.write_u32(0x8000_030C, 0);
    mem.write_u32(0x8000_0310, cic_id);
    mem.write_u32(0x8000_0314, 0);
    mem.write_u32(0x8000_0318, size as u32);
    let mut ctx = Ctx::new(mem);
    (game.register)(&mut ctx);
    // A stack at the top of RAM until the game sets its own.
    ctx.r[29] = sx32(0x8000_0000 + size as u32 - 0x10);
    ctx
}

/// Function names for reports, from `name = 0xADDR;` lines in local files.
pub fn read_names(paths: &[PathBuf]) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        for line in text.lines() {
            let line = line.split("//").next().unwrap_or("").trim();
            let Some((name, rest)) = line.split_once('=') else {
                continue;
            };
            let v = rest.trim().trim_end_matches(';').trim();
            if let Some(h) = v.strip_prefix("0x") {
                if let Ok(a) = u32::from_str_radix(h, 16) {
                    out.entry(a).or_insert_with(|| name.trim().to_string());
                }
            }
        }
    }
    out
}

/// The lines of a per-game `config.toml` that keep, from symbol maps, only
/// the routines the runner replaces or looks for: `name = 0xADDR`.
pub fn routine_list(names: &BTreeMap<u32, String>) -> String {
    names
        .iter()
        .filter(|(_, n)| N_AUDIO.contains(&n.as_str()) || os::HOOKS.iter().any(|(h, _)| h == n))
        .map(|(a, n)| format!("{n} = {a:#010x}\n"))
        .collect()
}

/// Routines that tell the alternative audio library is in use.
const N_AUDIO: [&str; 2] = ["n_alSynNew", "n_alAudioFrame"];

fn label(names: &BTreeMap<u32, String>, a: u32) -> String {
    match names.get(&a) {
        Some(n) => format!("{a:#010x} {n}"),
        None => format!("{a:#010x}"),
    }
}

/// Adds this run's cartridge copies to `dma.txt` and, if the run stopped at
/// a call to code that was never translated, a line to `missing.txt`: the
/// address and the cartridge copy that put code there (rom, ram, length;
/// all zero for code in the boot image).
/// Both live in the per-game folder. `n64rusty translate` reads them to find
/// code the game loads at run time.
///
/// Code no copy covers and that is not the boot image's own bytes was written
/// by the game itself (unpacked, for example). Its line has rom `0xffffffff`
/// and the RAM around it is saved to `images/<ram>_<addr>.bin` for translate.
fn record_run(
    local: &std::path::Path,
    dma: &[(u32, u32, u32)],
    outcome: &os::Outcome,
    ram_now: &[u8],
    rom: &[u8],
    entry: u32,
) -> std::io::Result<()> {
    use std::collections::BTreeSet;
    let read = |name: &str| std::fs::read_to_string(local.join(name)).unwrap_or_default();
    let mut lines: BTreeSet<String> = read("dma.txt").lines().map(str::to_string).collect();
    for (rom, ram, len) in dma {
        lines.insert(format!("{rom:#010x} {ram:#010x} {len:#x}"));
    }
    let mut text = String::from("# rom ram length: cartridge copies seen in headless runs\n");
    for l in lines.iter().filter(|l| !l.starts_with('#')) {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(local.join("dma.txt"), text)?;
    let mut new_missing = Vec::new();
    if let os::Outcome::Panic(m) = outcome {
        if let Some(rest) = m.strip_prefix("call to unknown guest function ") {
            let addr_text = rest.split_whitespace().next().unwrap_or("");
            let addr = u32::from_str_radix(addr_text.trim_start_matches("0x"), 16).unwrap_or(0);
            // Which copy put code there: the latest one covering it.
            // The copy must still be what is there: the game may have
            // written other code over it since (unpacking, for example).
            let still_there = |r: u32, ram: u32| {
                let (at, from) = (
                    (addr & 0x1FFF_FFFC) as usize,
                    (r + (addr & !3) - ram) as usize,
                );
                let n = 16.min(ram_now.len().saturating_sub(at));
                rom.get(from..from + n)
                    .is_some_and(|b| b == &ram_now[at..at + n])
            };
            let src = dma
                .iter()
                .rev()
                .find(|(_, ram, len)| addr >= *ram && addr < ram.wrapping_add(*len))
                .filter(|(r, ram, _)| still_there(*r, *ram));
            // No copy covers it: code in the boot image reached only
            // through a pointer, written with a zero copy.
            let mut found = src.copied().unwrap_or((0, 0, 0));
            if src.is_none() && addr != 0 {
                if let Some((lo, hi)) = written_code(ram_now, rom, entry, addr) {
                    let dir = local.join("images");
                    std::fs::create_dir_all(&dir)?;
                    let at = |a: u32| (a & 0x1FFF_FFFF) as usize;
                    std::fs::write(
                        dir.join(format!("{lo:08x}_{addr:08x}.bin")),
                        &ram_now[at(lo)..at(hi)],
                    )?;
                    found = (u32::MAX, lo, hi - lo);
                }
            }
            let (rom, ram, len) = found;
            // A call to address 0 is a null pointer, not code to translate.
            if addr != 0 {
                new_missing.push(format!("{addr:#010x} {rom:#010x} {ram:#010x} {len:#x}"));
            }
        }
    }
    // Code run in the interpreter, saved as it was when first run.
    for (addr, (lo, bytes)) in interp::seen() {
        if bytes.is_empty() {
            continue;
        }
        let dir = local.join("images");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{lo:08x}_{addr:08x}.bin")), &bytes)?;
        let (written, len) = (u32::MAX, bytes.len());
        new_missing.push(format!("{addr:#010x} {written:#010x} {lo:#010x} {len:#x}"));
    }
    if !new_missing.is_empty() {
        let mut missing: BTreeSet<String> =
            read("missing.txt").lines().map(str::to_string).collect();
        missing.extend(new_missing);
        let text: String = missing
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(local.join("missing.txt"), text)?;
    }
    Ok(())
}

/// The extent of code the game wrote into RAM around `addr`, if the bytes
/// there are not the boot image's: out to the nearest 4 KiB of zero bytes on each
/// side (padding between functions, and zeros in its data, are shorter), at most 4 MiB.
fn written_code(ram: &[u8], rom: &[u8], entry: u32, addr: u32) -> Option<(u32, u32)> {
    const GAP: usize = 4096;
    const MAX: usize = 4 << 20;
    let at = (addr & 0x1FFF_FFFF) as usize & !3;
    let boot = (entry & 0x1FFF_FFFF) as usize;
    let here = ram.get(at..at + 64)?;
    let in_boot = at >= boot && rom.get(0x1000 + at - boot..0x1000 + at - boot + 64) == Some(here);
    if in_boot {
        return None;
    }
    let zero_run = |i: usize| {
        ram.get(i..i + GAP)
            .is_some_and(|b| b.iter().all(|&x| x == 0))
    };
    let mut lo = at;
    while lo >= 4 && at - lo < MAX && !zero_run(lo.saturating_sub(GAP)) {
        lo -= 4;
    }
    let mut hi = at;
    while hi < ram.len() && hi - at < MAX && !zero_run(hi) {
        hi += 4;
    }
    let base = addr & !0x1FFF_FFFF;
    Some((base | lo as u32, base | hi as u32))
}

/// Where `--ram-out` saves RAM at the end of the run.
static RAM_OUT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The address range `--interpret` runs in the reference interpreter.
static INTERPRET: Mutex<Option<(u32, u32)>> = Mutex::new(None);

static NAMES: Mutex<BTreeMap<u32, String>> = Mutex::new(BTreeMap::new());
/// The ROM file this run plays, for [`find_rom`].
static ROM_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Another ROM the player owns, by the SHA-256 of its big-endian bytes, in
/// big-endian order: for mods that use a second game's data. Looks in the
/// played ROM's folder, the folders beside it and the current folder; reads
/// only files of a cartridge's size.
pub fn find_rom(sha256: &str) -> Option<Vec<u8>> {
    let played = ROM_PATH.lock().unwrap().clone()?;
    let here = played.parent()?.to_path_buf();
    let mut dirs = vec![here.clone(), PathBuf::from(".")];
    if let Some(Ok(sibs)) = here.parent().map(std::fs::read_dir) {
        dirs.extend(sibs.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    let n64 = |p: &PathBuf| {
        p.extension().is_some_and(|x| {
            ["z64", "n64", "v64"].contains(&x.to_string_lossy().to_lowercase().as_str())
        })
    };
    for d in dirs {
        let Ok(files) = std::fs::read_dir(&d) else {
            continue;
        };
        for p in files.flatten().map(|e| e.path()).filter(n64) {
            let fits = std::fs::metadata(&p).is_ok_and(|m| m.len() <= 0x400_0000);
            let rom = fits.then(|| std::fs::read(&p).ok()).flatten();
            if let Some(Ok(r)) = rom.map(Rom::from_bytes) {
                if r.sha256 == sha256 {
                    return Some(r.bytes().to_vec());
                }
            }
        }
    }
    None
}

/// Text describing the state at a panic: recent calls and registers.
pub fn crash_report(ctx: &Ctx) -> String {
    let names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = format!(
        "after {} calls, depth {}\nrecent calls (oldest first):\n",
        ctx.calls, ctx.depth
    );
    for a in ctx.recent_calls() {
        out.push_str(&format!("  {}\n", label(&names, a)));
    }
    out.push_str("registers:\n");
    for row in 0..8 {
        let regs: Vec<String> = (0..4)
            .map(|c| {
                let n = row * 4 + c;
                format!("r{n:<2}={:016x}", ctx.r[n])
            })
            .collect();
        out.push_str(&format!("  {}\n", regs.join(" ")));
    }
    // N64RUSTY_PEEK=addr:len,... adds memory words, for debugging.
    if let Ok(spec) = std::env::var("N64RUSTY_PEEK") {
        for item in spec.split(',') {
            let Some((a, n)) = item.split_once(':') else {
                continue;
            };
            let hex = |t: &str| u32::from_str_radix(t.trim_start_matches("0x"), 16).ok();
            let (Some(a), Some(n)) = (hex(a), hex(n)) else {
                continue;
            };
            for row in (0..n).step_by(16) {
                let words: Vec<String> = (0..16)
                    .step_by(4)
                    .filter(|o| row + o < n)
                    .map(|o| format!("{:08x}", ctx.mem.read_u32((a + row + o) & !3)))
                    .collect();
                out.push_str(&format!("  {:08x}: {}\n", a + row, words.join(" ")));
            }
        }
    }
    out
}

/// Entry point for a generated game binary:
/// `game <rom> [--symbols file]... [--config file] [--frames n]
/// [--capture dir] [--capture-every n] [--capture-scale n] [--capture-wide] [--capture-between] [--wav file] [--input script]
/// [--save file] [--window] [--run run.toml]`.
///
/// Display lists and audio command lists always go through the graphics
/// and audio interpreters. With `--capture`, every n-th finished frame (60
/// by default) is rendered off screen and saved as a PNG in `dir`
/// (`--capture-scale n` renders at n times the game's resolution,
/// `--capture-wide` 16:9 as the window's widescreen option draws it,
/// `--capture-between` also the picture half-way from the frame before, as
/// a high refresh rate window blends it); with
/// `--wav`, the sound is recorded to a file; `--input` plays a controller
/// script against retrace numbers. Nothing is shown or played.
///
/// `--window` plays the game live instead (see [`live`]): a window, sound,
/// keyboard and gamepads, paced at the game's own rate, with no frame limit
/// unless `--frames` is given. Window, input and pacing settings come from
/// `--run` (default: `run.toml` in the per-game folder, if present).
///
/// Cartridge save memory is kept in `--save file`; window runs default to
/// `save.sra` in the per-game folder. Headless runs without `--save` start
/// from blank save memory and keep nothing, so they stay repeatable.
///
/// System routines are replaced by role name. Their addresses come from the
/// per-game config (`name = 0xADDR` lines, found by us) and from symbol
/// maps read from a local file; neither is ever committed.
pub fn main(game: Game) -> ExitCode {
    main_with(game, None)
}

/// Like [`main`], with the given devices for graphics, audio and input
/// (`None` for the default interpreter and capture).
pub fn main_with(game: Game, devices: Option<Box<dyn devices::Devices>>) -> ExitCode {
    main_args(game, devices, std::env::args().skip(1).collect())
}

/// Like [`main_with`], with the given command-line arguments.
pub fn main_args(
    game: Game,
    devices: Option<Box<dyn devices::Devices>>,
    args: Vec<String>,
) -> ExitCode {
    let mut rom_path = None;
    let mut symbols = Vec::new();
    let mut frames = None;
    let mut window = false;
    let mut run_config = None;
    // The per-game folder: config, and run logs the translator reads.
    let mut local = PathBuf::from("..");
    let mut capture = None;
    let mut every = 60;
    let mut scale = 1;
    let mut wav = None;
    let mut input = None;
    let mut save = None;
    let mut wide = false;
    let mut between = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let flag = a.as_str();
        if !flag.starts_with("--") {
            rom_path = Some(PathBuf::from(a));
            continue;
        }
        if flag == "--window" {
            window = true;
            continue;
        }
        if flag == "--expansion" {
            EXPANSION.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        if flag == "--capture-wide" {
            wide = true;
            continue;
        }
        if flag == "--capture-between" {
            between = true;
            continue;
        }
        let Some(v) = it.next() else {
            eprintln!("error: {flag} needs a value");
            return ExitCode::from(2);
        };
        let number = || v.parse::<u64>().ok();
        match flag {
            "--symbols" | "--config" => symbols.push(PathBuf::from(v)),
            "--local" => local = PathBuf::from(v),
            "--capture" => capture = Some(PathBuf::from(v)),
            "--wav" => wav = Some(PathBuf::from(v)),
            "--input" => input = Some(PathBuf::from(v)),
            "--poke" => {
                match std::fs::read_to_string(v)
                    .map_err(|e| e.to_string())
                    .and_then(|t| os::parse_pokes(&t))
                {
                    Ok(p) => os::set_pokes(p),
                    Err(e) => {
                        eprintln!("error: poke script {v}: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--ram-out" => *RAM_OUT.lock().unwrap() = Some(PathBuf::from(v)),
            "--hash-out" => match std::fs::File::create(v) {
                Ok(f) => os::set_hash_out(f),
                Err(e) => {
                    eprintln!("error: {v}: {e}");
                    return ExitCode::from(2);
                }
            },
            "--interpret" => {
                let range = v.split_once('-').and_then(|(a, b)| {
                    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok();
                    Some((hex(a)?, hex(b)?))
                });
                let Some(range) = range.filter(|(a, b)| a < b) else {
                    eprintln!("error: --interpret needs a hex range like 80000400-80100000");
                    return ExitCode::from(2);
                };
                *INTERPRET.lock().unwrap() = Some(range);
            }
            "--save" => save = Some(PathBuf::from(v)),
            "--run" => run_config = Some(PathBuf::from(v)),
            "--frames" | "--capture-every" | "--capture-scale" => {
                let Some(n) = number() else {
                    eprintln!("error: {flag} needs a number, not {v:?}");
                    return ExitCode::from(2);
                };
                match flag {
                    "--frames" => frames = Some(n),
                    "--capture-every" => every = n,
                    _ => scale = n.min(8) as u32,
                }
            }
            _ => {
                eprintln!("error: unknown option {flag}");
                return ExitCode::from(2);
            }
        }
    }
    if devices.is_some() && (capture.is_some() || wav.is_some() || input.is_some()) {
        eprintln!("warning: --capture, --wav and --input only apply to the default devices");
    }
    let Some(rom_path) = rom_path else {
        eprintln!(
            "usage: <game> <rom> [--symbols file]... [--config file] [--frames n] [--local dir] [--capture dir] [--capture-every n] [--capture-scale n] [--capture-wide] [--capture-between] [--wav file] [--input script] [--poke script] [--ram-out file] [--hash-out file] [--interpret lo-hi] [--expansion] [--save file] [--window] [--run run.toml]"
        );
        return ExitCode::from(2);
    };
    let rom = match std::fs::read(&rom_path)
        .map_err(|e| e.to_string())
        .and_then(|b| Rom::from_bytes(b).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}: {e}", rom_path.display());
            return ExitCode::from(2);
        }
    };
    *ROM_PATH.lock().unwrap() = Some(rom_path.clone());
    if rom.sha256 != game.sha256 {
        eprintln!(
            "error: this build was generated from ROM {}, but {} is {}",
            game.sha256,
            rom_path.display(),
            rom.sha256
        );
        return ExitCode::from(2);
    }
    // The per-game config sits next to the generated crate by default.
    // The run config only matters to window runs.
    let run_config = run_config
        .or_else(|| Some(local.join("run.toml")).filter(|p| p.exists()))
        .filter(|_| window);
    let mut run_config = match run_config.map(|p| n64rusty_host::RunConfig::load(&p)) {
        Some(Err(e)) => {
            eprintln!("error: run config {e}");
            return ExitCode::from(2);
        }
        Some(Ok(c)) => c,
        None => n64rusty_host::RunConfig::default(),
    };
    // The game, not the window, ends a run at its frame limit, so the run
    // report is still printed.
    let frames = frames
        .or(run_config.frames.take())
        .unwrap_or(if window { u64::MAX } else { 600 });
    let default_config = local.join("config.toml");
    if default_config.exists() {
        symbols.push(default_config);
    }
    let save = match save.or_else(|| window.then(|| local.join("save.sra"))) {
        Some(p) => match save::Save::open(&p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: save file {}: {e}", p.display());
                return ExitCode::from(2);
            }
        },
        None => save::Save::in_memory(),
    };
    let names = read_names(&symbols);
    let by_name: BTreeMap<String, u32> = names.iter().map(|(a, n)| (n.clone(), *a)).collect();

    let mut ctx = boot(&rom, &game);
    let mut cart = rom.bytes().to_vec();
    mods::install(&mut ctx, &mut cart, game.sha256);
    let rom_bytes: Arc<[u8]> = Arc::from(cart);
    ctx.mem.set_io(Io {
        sp_mem: vec![0; 0x2000],
        rom: rom_bytes.clone(),
        regs: Default::default(),
        flash: Default::default(),
    });
    let hooked = os::install(&mut ctx, names.iter().map(|(a, n)| (n, a)));
    println!("replaced {} system routines", hooked.len());
    *NAMES.lock().unwrap() = names;
    let (snap_rom, snap_entry) = (rom_bytes.clone(), game.entry);
    ctx.interpret = *INTERPRET.lock().unwrap();
    interp::install(
        &mut ctx,
        Box::new(move |ram, addr| written_code(ram, &snap_rom, snap_entry, addr)),
    );

    panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!(" ({}:{})", l.file(), l.line()))
            .unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        *os::PANIC_MESSAGE.lock().unwrap_or_else(|e| e.into_inner()) =
            format!("{msg}{loc} [{thread}]");
    }));
    // The context lives in a box that is never freed: guest threads keep
    // using it until the process exits.
    let ctx: &'static mut Ctx = Box::leak(Box::new(ctx));
    let script = match input.as_ref().map(|p| {
        std::fs::read_to_string(p)
            .map_err(|e| e.to_string())
            .and_then(|t| n64rusty_host::script::parse(&t))
            .map_err(|e| format!("{}: {e}", p.display()))
    }) {
        Some(Err(e)) => {
            eprintln!("error: input script {e}");
            return ExitCode::from(2);
        }
        Some(Ok(events)) => Some(events),
        None => None,
    };
    // The alternative audio library has its own command set; tell them
    // apart by the library's routine names in the local symbol map.
    let n_audio = N_AUDIO.iter().any(|n| by_name.contains_key(*n));
    let media = media::Media::new(capture, every, wav, n_audio, script)
        .with_scale(scale)
        .with_widescreen(wide)
        .with_between(between);
    let entry = game.entry;
    if window {
        return live::play(&run_config, media, move |devices| {
            run_and_report(ctx, entry, rom_bytes, frames, devices, save, &local)
        });
    }
    let devices = devices.unwrap_or_else(|| Box::new(media));
    ExitCode::from(run_and_report(
        ctx, entry, rom_bytes, frames, devices, save, &local,
    ))
}

/// Runs the game to its end and prints the report; returns the exit code.
fn run_and_report(
    ctx: &'static mut Ctx,
    entry: u32,
    rom_bytes: Arc<[u8]>,
    frames: u64,
    devices: Box<dyn devices::Devices>,
    save: save::Save,
    local: &std::path::Path,
) -> u8 {
    let rom = rom_bytes.clone();
    let (outcome, report, frames_run) = os::run(ctx, entry, rom_bytes, frames, devices, save);
    let (_, dma, threads, device_summary) = os::summary();
    if let Err(e) = record_run(local, &dma, &outcome, ctx.mem.bytes(), &rom, entry) {
        eprintln!(
            "warning: could not write the run log in {}: {e}",
            local.display()
        );
    }
    println!("frames: {frames_run}");
    println!("cartridge DMA requests: {}", dma.len());
    for (rom_off, ram, len) in dma.iter().take(20) {
        println!("  rom {rom_off:#09x} -> {ram:#010x} len {len:#x}");
    }
    println!("threads:\n{threads}");
    // The thread that was on the CPU last keeps its registers in `ctx`.
    println!(
        "  last on the CPU: call at {:#010x}",
        (ctx.r[31] as u32).wrapping_sub(8)
    );
    print!("{device_summary}");
    if let Some(p) = RAM_OUT.lock().unwrap().as_ref() {
        if let Err(e) = std::fs::write(p, ctx.mem.bytes()) {
            eprintln!("warning: could not write RAM to {}: {e}", p.display());
        }
    }
    match outcome {
        os::Outcome::Frames(n) => {
            println!("ran {n} frames without a panic");
            0
        }
        os::Outcome::Returned => {
            println!("the boot code returned");
            0
        }
        os::Outcome::Deadlock(d) => {
            println!("deadlock: every thread waits and nothing can wake one\n{d}");
            1
        }
        os::Outcome::Panic(m) => {
            println!("panic: {m}\n{report}");
            1
        }
        os::Outcome::Hang(d) => {
            println!(
                "hang: no retrace for {} s of host time, so a guest loop makes no calls\n{d}",
                os::HANG_WALL.as_secs()
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_list_keeps_replaced_and_audio_routines() {
        let names: BTreeMap<u32, String> = [
            (0x8000_1000, "osCreateThread"),
            (0x8000_2000, "some_game_function"),
            (0x8000_3000, "n_alSynNew"),
        ]
        .into_iter()
        .map(|(a, n)| (a, n.to_string()))
        .collect();
        let list = routine_list(&names);
        assert_eq!(
            list,
            "osCreateThread = 0x80001000\nn_alSynNew = 0x80003000\n"
        );
        let path = std::env::temp_dir().join(format!("routines-{}.toml", std::process::id()));
        std::fs::write(&path, &list).unwrap();
        let back = read_names(std::slice::from_ref(&path));
        std::fs::remove_file(path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[&0x8000_1000], "osCreateThread");
    }
}
