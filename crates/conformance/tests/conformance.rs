//! Runs every synthetic program on the translated code and on the reference
//! CPU from the same random states and compares the results.

use n64rusty_conformance::generated::PROGRAMS;
use n64rusty_conformance::programs::{self, Program, DATA, RAM, SENTINEL};
use n64rusty_refcpu::Cpu;
use n64rusty_runtime::{sx32, Ctx, Memory};

const ITERATIONS: usize = 64;

/// xorshift64*, so the tests need no dependencies and are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Register values that hit the edge cases: zero, signs, 32-bit
    /// boundaries, values that are not properly sign-extended, and noise.
    fn reg(&mut self) -> u64 {
        const EDGES: [u64; 12] = [
            0,
            1,
            u64::MAX,
            0x7FFF_FFFF,
            0xFFFF_FFFF_8000_0000,
            0x8000_0000,
            0xFFFF_FFFF,
            0x8000_0000_0000_0000,
            0x7FFF_FFFF_FFFF_FFFF,
            0x1_0000_0000,
            31,
            32,
        ];
        match self.next() % 4 {
            0 => EDGES[(self.next() % EDGES.len() as u64) as usize],
            1 => sx32(self.next() as u32),
            2 => self.next() % 64,
            _ => self.next(),
        }
    }
}

impl Rng {
    /// Float register contents: special singles in either half, special
    /// doubles, or noise.
    fn freg(&mut self) -> u64 {
        const SINGLES: [u32; 14] = [
            0,
            0x8000_0000,
            0x3F80_0000, // 1.0
            0xBF80_0000, // -1.0
            0x3FC0_0000, // 1.5
            0x4020_0000, // 2.5
            0xC020_0000, // -2.5
            0x7F80_0000, // inf
            0xFF80_0000, // -inf
            0x7FC0_0000, // NaN
            0x0000_0001, // denormal
            0x4F00_0000, // 2^31
            0xCF00_0000, // -2^31
            0x5F00_0000, // 2^63
        ];
        const DOUBLES: [u64; 12] = [
            0,
            0x8000_0000_0000_0000,
            0x3FF0_0000_0000_0000, // 1.0
            0x3FF8_0000_0000_0000, // 1.5
            0x4004_0000_0000_0000, // 2.5
            0xC004_0000_0000_0000, // -2.5
            0x7FF0_0000_0000_0000, // inf
            0x7FF8_0000_0000_0000, // NaN
            0x0000_0000_0000_0001, // denormal
            0x41E0_0000_0000_0000, // 2^31
            0xC1E0_0000_0000_0000, // -2^31
            0x43E0_0000_0000_0000, // 2^63
        ];
        let single = |r: &mut Rng| match r.next() % 3 {
            0 => SINGLES[(r.next() % SINGLES.len() as u64) as usize],
            // An ordinary value of moderate size.
            1 => ((r.next() as f32 / u64::MAX as f32 - 0.5) * 4096.0).to_bits(),
            _ => r.next() as u32,
        };
        match self.next() % 4 {
            0 => DOUBLES[(self.next() % DOUBLES.len() as u64) as usize],
            1 => ((self.next() as f64 / u64::MAX as f64 - 0.5) * 1e6).to_bits(),
            2 => (u64::from(single(self)) << 32) | u64::from(single(self)),
            _ => self.next(),
        }
    }
}

fn noop(_: &mut Ctx) {}

fn run_one(p: &Program, register: fn(&mut Ctx), rng: &mut Rng) {
    // Memory: code at its addresses, random bytes around DATA.
    let mut ram = vec![0u8; RAM];
    let lo = (DATA - 0x800) as usize & (RAM - 1);
    for b in &mut ram[lo..lo + 0x1000] {
        *b = rng.next() as u8;
    }
    for (addr, words) in &p.funcs {
        let base = (*addr & 0x1FFF_FFFF) as usize;
        for (k, w) in words.iter().enumerate() {
            ram[base + 4 * k..base + 4 * k + 4].copy_from_slice(&w.to_be_bytes());
        }
    }

    let mut regs = [0u64; 32];
    for r in regs.iter_mut().skip(1) {
        *r = rng.reg();
    }
    for &(n, v) in &p.fixed {
        regs[n as usize] = sx32(v);
    }
    regs[31] = sx32(SENTINEL);
    let (hi, lo_reg) = (rng.reg(), rng.reg());
    let mut fregs = [0u64; 32];
    for f in &mut fregs {
        *f = rng.freg();
    }
    // Rounding mode and condition bit at random; both register modes.
    let fcr31 = (rng.next() as u32 & 3) | if rng.next() % 2 == 0 { 1 << 23 } else { 0 };
    let fr = rng.next() % 2 == 0;

    let mut cpu = Cpu::new(ram.clone());
    cpu.r = regs;
    cpu.hi = hi;
    cpu.lo = lo_reg;
    cpu.f = fregs;
    cpu.fcr31 = fcr31;
    cpu.fr = fr;
    cpu.run(p.funcs[0].0, SENTINEL, 10_000)
        .unwrap_or_else(|e| panic!("{}: reference CPU stopped: {e:?}", p.name));

    let mut ctx = Ctx::new(Memory::from_bytes(ram));
    ctx.r = regs;
    ctx.hi = hi;
    ctx.lo = lo_reg;
    ctx.f = fregs;
    ctx.fcr31 = fcr31;
    ctx.fr = fr;
    register(&mut ctx);
    ctx.register(SENTINEL, noop);
    ctx.call(p.funcs[0].0);

    let mut diffs = Vec::new();
    for n in 0..32 {
        if ctx.r[n] != cpu.r[n] {
            diffs.push(format!(
                "  r{n}: translated {:#018x}, reference {:#018x}",
                ctx.r[n], cpu.r[n]
            ));
        }
    }
    for n in 0..32 {
        if ctx.f[n] != cpu.f[n] {
            diffs.push(format!(
                "  f{n}: translated {:#018x}, reference {:#018x}",
                ctx.f[n], cpu.f[n]
            ));
        }
    }
    if ctx.fcr31 != cpu.fcr31 {
        diffs.push(format!(
            "  fcr31: translated {:#010x}, reference {:#010x}",
            ctx.fcr31, cpu.fcr31
        ));
    }
    if ctx.hi != cpu.hi {
        diffs.push(format!(
            "  hi: translated {:#018x}, reference {:#018x}",
            ctx.hi, cpu.hi
        ));
    }
    if ctx.lo != cpu.lo {
        diffs.push(format!(
            "  lo: translated {:#018x}, reference {:#018x}",
            ctx.lo, cpu.lo
        ));
    }
    let same = ctx.mem.bytes() == &cpu.mem[..];
    if let Some(at) = (0..RAM).find(|&k| !same && ctx.mem.bytes()[k] != cpu.mem[k]) {
        diffs.push(format!(
            "  memory differs first at {:#010x}: translated {:#04x}, reference {:#04x}",
            0x8000_0000 + at as u32,
            ctx.mem.bytes()[at],
            cpu.mem[at]
        ));
    }
    if !diffs.is_empty() {
        let init: Vec<String> = regs
            .iter()
            .enumerate()
            .filter(|(_, v)| **v != 0)
            .map(|(n, v)| format!("r{n}={v:#x}"))
            .collect();
        panic!(
            "{}: translated code differs from the reference CPU\n{}\n  start: {} hi={hi:#x} lo={lo_reg:#x} fr={fr} fcr31={fcr31:#x}\n  floats: {:x?}",
            p.name,
            diffs.join("\n"),
            init.join(" "),
            fregs
        );
    }
}

#[test]
fn every_program_matches_the_reference_cpu() {
    let progs = programs::all();
    assert_eq!(progs.len(), PROGRAMS.len());
    let mut failures = Vec::new();
    for (n, (p, (name, register))) in progs.iter().zip(PROGRAMS).enumerate() {
        assert_eq!(&p.name, name);
        let mut rng =
            Rng(0x9E37_79B9_7F4A_7C15 ^ (n as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
        for _ in 0..ITERATIONS {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_one(p, *register, &mut rng)
            }));
            if let Err(e) = r {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                failures.push(msg);
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} programs failed:\n{}",
        failures.len(),
        progs.len(),
        failures.join("\n")
    );
}
