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

    let mut cpu = Cpu::new(ram.clone());
    cpu.r = regs;
    cpu.hi = hi;
    cpu.lo = lo_reg;
    cpu.run(p.funcs[0].0, SENTINEL, 10_000)
        .unwrap_or_else(|e| panic!("{}: reference CPU stopped: {e:?}", p.name));

    let mut ctx = Ctx::new(Memory::from_bytes(ram));
    ctx.r = regs;
    ctx.hi = hi;
    ctx.lo = lo_reg;
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
            "{}: translated code differs from the reference CPU\n{}\n  start: {} hi={hi:#x} lo={lo_reg:#x}",
            p.name,
            diffs.join("\n"),
            init.join(" ")
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
