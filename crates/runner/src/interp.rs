//! Runs calls into code that has no translation (code a game loads to an
//! address no run saw it at, for example) with the reference interpreter, so
//! the game goes on instead of stopping. Calls it makes into translated code
//! or replaced system routines go back through [`Ctx::call`]; calls between
//! untranslated functions stay in the interpreter but cost the same time.
//!
//! Each function run this way is saved like a missed call (see
//! `record_run`), so the next translate covers it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use n64rusty_refcpu::{Cpu, Stop};
use n64rusty_runtime::Ctx;

/// Interpreted functions: entry address to (start, length) of the RAM
/// snapshot saved for it when first run. The bytes themselves go to disk
/// then (see [`install`]), so a run that interprets every call does not keep
/// up to 8 MiB of RAM for each function.
pub(crate) type Seen = BTreeMap<u32, (u32, usize)>;

/// This run's [`Seen`]. A global because `Ctx::on_missing` is a plain
/// `fn(&mut Ctx, u32)`.
static SEEN: Mutex<Seen> = Mutex::new(BTreeMap::new());

/// The RAM range to save around a function, given RAM and its address.
pub(crate) type Snapshot = Box<dyn Fn(&[u8], u32) -> Option<(u32, u32)> + Send>;

/// How this run saves the code it interprets.
struct Saver {
    /// The RAM range to save for a function.
    region: Snapshot,
    /// The folder the snapshots go to, as `<start>_<entry>.bin`.
    images: PathBuf,
    /// Whether a failed write has been reported (once per run).
    warned: bool,
}

static SAVER: Mutex<Option<Saver>> = Mutex::new(None);

/// Where the interpreter last left its own RAM path (a device register, a
/// TLB address, a bad pointer): the instruction a crash there is at.
pub(crate) static LAST_PC: AtomicU32 = AtomicU32::new(0);
/// The instruction at the interpreter's last TLB miss.
pub(crate) static MISS_PC: AtomicU32 = AtomicU32::new(0);

/// `N64RUSTY_WATCH=addr` (hex): the interpreter prints each change of the
/// RAM word there with the instruction it noticed it before. A debugging aid.
static WATCH: OnceLock<Option<usize>> = OnceLock::new();
static WATCHED: AtomicU32 = AtomicU32::new(0);

fn watch_addr() -> Option<usize> {
    let v = std::env::var("N64RUSTY_WATCH").ok()?;
    let a = crate::parse_hex(&v).ok()?;
    Some((a & 0x1FFF_FFFC) as usize)
}

/// Makes `ctx` interpret calls to untranslated code. `region` gives the RAM
/// range to save for a function it runs; the bytes are written to `images`
/// when the function first runs, as `<start>_<entry>.bin`.
pub(crate) fn install(ctx: &mut Ctx, region: Snapshot, images: PathBuf) {
    *SAVER.lock().unwrap() = Some(Saver {
        region,
        images,
        warned: false,
    });
    // Saved for this game's run log only, not the one played before.
    SEEN.lock().unwrap().clear();
    ctx.on_missing = Some(run);
}

/// The functions interpreted so far, with their saved RAM ranges.
pub(crate) fn seen() -> Seen {
    SEEN.lock().unwrap().clone()
}

/// Writes `bytes`, the RAM from `lo` around the function at `entry`, to
/// `images`.
fn save_image(images: &Path, lo: u32, entry: u32, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = images.join(format!("{lo:08x}_{entry:08x}.bin"));
    std::fs::create_dir_all(images)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Saves the RAM around `entry` as it is now, and returns the (start,
/// length) to record. A failed write is reported once and recorded as
/// length 0, so the run log never names an image that is not there.
fn snapshot(ctx: &Ctx, entry: u32) -> (u32, usize) {
    let mut saver = SAVER.lock().unwrap();
    let Some(saver) = saver.as_mut() else {
        return (entry, 0);
    };
    let (lo, hi) = (saver.region)(ctx.mem.bytes(), entry).unwrap_or((entry, entry));
    let at = |a: u32| (ctx.mem.kseg0(a) & 0x1FFF_FFFF) as usize;
    let bytes = ctx.mem.bytes().get(at(lo)..at(hi)).unwrap_or_default();
    if bytes.is_empty() {
        return (lo, 0);
    }
    match save_image(&saver.images, lo, entry, bytes) {
        Ok(_) => (lo, bytes.len()),
        Err(e) => {
            if !saver.warned {
                saver.warned = true;
                eprintln!(
                    "warning: could not save interpreted code to {}: {e} (later failures not shown)",
                    saver.images.display()
                );
            }
            (lo, 0)
        }
    }
}

fn load(cpu: &mut Cpu, c: &Ctx) {
    (cpu.r, cpu.hi, cpu.lo, cpu.f, cpu.fcr31, cpu.fr) = (c.r, c.hi, c.lo, c.f, c.fcr31, c.fr);
    // The callee may have written the TLB.
    if cpu.map_version != c.mem.map_version() {
        cpu.set_map(c.mem.map().to_vec());
        cpu.map_version = c.mem.map_version();
    }
}

fn store(cpu: &Cpu, c: &mut Ctx) {
    (c.r, c.hi, c.lo, c.f, c.fcr31, c.fr) = (cpu.r, cpu.hi, cpu.lo, cpu.f, cpu.fcr31, cpu.fr);
}

fn run(ctx: &mut Ctx, entry: u32) {
    // Code `--interpret` sends here may have a translation: nothing to save.
    let translated = ctx.interpret.is_some() && ctx.has_translation(entry);
    let mut seen = SEEN.lock().unwrap();
    if !translated && !seen.contains_key(&entry) {
        let saved = snapshot(ctx, entry);
        seen.insert(entry, saved);
    }
    drop(seen);
    let mut cpu = Cpu::new(Vec::new());
    load(&mut cpu, ctx);
    // The function is done when it returns to its caller.
    let stop = ctx.r[31] as u32;
    cpu.jump_to(entry);
    // The 4 KiB page of code known to be mapped. Forgotten after anything
    // that can run game code or bring pages in (and so push others out).
    let mut known = u32::MAX;
    // Where the next instruction in sequence would be: translated code is
    // looked for only where execution jumped to.
    // shortcut: code that runs on into the start of a translated function
    // without a jump stays here; no game has needed it.
    let mut next = entry;
    while cpu.pc != stop {
        let pc = cpu.pc;
        if pc >> 12 != known {
            if ctx.unmapped(pc) {
                // Code in a page not loaded yet: the game's refill routine
                // loads it, as on a TLB miss.
                store(&cpu, ctx);
                MISS_PC.store(pc, Ordering::Relaxed);
                ctx.tlb_miss(pc);
                load(&mut cpu, ctx);
            }
            known = pc >> 12;
        }
        let jumped = pc != next;
        next = pc.wrapping_add(4);
        if jumped && pc != entry && ctx.has_translation(pc) {
            // A call (or tail jump) into translated code: run it there, then
            // carry on at the return address it leaves.
            store(&cpu, ctx);
            ctx.call(pc);
            known = u32::MAX;
            load(&mut cpu, ctx);
            cpu.jump_to(ctx.r[31] as u32);
            continue;
        }
        // Fetched once, here, and handed to the CPU. A fetch it cannot make
        // fails again in `step` below, as before.
        let fetched = cpu.fetch(ctx.mem.bytes());
        let word = match fetched {
            Ok(w) => w,
            Err(_) => ctx.mem.read_u32(pc),
        };
        if let Some(at) = *WATCH.get_or_init(watch_addr) {
            let now = ctx
                .mem
                .bytes()
                .get(at..at + 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()));
            let last = WATCHED.swap(now.unwrap_or(0), Ordering::Relaxed);
            if now.is_some_and(|n| n != last) {
                eprintln!(
                    "watch: {last:#010x} -> {:#010x} before {pc:#010x} (ra {:#010x})",
                    now.unwrap(),
                    cpu.r[31] as u32
                );
            }
        }
        let target = match word >> 26 {
            3 => Some((pc & 0xF000_0000) | (word & 0x03FF_FFFF) << 2),
            // JALR links into rd: with rd = 0 it is a plain jump.
            0 if word & 0x3F == 9 && (word >> 11) & 31 != 0 => {
                Some(cpu.r[(word >> 21) as usize & 31] as u32)
            }
            _ => None,
        };
        // Calls into translated code are charged by `Ctx::call`, except a
        // call back to `entry` (recursion under `--interpret`), which stays
        // here.
        let calls = target.is_some_and(|t| t == entry || !ctx.has_translation(t));
        let stepped = match fetched {
            Ok(w) => cpu.step_word(ctx.mem.bytes_mut(), w),
            Err(e) => Err(e),
        };
        if let Err(e) = stepped {
            if let Stop::BadAccess { addr, .. } = e {
                if ctx.unmapped(addr) {
                    store(&cpu, ctx);
                    MISS_PC.store(cpu.pc, Ordering::Relaxed);
                    let mapped = ctx.tlb_miss(addr);
                    known = u32::MAX;
                    load(&mut cpu, ctx);
                    if mapped {
                        // The instruction did nothing yet: run it again.
                        continue;
                    }
                }
            }
            // The reference CPU knows plain RAM only: COP0 and device
            // registers go through the runtime, as translated code does.
            if !outside(&mut cpu, ctx, word) {
                panic!("interpreting untranslated code from {entry:#010x}: {e:?}");
            }
            cpu.skip();
            // It may have been a TLB write.
            known = u32::MAX;
        }
        if calls {
            // A call inside untranslated code costs what a translated call
            // does; the deadline handler may switch threads, so the
            // registers go through `ctx`.
            store(&cpu, ctx);
            ctx.tick(pc);
            known = u32::MAX;
            load(&mut cpu, ctx);
        }
    }
    store(&cpu, ctx);
}

/// Runs `word` at `cpu.pc` through `ctx` if it is a COP0 move, a TLB
/// instruction or a load or store the reference CPU cannot reach; returns
/// whether it did.
fn outside(cpu: &mut Cpu, ctx: &mut Ctx, word: u32) -> bool {
    // A crash from here reports this instruction and its registers.
    LAST_PC.store(cpu.pc, Ordering::Relaxed);
    store(cpu, ctx);
    let (rs, rt, rd) = (
        (word >> 21) & 31,
        (word >> 16) as usize & 31,
        (word >> 11) as usize & 31,
    );
    let ea = (cpu.r[rs as usize] as u32).wrapping_add(word as u16 as i16 as i32 as u32);
    let t = cpu.r[rt];
    let sx = |v: u32| v as i32 as i64 as u64;
    let v = match word >> 26 {
        0x10 if rs == 0 => ctx.mfc0(rd),
        0x10 if rs == 4 => {
            ctx.mtc0(rd, t);
            return true;
        }
        0x10 if rs == 0x10 => {
            store(cpu, ctx);
            ctx.tlb(word & 0x3F);
            load(cpu, ctx);
            return true;
        }
        0x20 => ctx.mem.read_u8(ea) as i8 as i64 as u64,
        0x24 => u64::from(ctx.mem.read_u8(ea)),
        0x21 => ctx.mem.read_u16(ea) as i16 as i64 as u64,
        0x25 => u64::from(ctx.mem.read_u16(ea)),
        0x23 => sx(ctx.mem.read_u32(ea)),
        0x27 => u64::from(ctx.mem.read_u32(ea)),
        0x37 => ctx.mem.read_u64(ea),
        0x28 | 0x29 | 0x2B | 0x3F => {
            match word >> 26 {
                0x28 => ctx.mem.write_u8(ea, t as u8),
                0x29 => ctx.mem.write_u16(ea, t as u16),
                0x2B => ctx.mem.write_u32(ea, t as u32),
                _ => ctx.mem.write_u64(ea, t),
            }
            return true;
        }
        _ => return false,
    };
    if rt != 0 {
        cpu.r[rt] = v;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use n64rusty_runtime::{sx32, Memory};

    /// The tests share the installed snapshot rule and the saved snapshots:
    /// one at a time.
    static ONE: Mutex<()> = Mutex::new(());

    /// An empty folder for one test's saved images.
    fn images_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("n64rusty-interp-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn each_function_is_saved_as_it_was_when_first_run() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        const BLOB: u32 = 0x8000_4000;
        const BLOB_END: u32 = BLOB + 0x20;
        let images = images_dir("first-run");
        let mut ctx = Ctx::new(Memory::new(0x10000));
        // Two functions, each `jr ra; nop`, in one blob of written code.
        for f in [BLOB, BLOB + 0x10] {
            ctx.mem.write_u32(f, 0x03E0_0008);
        }
        install(
            &mut ctx,
            Box::new(|_, _| Some((BLOB, BLOB_END))),
            images.clone(),
        );
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(BLOB);
        // The blob changes after the first function ran, before the second.
        ctx.mem.write_u32(BLOB + 0x08, 0x1234_5678);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(BLOB + 0x10);
        // Changed again, then the first runs again: its image stays.
        ctx.mem.write_u32(BLOB + 0x0C, 0x9ABC_DEF0);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(BLOB);
        let seen = seen();
        assert_eq!(seen[&BLOB], (BLOB, 0x20));
        assert_eq!(seen[&(BLOB + 0x10)], (BLOB, 0x20));
        let image = |name: &str| std::fs::read(images.join(name)).unwrap();
        let (first, second) = (
            image("80004000_80004000.bin"),
            image("80004000_80004010.bin"),
        );
        assert_eq!(first[..4], 0x03E0_0008u32.to_be_bytes());
        assert_eq!(first[0x08..0x10], [0; 8], "as it was when first run");
        assert_eq!(second[0x08..0x0C], 0x1234_5678u32.to_be_bytes());
        assert_eq!(second[0x0C..0x10], [0; 4]);
        std::fs::remove_dir_all(images).unwrap();
    }

    #[test]
    fn a_failed_save_records_nothing_to_translate() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut ctx = Ctx::new(Memory::new(0x10000));
        ctx.mem.write_u32(0x8000_4000, 0x03E0_0008);
        // A file where the folder should be: the write fails.
        let images = images_dir("failed-save");
        std::fs::write(&images, b"not a folder").unwrap();
        install(&mut ctx, Box::new(|_, a| Some((a, a + 32))), images.clone());
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_4000);
        assert_eq!(
            seen()[&0x8000_4000],
            (0x8000_4000, 0),
            "no image, no length"
        );
        std::fs::remove_file(images).unwrap();
    }

    #[test]
    fn untranslated_code_runs_and_calls_translated_code() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        fn translated(c: &mut Ctx) {
            c.r[2] = 41;
        }
        let mut ctx = Ctx::new(Memory::new(0x10000));
        // addiu sp,sp,-8; sw ra,0(sp); jal 0x80002000; nop;
        // lw ra,0(sp); addiu v0,v0,1; jr ra; addiu sp,sp,8
        let code = [
            0x27BD_FFF8u32,
            0xAFBF_0000,
            0x0C00_0800,
            0,
            0x8FBF_0000,
            0x2442_0001,
            0x03E0_0008,
            0x27BD_0008,
        ];
        for (k, w) in code.iter().enumerate() {
            ctx.mem.write_u32(0x8000_1000 + 4 * k as u32, *w);
        }
        ctx.register(0x8000_2000, translated);
        install(
            &mut ctx,
            Box::new(|_, a| Some((a, a + 32))),
            images_dir("other"),
        );
        ctx.r[29] = sx32(0x8000_8000);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_1000);
        assert_eq!(ctx.r[2], 42);
        assert_eq!(ctx.r[29], sx32(0x8000_8000));
        assert_eq!(seen()[&0x8000_1000], (0x8000_1000, 32));
    }

    #[test]
    fn calls_into_saved_image_code_run_its_translation() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        fn translated(c: &mut Ctx) {
            c.r[2] = 41;
        }
        let mut ctx = Ctx::new(Memory::new(0x10000));
        // addiu sp,sp,-8; sw ra,0(sp); jal 0x80002000; nop;
        // lw ra,0(sp); addiu v0,v0,1; jr ra; addiu sp,sp,8
        let code = [
            0x27BD_FFF8u32,
            0xAFBF_0000,
            0x0C00_0800,
            0,
            0x8FBF_0000,
            0x2442_0001,
            0x03E0_0008,
            0x27BD_0008,
        ];
        for (k, w) in code.iter().enumerate() {
            ctx.mem.write_u32(0x8000_3000 + 4 * k as u32, *w);
        }
        // The callee's bytes (jr ra; nop) would leave v0 alone if run raw.
        ctx.mem.write_u32(0x8000_2000, 0x03E0_0008);
        let bytes = [0x03, 0xE0, 0x00, 0x08, 0, 0, 0, 0];
        ctx.register_image_fn(
            0x8000_2000,
            8,
            n64rusty_runtime::code_hash(&bytes),
            translated,
        );
        // The same snapshot rule as the other test: they run in parallel and
        // share the installed rule.
        install(
            &mut ctx,
            Box::new(|_, a| Some((a, a + 32))),
            images_dir("other"),
        );
        ctx.r[29] = sx32(0x8000_8000);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_3000);
        assert_eq!(ctx.r[2], 42);
    }

    #[test]
    fn cop0_moves_go_through_the_runtime() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut ctx = Ctx::new(Memory::new(0x10000));
        // mtc0 a0,Status; mfc0 v0,Status; jr ra; nop
        let code = [0x4084_6000u32, 0x4002_6000, 0x03E0_0008, 0];
        for (k, w) in code.iter().enumerate() {
            ctx.mem.write_u32(0x8000_3000 + 4 * k as u32, *w);
        }
        install(
            &mut ctx,
            Box::new(|_, a| Some((a, a + 32))),
            images_dir("other"),
        );
        ctx.r[4] = 0x1234;
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_3000);
        assert_eq!(ctx.r[2], 0x1234);
    }
}
