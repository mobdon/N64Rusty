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

pub mod mods;

/// Bytes of RAM above the game's own for mods' data, when a mod is on.
pub const MOD_RAM: usize = 0x40_0000;
/// Whether this run has mods on (and so [`MOD_RAM`]).
static WITH_MODS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

use std::collections::BTreeMap;
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use n64rusty_rom::{Cic, Rom, BOOT_SEGMENT_ROM};
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

/// The physical address bits of a KSEG0/KSEG1 address.
pub(crate) const PHYS_MASK: u32 = 0x1FFF_FFFF;
/// Bytes of RSP memory (DMEM and IMEM).
const SP_MEM_SIZE: usize = 0x2000;
/// The largest cartridge ROM: 64 MiB.
const MAX_ROM_SIZE: u64 = 0x400_0000;
/// File extensions of ROM files (big-endian, little-endian, byte-swapped).
pub const ROM_EXTENSIONS: [&str; 3] = ["z64", "n64", "v64"];

/// Where the boot code leaves its boot information in low RAM for the
/// system library (`osTvType`, `osRomBase`, `osResetType`, `osCicId`,
/// `osVersion`, `osMemSize`).
mod boot_info {
    /// TV type: 1 is NTSC.
    pub const TV_TYPE: u32 = 0x8000_0300;
    /// The cartridge's PI base address.
    pub const ROM_BASE: u32 = 0x8000_0308;
    /// Reset type: 0 is a cold reset.
    pub const RESET_TYPE: u32 = 0x8000_030C;
    /// The boot code's (CIC) id.
    pub const CIC_ID: u32 = 0x8000_0310;
    /// Library version.
    pub const VERSION: u32 = 0x8000_0314;
    /// RAM size in bytes.
    pub const MEM_SIZE: u32 = 0x8000_0318;
}

/// A hex number with an optional `0x` prefix, as flags, configs and run
/// logs write addresses. (Any number of leading `0x` are taken off, as the
/// copies this replaces did.)
pub(crate) fn parse_hex(s: &str) -> Result<u32, std::num::ParseIntError> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16)
}

/// Whether this run has the Expansion Pak plugged in.
static EXPANSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The game's own RAM size this run, without the mods' RAM above it.
pub(crate) fn ram_size() -> usize {
    if EXPANSION.load(std::sync::atomic::Ordering::Relaxed) {
        RAM_SIZE_EXPANDED
    } else {
        RAM_SIZE
    }
}

/// Builds a context with RAM laid out as the boot code leaves it.
///
/// Fails when the ROM header's entry point lies outside RAM, since the boot
/// segment could not be copied there.
pub fn boot(rom: &Rom, game: &Game) -> Result<Ctx, String> {
    let size = ram_size();
    let mut mem = Memory::new(
        size + if WITH_MODS.load(std::sync::atomic::Ordering::Relaxed) {
            MOD_RAM
        } else {
            0
        },
    );
    let seg = rom.boot_segment();
    let at = (rom.entry() & PHYS_MASK) as usize;
    if at >= size {
        return Err(format!(
            "ROM entry point {:#010x} is outside RAM ({size:#x} bytes)",
            rom.entry()
        ));
    }
    let n = seg.len().min(size - at);
    mem.bytes_mut()[at..at + n].copy_from_slice(&seg[..n]);
    // Boot information the boot code stores in low RAM for the system
    // library: TV type (1 = NTSC), ROM base, reset type, boot code id,
    // version and memory size.
    let cic_id = match rom.cic {
        Cic::Cic6101 | Cic::Cic6102 => 2,
        Cic::Cic6103 => 3,
        // The number itself: a game compares it with 6105 before it starts
        // its main thread. shortcut: the other codes keep short ids nothing
        // has read yet; give them their numbers too if a game checks one.
        Cic::Cic6105 => 6105,
        Cic::Cic6106 => 6,
        Cic::Unknown(_) => 2,
    };
    mem.write_u32(boot_info::TV_TYPE, 1);
    mem.write_u32(boot_info::ROM_BASE, 0xB000_0000);
    mem.write_u32(boot_info::RESET_TYPE, 0);
    mem.write_u32(boot_info::CIC_ID, cic_id);
    mem.write_u32(boot_info::VERSION, 0);
    mem.write_u32(boot_info::MEM_SIZE, size as u32);
    let mut ctx = Ctx::new(mem);
    (game.register)(&mut ctx);
    // A stack at the top of RAM until the game sets its own.
    ctx.r[29] = sx32(0x8000_0000 + size as u32 - 0x10);
    Ok(ctx)
}

/// Function names for reports, from `name = 0xADDR;` lines in local files.
///
/// Fails with the file's path when one of `paths` cannot be read, so a
/// mistyped map is reported instead of quietly giving fewer names.
pub fn read_names(paths: &[PathBuf]) -> Result<BTreeMap<u32, String>, String> {
    let mut out = BTreeMap::new();
    for p in paths {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
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
    Ok(out)
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

/// Per-game config name: forget a drawn picture once it has not been drawn
/// into for this many frames (`readback_frames = 0x3`).
const READBACK_FRAMES: &str = "readback_frames";

/// Per-game config name: the `n_audio` command set with that game's stream
/// commands (the last step of its MP3 decoding).
const N_AUDIO_STREAMS: &str = "n_audio_streams";

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
            let addr = parse_hex(addr_text).unwrap_or(0);
            // A call to address 0 is a null pointer, not code to translate.
            if addr != 0 {
                let (rom, ram, len) = missing_source(local, dma, ram_now, rom, entry, addr)?;
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

/// Where the code at `addr`, called but never translated, came from:
/// (rom, ram, length) of the cartridge copy that put it there, all zero for
/// the boot image, or rom `0xffffffff` for code the game wrote itself (its
/// RAM is then saved to `images/` in `local`).
fn missing_source(
    local: &std::path::Path,
    dma: &[(u32, u32, u32)],
    ram_now: &[u8],
    rom: &[u8],
    entry: u32,
    addr: u32,
) -> std::io::Result<(u32, u32, u32)> {
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
    if let Some(&found) = src {
        return Ok(found);
    }
    if let Some((lo, hi)) = written_code(ram_now, rom, entry, addr) {
        let dir = local.join("images");
        std::fs::create_dir_all(&dir)?;
        let at = |a: u32| (a & PHYS_MASK) as usize;
        std::fs::write(
            dir.join(format!("{lo:08x}_{addr:08x}.bin")),
            &ram_now[at(lo)..at(hi)],
        )?;
        return Ok((u32::MAX, lo, hi - lo));
    }
    // No copy covers it: code in the boot image reached only
    // through a pointer, written with a zero copy.
    Ok((0, 0, 0))
}

/// See `N64RUSTY_PAGES_OUT` where it is read: `spec` is `file:lo-hi`.
/// Pages no TLB entry maps after a refill are saved as zeros.
fn save_pages(ctx: &mut Ctx, spec: &str) -> Result<(), String> {
    let (file, range) = spec.rsplit_once(':').ok_or("needs file:lo-hi")?;
    let (lo, hi) = range.split_once('-').ok_or("needs file:lo-hi")?;
    let hex = |s: &str| parse_hex(s).map_err(|e| e.to_string());
    let (lo, hi) = (hex(lo)? & !0xFFF, hex(hi)?);
    let mut out = Vec::new();
    let mut missing = 0;
    for page in (lo..hi).step_by(0x1000) {
        ctx.tlb_miss(page);
        if ctx.unmapped(page) {
            missing += 1;
            out.extend_from_slice(&[0; 0x1000]);
            continue;
        }
        for k in (0..0x1000).step_by(4) {
            out.extend_from_slice(&ctx.mem.read_u32(page + k).to_be_bytes());
        }
    }
    std::fs::write(file, &out).map_err(|e| e.to_string())?;
    println!(
        "saved {:#x} bytes of TLB-mapped memory from {lo:#010x} to {file} ({missing} pages not mapped)",
        out.len()
    );
    Ok(())
}

/// The extent of code the game wrote into RAM around `addr`, if the bytes
/// there are not the boot image's: out to the nearest 4 KiB of zero bytes on each
/// side (padding between functions, and zeros in its data, are shorter), at most 4 MiB.
fn written_code(ram: &[u8], rom: &[u8], entry: u32, addr: u32) -> Option<(u32, u32)> {
    const GAP: usize = 4096;
    const MAX: usize = 4 << 20;
    let at = (addr & PHYS_MASK) as usize & !3;
    let boot = (entry & PHYS_MASK) as usize;
    let here = ram.get(at..at + 64)?;
    let in_boot = at >= boot && {
        let from = BOOT_SEGMENT_ROM + at - boot;
        rom.get(from..from + 64) == Some(here)
    };
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
    let base = addr & !PHYS_MASK;
    Some((base | lo as u32, base | hi as u32))
}

static NAMES: Mutex<BTreeMap<u32, String>> = Mutex::new(BTreeMap::new());
/// The ROM file this run plays, for [`find_rom`].
static ROM_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Whether this run plays in a window, which ends the game when a mod asks
/// for another one; a headless run stops itself.
static WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Another ROM the player owns, by the SHA-256 of its big-endian bytes, in
/// big-endian order: for mods that use a second game's data. Looks in the
/// played ROM's folder, the folders beside it and the current folder; reads
/// only files of a cartridge's size.
pub fn find_rom(sha256: &str) -> Option<Vec<u8>> {
    find_rom_path(sha256).map(|(_, rom)| rom)
}

/// [`find_rom`] with the file the ROM is in.
pub fn find_rom_path(sha256: &str) -> Option<(PathBuf, Vec<u8>)> {
    let played = ROM_PATH.lock().unwrap().clone()?;
    let here = played.parent()?.to_path_buf();
    let mut dirs = vec![here.clone(), PathBuf::from(".")];
    if let Some(Ok(sibs)) = here.parent().map(std::fs::read_dir) {
        dirs.extend(sibs.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    let n64 = |p: &PathBuf| {
        p.extension()
            .is_some_and(|x| ROM_EXTENSIONS.contains(&x.to_string_lossy().to_lowercase().as_str()))
    };
    for d in dirs {
        let Ok(files) = std::fs::read_dir(&d) else {
            continue;
        };
        for p in files.flatten().map(|e| e.path()).filter(n64) {
            let fits = std::fs::metadata(&p).is_ok_and(|m| m.len() <= MAX_ROM_SIZE);
            let rom = fits.then(|| std::fs::read(&p).ok()).flatten();
            if let Some(Ok(r)) = rom.map(Rom::from_bytes) {
                if r.sha256 == sha256 {
                    return Some((p, r.bytes().to_vec()));
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
    let pc = interp::LAST_PC.load(std::sync::atomic::Ordering::Relaxed);
    if pc != 0 {
        out.push_str(&format!(
            "interpreter last outside plain RAM at {pc:#010x}\n"
        ));
    }
    if ctx.tlb_refill != 0 {
        out.push_str(&format!(
            "last TLB miss at {:#010x}, interpreter then at {:#010x}\n",
            ctx.cop0[8],
            interp::MISS_PC.load(std::sync::atomic::Ordering::Relaxed)
        ));
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
            let (Ok(a), Ok(n)) = (parse_hex(a), parse_hex(n)) else {
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

/// The command-line flags of one run (see [`main`]).
struct Args {
    rom_path: Option<PathBuf>,
    symbols: Vec<PathBuf>,
    frames: Option<u64>,
    window: bool,
    run_config: Option<PathBuf>,
    /// The per-game folder: config, and run logs the translator reads.
    local: PathBuf,
    capture: Option<PathBuf>,
    every: u64,
    scale: u32,
    wav: Option<PathBuf>,
    input: Option<PathBuf>,
    save: Option<PathBuf>,
    wide: bool,
    between: bool,
    /// Where `--ram-out` saves RAM at the end of the run.
    ram_out: Option<PathBuf>,
    /// The address range `--interpret` runs in the reference interpreter.
    interpret: Option<(u32, u32)>,
}

/// Reads the flags in `args`. Flags that set a run-wide setting (`--poke`,
/// `--hash-out`, `--expansion`) set it here. Fails, after printing why,
/// with the exit code for a bad command line.
fn parse_args(args: &[String]) -> Result<Args, ExitCode> {
    let mut a = Args {
        rom_path: None,
        symbols: Vec::new(),
        frames: None,
        window: false,
        run_config: None,
        local: PathBuf::from(".."),
        capture: None,
        every: 60,
        scale: 1,
        wav: None,
        input: None,
        save: None,
        wide: false,
        between: false,
        ram_out: None,
        interpret: None,
    };
    // A player plays one game after another in the same program: nothing a
    // flag set for the last game carries over.
    os::set_pokes(Vec::new());
    EXPANSION.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        if !flag.starts_with("--") {
            a.rom_path = Some(PathBuf::from(arg));
            continue;
        }
        if flag == "--window" {
            a.window = true;
            continue;
        }
        if flag == "--expansion" {
            EXPANSION.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        if flag == "--capture-wide" {
            a.wide = true;
            continue;
        }
        if flag == "--capture-between" {
            a.between = true;
            continue;
        }
        let Some(v) = it.next() else {
            eprintln!("error: {flag} needs a value");
            return Err(ExitCode::from(2));
        };
        let number = || v.parse::<u64>().ok();
        match flag {
            "--symbols" | "--config" => a.symbols.push(PathBuf::from(v)),
            "--local" => a.local = PathBuf::from(v),
            "--capture" => a.capture = Some(PathBuf::from(v)),
            "--wav" => a.wav = Some(PathBuf::from(v)),
            "--input" => a.input = Some(PathBuf::from(v)),
            "--poke" => {
                match std::fs::read_to_string(v)
                    .map_err(|e| e.to_string())
                    .and_then(|t| os::parse_pokes(&t))
                {
                    Ok(p) => os::set_pokes(p),
                    Err(e) => {
                        eprintln!("error: poke script {v}: {e}");
                        return Err(ExitCode::from(2));
                    }
                }
            }
            "--ram-out" => a.ram_out = Some(PathBuf::from(v)),
            "--hash-out" => match std::fs::File::create(v) {
                Ok(f) => os::set_hash_out(f),
                Err(e) => {
                    eprintln!("error: {v}: {e}");
                    return Err(ExitCode::from(2));
                }
            },
            "--interpret" => {
                let range = v.split_once('-').and_then(|(lo, hi)| {
                    let hex = |s: &str| parse_hex(s).ok();
                    Some((hex(lo)?, hex(hi)?))
                });
                let Some(range) = range.filter(|(lo, hi)| lo < hi) else {
                    eprintln!("error: --interpret needs a hex range like 80000400-80100000");
                    return Err(ExitCode::from(2));
                };
                a.interpret = Some(range);
            }
            "--save" => a.save = Some(PathBuf::from(v)),
            "--run" => a.run_config = Some(PathBuf::from(v)),
            "--frames" | "--capture-every" | "--capture-scale" => {
                let Some(n) = number() else {
                    eprintln!("error: {flag} needs a number, not {v:?}");
                    return Err(ExitCode::from(2));
                };
                match flag {
                    "--frames" => a.frames = Some(n),
                    "--capture-every" => a.every = n,
                    _ => a.scale = n.min(8) as u32,
                }
            }
            _ => {
                eprintln!("error: unknown option {flag}");
                return Err(ExitCode::from(2));
            }
        }
    }
    Ok(a)
}

/// Like [`main_with`], with the given command-line arguments.
pub fn main_args(
    game: Game,
    devices: Option<Box<dyn devices::Devices>>,
    args: Vec<String>,
) -> ExitCode {
    let Args {
        rom_path,
        mut symbols,
        frames,
        window,
        run_config,
        local,
        capture,
        every,
        scale,
        wav,
        input,
        save,
        wide,
        between,
        ram_out,
        interpret,
    } = match parse_args(&args) {
        Ok(a) => a,
        Err(code) => return code,
    };
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
    // Headless runs take only its widescreen HUD areas, for captures.
    let run_config = run_config.or_else(|| Some(local.join("run.toml")).filter(|p| p.exists()));
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
        .or(run_config.frames.take().filter(|_| window))
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
    let names = match read_names(&symbols) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: cannot read {e}");
            return ExitCode::from(2);
        }
    };
    let by_name: BTreeMap<String, u32> = names.iter().map(|(a, n)| (n.clone(), *a)).collect();
    let mut save = save;
    // A game with a cartridge EEPROM says so in its config, with the size
    // in bytes: `eeprom = 0x200` (4 Kbit) or `0x800` (16 Kbit).
    if let Some(&size) = by_name.get("eeprom") {
        if let Err(e) = save.set_eeprom(size as usize) {
            eprintln!("error: config: {e}");
            return ExitCode::from(2);
        }
    }

    let libs = mods::load();
    WITH_MODS.store(!libs.is_empty(), std::sync::atomic::Ordering::Relaxed);
    let mut ctx = match boot(&rom, &game) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}: {e}", rom_path.display());
            return ExitCode::from(2);
        }
    };
    let mut cart = rom.bytes().to_vec();
    // Only the copy the mods may patch is kept for the run.
    drop(rom);
    mods::install(libs, &mut ctx, &mut cart, game.sha256);
    let rom_bytes: Arc<[u8]> = Arc::from(cart);
    ctx.mem.set_io(Io {
        sp_mem: vec![0; SP_MEM_SIZE],
        rom: rom_bytes.clone(),
        regs: Default::default(),
        flash: Default::default(),
    });
    let hooked = os::install(&mut ctx, names.iter().map(|(a, n)| (n, a)));
    mods::route(&mut ctx);
    println!("replaced {} system routines", hooked.len());
    *NAMES.lock().unwrap() = names;
    let (snap_rom, snap_entry) = (rom_bytes.clone(), game.entry);
    ctx.interpret = interpret;
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
    // apart by the library's routine names in the local symbol map. A game
    // whose microcode extends it with stream commands says so in its own
    // config (`n_audio_streams = 0x1`).
    let streams = by_name.contains_key(N_AUDIO_STREAMS);
    let n_audio = streams || N_AUDIO.iter().any(|n| by_name.contains_key(*n));
    let media = media::Media::new(capture, every, wav, n_audio, script)
        .with_audio_streams(streams)
        .with_readback_frames(by_name.get(READBACK_FRAMES).copied())
        .with_scale(scale)
        .with_widescreen(wide)
        .with_between(between)
        .with_hud(run_config.window.hud.clone(), run_config.window.hud_when);
    let entry = game.entry;
    WINDOW.store(
        window && !run_config.headless,
        std::sync::atomic::Ordering::Relaxed,
    );
    if window {
        return live::play(&run_config, media, move |devices| {
            run_and_report(
                ctx, entry, rom_bytes, frames, devices, save, &local, ram_out,
            )
        });
    }
    let devices = devices.unwrap_or_else(|| Box::new(media));
    ExitCode::from(run_and_report(
        ctx, entry, rom_bytes, frames, devices, save, &local, ram_out,
    ))
}

/// Runs the game to its end and prints the report; returns the exit code.
#[allow(clippy::too_many_arguments)]
fn run_and_report(
    ctx: &'static mut Ctx,
    entry: u32,
    rom_bytes: Arc<[u8]>,
    frames: u64,
    devices: Box<dyn devices::Devices>,
    save: save::Save,
    local: &std::path::Path,
    ram_out: Option<PathBuf>,
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
    if let Some(p) = &ram_out {
        if let Err(e) = std::fs::write(p, ctx.mem.bytes()) {
            eprintln!("warning: could not write RAM to {}: {e}", p.display());
        }
    }
    // N64RUSTY_PAGES_OUT=file:lo-hi (hex) saves the game's TLB-mapped
    // memory from lo to hi, a debugging aid: each 4 KiB page is brought in
    // by the game's own refill routine and copied before the next one may
    // replace it, so code a game unpacks only on demand can be read whole.
    if let Ok(spec) = std::env::var("N64RUSTY_PAGES_OUT") {
        if let Err(e) = save_pages(ctx, &spec) {
            eprintln!("warning: N64RUSTY_PAGES_OUT={spec}: {e}");
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
        os::Outcome::Stopped => {
            println!("stopped for another game");
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
        let back = read_names(std::slice::from_ref(&path)).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[&0x8000_1000], "osCreateThread");
    }

    #[test]
    fn read_names_names_a_file_it_cannot_read() {
        let path = std::env::temp_dir().join(format!("no-such-map-{}.txt", std::process::id()));
        let e = read_names(std::slice::from_ref(&path)).unwrap_err();
        assert!(e.contains(&path.display().to_string()), "{e}");
    }

    /// A big-endian test ROM whose header starts the game at `entry`.
    fn rom_with_entry(entry: u32) -> Rom {
        const TEST_ROM_SIZE: usize = 0x2000;
        const ENTRY_WORD: usize = 0x08;
        let mut b = vec![0u8; TEST_ROM_SIZE];
        b[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        b[ENTRY_WORD..ENTRY_WORD + 4].copy_from_slice(&entry.to_be_bytes());
        b[n64rusty_rom::BOOT_SEGMENT_ROM] = 0xAB;
        Rom::from_bytes(b).unwrap()
    }

    const TEST_GAME: Game = Game {
        register: |_| {},
        entry: 0,
        sha256: "",
    };

    #[test]
    fn boot_copies_the_boot_segment_to_the_entry_point() {
        let ctx = boot(&rom_with_entry(0x8000_0400), &TEST_GAME).unwrap();
        assert_eq!(ctx.mem.read_u8(0x8000_0400), 0xAB);
    }

    #[test]
    fn boot_rejects_an_entry_point_outside_ram() {
        let e = boot(&rom_with_entry(0x8FFF_0000), &TEST_GAME)
            .err()
            .expect("an entry past RAM fails");
        assert!(e.contains("0x8fff0000"), "{e}");
    }

    /// Where the test ROM's header starts the game, in KSEG0.
    const TEST_ENTRY: u32 = 0x8000_0400;
    /// Bytes in a small test ROM: header and boot code, then 4 KiB of boot
    /// segment.
    const TEST_ROM_LEN: usize = 0x2000;

    /// A big-endian test ROM of `len` bytes that starts the game at
    /// `entry`, with every boot segment byte set by `fill` from its index.
    fn rom_filled(entry: u32, len: usize, fill: impl Fn(usize) -> u8) -> Rom {
        let mut b = vec![0u8; len];
        b[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        b[8..12].copy_from_slice(&entry.to_be_bytes());
        for (i, x) in b[n64rusty_rom::BOOT_SEGMENT_ROM..].iter_mut().enumerate() {
            *x = fill(i);
        }
        Rom::from_bytes(b).unwrap()
    }

    #[test]
    fn boot_copies_every_byte_of_the_boot_segment() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |i| (i as u8) | 1);
        let ctx = boot(&rom, &TEST_GAME).unwrap();
        let seg = rom.boot_segment();
        let last = seg.len() as u32 - 1;
        assert_eq!(ctx.mem.read_u8(TEST_ENTRY), seg[0]);
        assert_eq!(ctx.mem.read_u8(TEST_ENTRY + last), seg[last as usize]);
        assert_eq!(ctx.mem.read_u8(TEST_ENTRY + last + 1), 0, "nothing past it");
    }

    #[test]
    fn boot_loads_a_kseg1_entry_point_at_the_same_physical_address() {
        const KSEG1_ENTRY: u32 = 0xA000_0400;
        let rom = rom_filled(KSEG1_ENTRY, TEST_ROM_LEN, |_| 0x5A);
        let ctx = boot(&rom, &TEST_GAME).unwrap();
        assert_eq!(ctx.mem.read_u8(TEST_ENTRY), 0x5A);
    }

    #[test]
    fn boot_clips_a_boot_segment_that_runs_past_the_top_of_ram() {
        const NEAR_TOP: u32 = 0x8000_0000 + RAM_SIZE as u32 - 0x100;
        let rom = rom_filled(NEAR_TOP, TEST_ROM_LEN, |i| (i as u8) | 1);
        let ctx = boot(&rom, &TEST_GAME).expect("an entry inside RAM boots");
        assert_eq!(ctx.mem.read_u8(NEAR_TOP + 0xFF), 0xFF, "last byte of RAM");
    }

    #[test]
    fn boot_stores_the_boot_information_libultra_reads() {
        // libultra's osTvType, osRomBase, osResetType, osVersion and
        // osMemSize live at these low-RAM addresses (os.h, boot.s).
        const OS_TV_TYPE: u32 = 0x8000_0300;
        const OS_ROM_BASE: u32 = 0x8000_0308;
        const OS_RESET_TYPE: u32 = 0x8000_030C;
        const OS_VERSION: u32 = 0x8000_0314;
        const OS_MEM_SIZE: u32 = 0x8000_0318;
        const TV_TYPE_NTSC: u32 = 1;
        const ROM_BASE_KSEG1: u32 = 0xB000_0000;
        const RESET_COLD: u32 = 0;
        let ctx = boot(&rom_with_entry(TEST_ENTRY), &TEST_GAME).unwrap();
        assert_eq!(ctx.mem.read_u32(OS_TV_TYPE), TV_TYPE_NTSC);
        assert_eq!(ctx.mem.read_u32(OS_ROM_BASE), ROM_BASE_KSEG1);
        assert_eq!(ctx.mem.read_u32(OS_RESET_TYPE), RESET_COLD);
        assert_eq!(ctx.mem.read_u32(OS_VERSION), 0);
        assert_eq!(ctx.mem.read_u32(OS_MEM_SIZE), RAM_SIZE as u32);
    }

    #[test]
    fn boot_starts_the_stack_16_bytes_below_the_top_of_ram() {
        // The HLE runner's own convention (boot: "A stack at the top of RAM until
        // the game sets its own"); real IPL3 enters with sp = 0xA4001FF0 in SP IMEM.
        const STACK_TOP: u32 = 0x8000_0000 + RAM_SIZE as u32 - 0x10;
        let ctx = boot(&rom_with_entry(TEST_ENTRY), &TEST_GAME).unwrap();
        assert_eq!(
            ctx.r[29],
            sx32(STACK_TOP),
            "sign-extended, as a 32-bit address is"
        );
    }

    #[test]
    fn boot_registers_the_games_functions() {
        const FN_ADDR: u32 = 0x8000_1000;
        let game = Game {
            register: |c| c.register(FN_ADDR, |_| {}),
            ..TEST_GAME
        };
        let ctx = boot(&rom_with_entry(TEST_ENTRY), &game).unwrap();
        assert!(ctx.has_function(FN_ADDR));
    }

    /// Bytes of zeros that end a stretch of written code.
    const ZERO_GAP: usize = 4096;
    /// The most written code [`written_code`] reports on each side.
    const WRITTEN_MAX: usize = 4 << 20;

    /// RAM of `len` zero bytes with `code` written at physical `at`.
    fn ram_with_code(len: usize, at: usize, code: &[u8]) -> Vec<u8> {
        let mut ram = vec![0u8; len];
        ram[at..at + code.len()].copy_from_slice(code);
        ram
    }

    #[test]
    fn written_code_is_none_for_the_boot_images_own_bytes() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |i| (i as u8) | 1);
        let seg = rom.boot_segment();
        let ram = ram_with_code(0x10_000, 0x400, seg);
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_0500),
            None
        );
    }

    #[test]
    fn written_code_reaches_out_to_the_zero_runs_on_both_sides() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let ram = ram_with_code(0x10_000, 0x6000, &[0xAA; 0x100]);
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_6040),
            Some((0x8000_6000, 0x8000_6100))
        );
    }

    #[test]
    fn written_code_differs_from_the_boot_image_even_inside_its_range() {
        // Code the game unpacked over its own boot image is the game's.
        // A boot segment from 0x400 to 0xF400, with other code at 0x6000.
        let rom = rom_filled(TEST_ENTRY, 0x10_000, |_| 1);
        let ram = ram_with_code(0x10_000, 0x6000, &[0xAA; 0x100]);
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_6000),
            Some((0x8000_6000, 0x8000_6100))
        );
    }

    #[test]
    fn written_code_spans_zero_gaps_shorter_than_4_kib() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let mut ram = ram_with_code(0x10_000, 0x6000, &[0xAA; 0x100]);
        // Padding between two functions, one word short of a gap.
        let second = 0x6100 + ZERO_GAP - 4;
        ram[second..second + 0x100].fill(0xBB);
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_6000),
            Some((0x8000_6000, 0x8000_0000 + second as u32 + 0x100))
        );
    }

    #[test]
    fn written_code_keeps_the_segment_of_the_address() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let ram = ram_with_code(0x10_000, 0x6000, &[0xAA; 0x100]);
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0xA000_6043),
            Some((0xA000_6000, 0xA000_6100)),
            "KSEG1, and a misaligned address rounds down to its word"
        );
    }

    #[test]
    fn written_code_stops_at_the_ends_of_ram() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let ram = vec![0xAAu8; 0x10_000];
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_6000),
            Some((0x8000_0000, 0x8001_0000))
        );
    }

    #[test]
    fn written_code_is_none_with_less_than_64_bytes_of_ram_left() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let ram = vec![0xAAu8; 0x10_000];
        assert!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_FFC0).is_some(),
            "exactly 64 bytes left is enough"
        );
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_FFC4),
            None
        );
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8010_0000),
            None,
            "past RAM"
        );
    }

    #[test]
    fn written_code_reports_at_most_4_mib_on_each_side() {
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |_| 1);
        let ram = vec![0xAAu8; 2 * WRITTEN_MAX + 0x10_0000];
        let at = WRITTEN_MAX + 0x8_0000;
        assert_eq!(
            written_code(&ram, rom.bytes(), TEST_ENTRY, 0x8000_0000 + at as u32),
            Some((
                0x8000_0000 + (at - WRITTEN_MAX) as u32,
                0x8000_0000 + (at + WRITTEN_MAX) as u32
            ))
        );
    }

    /// A fresh, empty per-game folder for one test's run logs.
    fn run_folder(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("n64rusty-runlog-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The first line [`record_run`] writes to `dma.txt`.
    const DMA_HEADER: &str = "# rom ram length: cartridge copies seen in headless runs\n";

    /// A run that stopped at a call to untranslated code at `addr`, worded
    /// as the runtime's panic is.
    fn call_to_unknown(addr: u32) -> os::Outcome {
        os::Outcome::Panic(format!(
            "call to unknown guest function {addr:#010x} from 0x80001234"
        ))
    }

    fn read_log(dir: &std::path::Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    /// The lines of `missing.txt`. Code another test ran in the shared
    /// interpreter adds lines of its own, so tests look only for theirs.
    fn missing_lines(dir: &std::path::Path) -> Vec<String> {
        let text = std::fs::read_to_string(dir.join("missing.txt")).unwrap_or_default();
        text.lines().map(str::to_string).collect()
    }

    /// Asserts that `missing.txt` has `line`.
    fn assert_missing(dir: &std::path::Path, line: &str) {
        let lines = missing_lines(dir);
        assert!(lines.iter().any(|l| l == line), "{line:?} not in {lines:?}");
    }

    #[test]
    fn record_run_keeps_old_dma_rows_and_adds_new_ones_once() {
        let dir = run_folder("dma-merge");
        std::fs::write(
            dir.join("dma.txt"),
            "# an older header\n0x00001000 0x80001000 0x100\n",
        )
        .unwrap();
        let dma = [(0x2000, 0x8010_0000, 0x100), (0x1000, 0x8000_1000, 0x100)];
        record_run(
            &dir,
            &dma,
            &os::Outcome::Frames(1),
            &[0; 0x1000],
            &[],
            TEST_ENTRY,
        )
        .unwrap();
        assert_eq!(
            read_log(&dir, "dma.txt"),
            format!("{DMA_HEADER}0x00001000 0x80001000 0x100\n0x00002000 0x80100000 0x100\n")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_run_names_the_copy_that_put_missing_code_there() {
        const ROM_AT: usize = 0x2000;
        const RAM_AT: usize = 0x10_0000;
        const LEN: usize = 0x100;
        let dir = run_folder("missing-copy");
        let rom: Vec<u8> = (0..ROM_AT + LEN).map(|i| (i as u8) | 1).collect();
        let ram = ram_with_code(0x20_0000, RAM_AT, &rom[ROM_AT..]);
        let dma = [(ROM_AT as u32, 0x8010_0000, LEN as u32)];
        record_run(
            &dir,
            &dma,
            &call_to_unknown(0x8010_0040),
            &ram,
            &rom,
            TEST_ENTRY,
        )
        .unwrap();
        assert_missing(&dir, "0x80100040 0x00002000 0x80100000 0x100");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_run_saves_code_written_over_a_copy_as_an_image() {
        const RAM_AT: usize = 0x10_0000;
        const LEN: usize = 0x100;
        let dir = run_folder("missing-written");
        let rom: Vec<u8> = (0..0x2100).map(|i| (i as u8) | 1).collect();
        // The game unpacked other code over what the copy brought in.
        let ram = ram_with_code(0x20_0000, RAM_AT, &[0xEE; LEN]);
        let dma = [(0x2000, 0x8010_0000, LEN as u32)];
        record_run(
            &dir,
            &dma,
            &call_to_unknown(0x8010_0040),
            &ram,
            &rom,
            TEST_ENTRY,
        )
        .unwrap();
        assert_missing(&dir, "0x80100040 0xffffffff 0x80100000 0x100");
        let image = std::fs::read(dir.join("images").join("80100000_80100040.bin")).unwrap();
        assert_eq!(image, [0xEE; LEN]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_run_writes_boot_image_code_with_a_zero_copy() {
        let dir = run_folder("missing-boot");
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |i| (i as u8) | 1);
        let ram = ram_with_code(0x10_000, 0x400, rom.boot_segment());
        record_run(
            &dir,
            &[],
            &call_to_unknown(0x8000_0500),
            &ram,
            rom.bytes(),
            TEST_ENTRY,
        )
        .unwrap();
        assert_missing(&dir, "0x80000500 0x00000000 0x00000000 0x0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_run_keeps_old_missing_lines_and_skips_a_null_call() {
        const OLD: &str = "0x80200000 0x00000000 0x00000000 0x0";
        let dir = run_folder("missing-merge");
        std::fs::write(dir.join("missing.txt"), format!("{OLD}\n")).unwrap();
        let rom = rom_filled(TEST_ENTRY, TEST_ROM_LEN, |i| (i as u8) | 1);
        let ram = ram_with_code(0x10_000, 0x400, rom.boot_segment());
        record_run(
            &dir,
            &[],
            &call_to_unknown(0),
            &ram,
            rom.bytes(),
            TEST_ENTRY,
        )
        .unwrap();
        assert_missing(&dir, OLD);
        let lines = missing_lines(&dir);
        let null = lines.iter().find(|l| l.starts_with("0x00000000 "));
        assert_eq!(null, None, "a null pointer is not code");
        record_run(
            &dir,
            &[],
            &call_to_unknown(0x8000_0500),
            &ram,
            rom.bytes(),
            TEST_ENTRY,
        )
        .unwrap();
        assert_missing(&dir, OLD);
        assert_missing(&dir, "0x80000500 0x00000000 0x00000000 0x0");
        let lines = missing_lines(&dir);
        assert!(lines.is_sorted(), "kept in order: {lines:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The exit code `main_args` gives for a command line it refuses.
    const USAGE_ERROR: u8 = 2;

    fn run_with(args: &[&str]) -> ExitCode {
        main_args(
            TEST_GAME,
            None,
            args.iter().map(|a| a.to_string()).collect(),
        )
    }

    #[test]
    fn main_args_refuses_a_command_line_without_a_rom() {
        assert_eq!(run_with(&[]), ExitCode::from(USAGE_ERROR));
        assert_eq!(run_with(&["--frames", "1"]), ExitCode::from(USAGE_ERROR));
    }

    /// A missing ROM also gives exit 2, so exit 2 alone does not show which
    /// check refused. `--hash-out` after the bad flag creates its file only if
    /// parsing went past the bad flag; a correct parse stops first.
    fn assert_refused_at(args: &[&str], case: &str) {
        let probe =
            std::env::temp_dir().join(format!("n64rusty-hash-probe-{case}-{}", std::process::id()));
        let probe_arg = probe.to_string_lossy().into_owned();
        let mut all = args.to_vec();
        all.extend(["--hash-out", probe_arg.as_str()]);
        let code = run_with(&all);
        let parsed_past = probe.exists();
        let _ = std::fs::remove_file(&probe);
        assert_eq!(code, ExitCode::from(USAGE_ERROR), "{case}");
        assert!(!parsed_past, "{case}: parsing went past the bad argument");
    }

    #[test]
    fn main_args_refuses_an_unknown_option() {
        assert_refused_at(&["game.z64", "--no-such-flag", "1"], "unknown-option");
    }

    #[test]
    fn main_args_refuses_an_option_without_its_value() {
        assert_eq!(
            run_with(&["game.z64", "--frames"]),
            ExitCode::from(USAGE_ERROR)
        );
    }

    #[test]
    fn main_args_refuses_a_count_that_is_not_a_number() {
        for flag in ["--frames", "--capture-every", "--capture-scale"] {
            assert_refused_at(&["game.z64", flag, "ten"], &flag[2..]);
        }
    }

    #[test]
    fn main_args_refuses_an_interpret_range_that_is_empty_or_not_hex() {
        for (case, range) in [
            ("reversed", "80100000-80000400"),
            ("empty", "80000400-80000400"),
            ("not-hex", "zz-80000400"),
            ("no-dash", "80000400"),
        ] {
            assert_refused_at(&["game.z64", "--interpret", range], case);
        }
    }

    #[test]
    fn main_args_refuses_a_rom_file_it_cannot_read() {
        let path = std::env::temp_dir().join(format!("no-such-rom-{}.z64", std::process::id()));
        assert_eq!(
            run_with(&[&path.to_string_lossy()]),
            ExitCode::from(USAGE_ERROR)
        );
    }
}
