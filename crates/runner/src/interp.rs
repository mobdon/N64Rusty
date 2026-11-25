//! Runs calls into code that has no translation (code a game loads to an
//! address no run saw it at, for example) with the reference interpreter, so
//! the game goes on instead of stopping. Calls it makes into translated code
//! or replaced system routines go back through [`Ctx::call`]; calls between
//! untranslated functions stay in the interpreter but cost the same time.
//!
//! Each function run this way is saved like a missed call (see
//! `record_run`), so the next translate covers it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use n64rusty_refcpu::Cpu;
use n64rusty_runtime::Ctx;

/// First-run snapshots of the RAM around each interpreted function: entry
/// address to (start of the snapshot, bytes).
static SEEN: Mutex<BTreeMap<u32, (u32, Vec<u8>)>> = Mutex::new(BTreeMap::new());

/// The RAM range to save around a function, given RAM and its address.
pub(crate) type Snapshot = Box<dyn Fn(&[u8], u32) -> Option<(u32, u32)> + Send>;

static REGION: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Makes `ctx` interpret calls to untranslated code; `region` gives the RAM
/// range to save for a function it runs.
pub(crate) fn install(ctx: &mut Ctx, region: Snapshot) {
    *REGION.lock().unwrap() = Some(region);
    ctx.on_missing = Some(run);
}

/// The functions interpreted so far, with their saved RAM.
pub(crate) fn seen() -> BTreeMap<u32, (u32, Vec<u8>)> {
    SEEN.lock().unwrap().clone()
}

fn load(cpu: &mut Cpu, c: &Ctx) {
    (cpu.r, cpu.hi, cpu.lo, cpu.f, cpu.fcr31, cpu.fr) = (c.r, c.hi, c.lo, c.f, c.fcr31, c.fr);
}

fn store(cpu: &Cpu, c: &mut Ctx) {
    (c.r, c.hi, c.lo, c.f, c.fcr31, c.fr) = (cpu.r, cpu.hi, cpu.lo, cpu.f, cpu.fcr31, cpu.fr);
}

fn run(ctx: &mut Ctx, entry: u32) {
    SEEN.lock().unwrap().entry(entry).or_insert_with(|| {
        let region = REGION
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|f| f(ctx.mem.bytes(), entry));
        let (lo, hi) = region.unwrap_or((entry, entry));
        let at = |a: u32| (a & 0x1FFF_FFFF) as usize;
        let bytes = ctx.mem.bytes().get(at(lo)..at(hi)).unwrap_or_default();
        (lo, bytes.to_vec())
    });
    let mut cpu = Cpu::new(Vec::new());
    load(&mut cpu, ctx);
    // The function is done when it returns to its caller.
    let stop = ctx.r[31] as u32;
    cpu.jump_to(entry);
    while cpu.pc != stop {
        let pc = cpu.pc;
        if pc != entry && ctx.has_translation(pc) {
            // A call (or tail jump) into translated code: run it there, then
            // carry on at the return address it leaves.
            store(&cpu, ctx);
            ctx.call(pc);
            load(&mut cpu, ctx);
            cpu.jump_to(ctx.r[31] as u32);
            continue;
        }
        let word = ctx.mem.read_u32(pc);
        let target = match word >> 26 {
            3 => Some((pc & 0xF000_0000) | (word & 0x03FF_FFFF) << 2),
            // JALR links into rd: with rd = 0 it is a plain jump.
            0 if word & 0x3F == 9 && (word >> 11) & 31 != 0 => {
                Some(cpu.r[(word >> 21) as usize & 31] as u32)
            }
            _ => None,
        };
        // Calls into translated code are charged by `Ctx::call`.
        let calls = target.is_some_and(|t| !ctx.has_translation(t));
        if let Err(e) = cpu.step_on(ctx.mem.bytes_mut()) {
            // The reference CPU knows plain RAM only: COP0 and device
            // registers go through the runtime, as translated code does.
            if !outside(&mut cpu, ctx, word) {
                panic!("interpreting untranslated code from {entry:#010x}: {e:?}");
            }
            cpu.skip();
        }
        if calls {
            // A call inside untranslated code costs what a translated call
            // does; the deadline handler may switch threads, so the
            // registers go through `ctx`.
            store(&cpu, ctx);
            ctx.tick(pc);
            load(&mut cpu, ctx);
        }
    }
    store(&cpu, ctx);
}

/// Runs `word` at `cpu.pc` through `ctx` if it is a COP0 move, a TLB
/// instruction or a load or store the reference CPU cannot reach; returns
/// whether it did.
fn outside(cpu: &mut Cpu, ctx: &mut Ctx, word: u32) -> bool {
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

    #[test]
    fn untranslated_code_runs_and_calls_translated_code() {
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
        install(&mut ctx, Box::new(|_, a| Some((a, a + 32))));
        ctx.r[29] = sx32(0x8000_8000);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_1000);
        assert_eq!(ctx.r[2], 42);
        assert_eq!(ctx.r[29], sx32(0x8000_8000));
        assert_eq!(seen()[&0x8000_1000].1.len(), 32);
    }

    #[test]
    fn calls_into_saved_image_code_run_its_translation() {
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
        install(&mut ctx, Box::new(|_, a| Some((a, a + 32))));
        ctx.r[29] = sx32(0x8000_8000);
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_3000);
        assert_eq!(ctx.r[2], 42);
    }

    #[test]
    fn cop0_moves_go_through_the_runtime() {
        let mut ctx = Ctx::new(Memory::new(0x10000));
        // mtc0 a0,Status; mfc0 v0,Status; jr ra; nop
        let code = [0x4084_6000u32, 0x4002_6000, 0x03E0_0008, 0];
        for (k, w) in code.iter().enumerate() {
            ctx.mem.write_u32(0x8000_3000 + 4 * k as u32, *w);
        }
        install(&mut ctx, Box::new(|_, a| Some((a, a + 32))));
        ctx.r[4] = 0x1234;
        ctx.r[31] = sx32(0x8000_0F00);
        ctx.call(0x8000_3000);
        assert_eq!(ctx.r[2], 0x1234);
    }
}
