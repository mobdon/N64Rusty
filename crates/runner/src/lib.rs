//! Headless host for recompiled N64 code.
//!
//! A generated game crate's `main` hands its function table to [`main`],
//! which loads the player's ROM, sets up RAM the way the console's boot code
//! leaves it, and runs the game's entry point. Execution runs until the
//! first panic, which is reported with the most recent calls, so system
//! routines can be replaced one at a time. Nothing is drawn, no window is
//! opened and no sound is played.

pub mod devices;
pub mod media;
pub mod os;

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

/// Builds a context with RAM laid out as the boot code leaves it.
pub fn boot(rom: &Rom, game: &Game) -> Ctx {
    let mut mem = Memory::new(RAM_SIZE);
    let seg = rom.boot_segment();
    let at = (rom.entry() & 0x1FFF_FFFF) as usize;
    let n = seg.len().min(RAM_SIZE - at);
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
    mem.write_u32(0x8000_0318, RAM_SIZE as u32);
    let mut ctx = Ctx::new(mem);
    (game.register)(&mut ctx);
    // A stack at the top of RAM until the game sets its own.
    ctx.r[29] = sx32(0x8000_0000 + RAM_SIZE as u32 - 0x10);
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
fn record_run(
    local: &std::path::Path,
    dma: &[(u32, u32, u32)],
    outcome: &os::Outcome,
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
    if let os::Outcome::Panic(m) = outcome {
        if let Some(rest) = m.strip_prefix("call to unknown guest function ") {
            let addr_text = rest.split_whitespace().next().unwrap_or("");
            let addr = u32::from_str_radix(addr_text.trim_start_matches("0x"), 16).unwrap_or(0);
            // Which copy put code there: the latest one covering it.
            let src = dma
                .iter()
                .rev()
                .find(|(_, ram, len)| addr >= *ram && addr < ram.wrapping_add(*len));
            // No copy covers it: code in the boot image reached only
            // through a pointer, written with a zero copy.
            let (rom, ram, len) = src.copied().unwrap_or((0, 0, 0));
            if addr == 0 {
                return Ok(());
            }
            let entry = format!("{addr:#010x} {rom:#010x} {ram:#010x} {len:#x}");
            let mut missing: BTreeSet<String> =
                read("missing.txt").lines().map(str::to_string).collect();
            missing.insert(entry);
            let text: String = missing
                .iter()
                .filter(|l| !l.is_empty())
                .map(|l| format!("{l}\n"))
                .collect();
            std::fs::write(local.join("missing.txt"), text)?;
        }
    }
    Ok(())
}

static NAMES: Mutex<BTreeMap<u32, String>> = Mutex::new(BTreeMap::new());

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
/// [--capture dir] [--capture-every n] [--wav file] [--input script]`.
///
/// Display lists and audio command lists always go through the graphics
/// and audio interpreters. With `--capture`, every n-th finished frame (60
/// by default) is rendered off screen and saved as a PNG in `dir`; with
/// `--wav`, the sound is recorded to a file; `--input` plays a controller
/// script against retrace numbers. Nothing is shown or played.
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rom_path = None;
    let mut symbols = Vec::new();
    let mut frames = 600;
    // The per-game folder: config, and run logs the translator reads.
    let mut local = PathBuf::from("..");
    let mut capture = None;
    let mut every = 60;
    let mut wav = None;
    let mut input = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--symbols" | "--config" => symbols.extend(it.next().map(PathBuf::from)),
            "--local" => local = it.next().map(PathBuf::from).unwrap_or(local),
            "--frames" => frames = it.next().and_then(|v| v.parse().ok()).unwrap_or(frames),
            "--capture" => capture = it.next().map(PathBuf::from),
            "--wav" => wav = it.next().map(PathBuf::from),
            "--input" => input = it.next().map(PathBuf::from),
            "--capture-every" => every = it.next().and_then(|v| v.parse().ok()).unwrap_or(every),
            s => rom_path = Some(PathBuf::from(s)),
        }
    }
    let Some(rom_path) = rom_path else {
        eprintln!(
            "usage: <game> <rom> [--symbols file]... [--config file] [--frames n] [--local dir] [--capture dir] [--capture-every n] [--wav file] [--input script]"
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
    let default_config = local.join("config.toml");
    if default_config.exists() {
        symbols.push(default_config);
    }
    let names = read_names(&symbols);
    let by_name: BTreeMap<String, u32> = names.iter().map(|(a, n)| (n.clone(), *a)).collect();
    *NAMES.lock().unwrap() = names;

    let mut ctx = boot(&rom, &game);
    let rom_bytes: Arc<[u8]> = Arc::from(rom.bytes());
    ctx.mem.set_io(Io {
        sp_mem: vec![0; 0x2000],
        rom: rom_bytes.clone(),
        regs: Default::default(),
    });
    let hooked = os::install(&mut ctx, &by_name);
    println!("replaced {} system routines", hooked.len());

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
    let n_audio = ["n_alSynNew", "n_alAudioFrame"]
        .iter()
        .any(|n| by_name.contains_key(*n));
    let devices = devices
        .unwrap_or_else(|| Box::new(media::Media::new(capture, every, wav, n_audio, script)));
    let (outcome, report, frames_run) = os::run(ctx, game.entry, rom_bytes, frames, devices);
    let (_, dma, threads, device_summary) = os::summary();
    if let Err(e) = record_run(&local, &dma, &outcome) {
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
    print!("{device_summary}");
    match outcome {
        os::Outcome::Frames(n) => {
            println!("ran {n} frames without a panic");
            ExitCode::SUCCESS
        }
        os::Outcome::Returned => {
            println!("the boot code returned");
            ExitCode::SUCCESS
        }
        os::Outcome::Deadlock(d) => {
            println!("deadlock: every thread waits and nothing can wake one\n{d}");
            ExitCode::from(1)
        }
        os::Outcome::Panic(m) => {
            println!("panic: {m}\n{report}");
            ExitCode::from(1)
        }
    }
}
