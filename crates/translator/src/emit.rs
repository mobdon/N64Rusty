//! Rust emitter: one Rust function per guest function.
//!
//! The function body is `loop { match pc { ... } }` over basic blocks.
//! A block starts at the function entry, at every branch target inside the
//! function, and after every branch or jump plus its delay slot.
//!
//! Branch delay slots: a branch's condition (or a register jump's target) is
//! read first, then the link register is written, then the delay slot runs,
//! and only then does control move. "Likely" branches run the delay slot only
//! when taken. Jumps and branches that leave the function are tail calls,
//! `jr $ra` returns, and every call goes through `Ctx::call`.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::decode::{Flow, Inst, Op};
use crate::{Function, TranslateError};

const RT: &str = "::n64rusty_runtime";

/// Name of the Rust function generated for the guest function at `addr`.
pub fn fn_name(addr: u32) -> String {
    format!("f_{addr:08x}")
}

fn r(n: u8) -> String {
    format!("ctx.r[{n}]")
}

fn sx_lit(v: u32) -> String {
    format!("{:#018x}u64", v as i32 as i64 as u64)
}

fn set(d: u8, expr: &str) -> String {
    if d == 0 {
        // Writes to $zero are discarded.
        String::new()
    } else {
        format!("ctx.r[{d}] = {expr};")
    }
}

/// Address expression for a load or store.
fn addr(i: &Inst) -> String {
    format!(
        "(ctx.r[{}] as u32).wrapping_add({:#010x}u32)",
        i.rs,
        i.simm() as u32
    )
}

/// Statements for an instruction that does not change control flow.
fn simple(i: &Inst, pc: u32) -> String {
    use Op::*;
    let (s, t, d, sa) = (i.rs, i.rt, i.rd, i.sa);
    let sx = |e: String| format!("{RT}::sx32({e})");
    let simm64 = format!("{:#018x}u64", i.simm() as u64);
    let simm32 = format!("{:#010x}u32", i.simm() as u32);
    let uimm = format!("{:#06x}u64", i.uimm());
    match i.op {
        Sll => set(d, &sx(format!("({} as u32) << {sa}", r(t)))),
        Srl => set(d, &sx(format!("({} as u32) >> {sa}", r(t)))),
        // The VR4300 shifts the whole 64-bit register, then keeps the low word.
        Sra => set(d, &sx(format!("(({} as i64) >> {sa}) as u32", r(t)))),
        Sllv => set(d, &sx(format!("({} as u32) << ({} & 31)", r(t), r(s)))),
        Srlv => set(d, &sx(format!("({} as u32) >> ({} & 31)", r(t), r(s)))),
        Srav => set(
            d,
            &sx(format!("(({} as i64) >> ({} & 31)) as u32", r(t), r(s))),
        ),
        Dsllv => set(d, &format!("{} << ({} & 63)", r(t), r(s))),
        Dsrlv => set(d, &format!("{} >> ({} & 63)", r(t), r(s))),
        Dsrav => set(d, &format!("(({} as i64) >> ({} & 63)) as u64", r(t), r(s))),
        Dsll => set(d, &format!("{} << {sa}", r(t))),
        Dsrl => set(d, &format!("{} >> {sa}", r(t))),
        Dsra => set(d, &format!("(({} as i64) >> {sa}) as u64", r(t))),
        Dsll32 => set(d, &format!("{} << {}", r(t), sa + 32)),
        Dsrl32 => set(d, &format!("{} >> {}", r(t), sa + 32)),
        Dsra32 => set(d, &format!("(({} as i64) >> {}) as u64", r(t), sa + 32)),
        Mfhi => set(d, "ctx.hi"),
        Mflo => set(d, "ctx.lo"),
        Mthi => format!("ctx.hi = {};", r(s)),
        Mtlo => format!("ctx.lo = {};", r(s)),
        Mult => format!("ctx.mult({}, {});", r(s), r(t)),
        Multu => format!("ctx.multu({}, {});", r(s), r(t)),
        Div => format!("ctx.div({}, {});", r(s), r(t)),
        Divu => format!("ctx.divu({}, {});", r(s), r(t)),
        Dmult => format!("ctx.dmult({}, {});", r(s), r(t)),
        Dmultu => format!("ctx.dmultu({}, {});", r(s), r(t)),
        Ddiv => format!("ctx.ddiv({}, {});", r(s), r(t)),
        Ddivu => format!("ctx.ddivu({}, {});", r(s), r(t)),
        // Overflow exceptions are not modelled: ADD/SUB behave like ADDU/SUBU.
        Add | Addu => set(
            d,
            &sx(format!("({} as u32).wrapping_add({} as u32)", r(s), r(t))),
        ),
        Sub | Subu => set(
            d,
            &sx(format!("({} as u32).wrapping_sub({} as u32)", r(s), r(t))),
        ),
        Dadd | Daddu => set(d, &format!("{}.wrapping_add({})", r(s), r(t))),
        Dsub | Dsubu => set(d, &format!("{}.wrapping_sub({})", r(s), r(t))),
        And => set(d, &format!("{} & {}", r(s), r(t))),
        Or => set(d, &format!("{} | {}", r(s), r(t))),
        Xor => set(d, &format!("{} ^ {}", r(s), r(t))),
        Nor => set(d, &format!("!({} | {})", r(s), r(t))),
        Slt => set(
            d,
            &format!("(({} as i64) < ({} as i64)) as u64", r(s), r(t)),
        ),
        Sltu => set(d, &format!("({} < {}) as u64", r(s), r(t))),
        Addi | Addiu => set(t, &sx(format!("({} as u32).wrapping_add({simm32})", r(s)))),
        Daddi | Daddiu => set(t, &format!("{}.wrapping_add({simm64})", r(s))),
        Slti => set(
            t,
            &format!("(({} as i64) < ({simm64} as i64)) as u64", r(s)),
        ),
        Sltiu => set(t, &format!("({} < {simm64}) as u64", r(s))),
        Andi => set(t, &format!("{} & {uimm}", r(s))),
        Ori => set(t, &format!("{} | {uimm}", r(s))),
        Xori => set(t, &format!("{} ^ {uimm}", r(s))),
        Lui => set(t, &sx_lit((i.uimm() as u32) << 16)),
        Lb => set(t, &format!("ctx.mem.read_u8({}) as i8 as u64", addr(i))),
        Lbu => set(t, &format!("ctx.mem.read_u8({}) as u64", addr(i))),
        Lh => set(t, &format!("ctx.mem.read_u16({}) as i16 as u64", addr(i))),
        Lhu => set(t, &format!("ctx.mem.read_u16({}) as u64", addr(i))),
        Lw => set(t, &sx(format!("ctx.mem.read_u32({})", addr(i)))),
        Lwu => set(t, &format!("ctx.mem.read_u32({}) as u64", addr(i))),
        Ld => set(t, &format!("ctx.mem.read_u64({})", addr(i))),
        Lwl => set(t, &format!("ctx.mem.lwl({}, {})", addr(i), r(t))),
        Lwr => set(t, &format!("ctx.mem.lwr({}, {})", addr(i), r(t))),
        Ldl => set(t, &format!("ctx.mem.ldl({}, {})", addr(i), r(t))),
        Ldr => set(t, &format!("ctx.mem.ldr({}, {})", addr(i), r(t))),
        Sb => format!("ctx.mem.write_u8({}, {} as u8);", addr(i), r(t)),
        Sh => format!("ctx.mem.write_u16({}, {} as u16);", addr(i), r(t)),
        Sw => format!("ctx.mem.write_u32({}, {} as u32);", addr(i), r(t)),
        Sd => format!("ctx.mem.write_u64({}, {});", addr(i), r(t)),
        Swl => format!("ctx.mem.swl({}, {});", addr(i), r(t)),
        Swr => format!("ctx.mem.swr({}, {});", addr(i), r(t)),
        Sdl => format!("ctx.mem.sdl({}, {});", addr(i), r(t)),
        Sdr => format!("ctx.mem.sdr({}, {});", addr(i), r(t)),
        // Caches and memory ordering have no effect on the host.
        Cache | Sync => String::new(),
        // COP1 moves, loads and stores. `ft` is the rt field, `fs` the rd field.
        Mfc1 => set(t, &sx(format!("ctx.fw({d})"))),
        Dmfc1 => set(t, &format!("ctx.fl({d})")),
        Cfc1 => set(t, &sx(format!("ctx.cfc1({d})"))),
        Mtc1 => format!("ctx.set_fw({d}, {} as u32);", r(t)),
        Dmtc1 => format!("ctx.set_fl({d}, {});", r(t)),
        Ctc1 => format!("ctx.ctc1({d}, {} as u32);", r(t)),
        Lwc1 => format!("ctx.set_fw({t}, ctx.mem.read_u32({}));", addr(i)),
        Ldc1 => format!("ctx.set_fl({t}, ctx.mem.read_u64({}));", addr(i)),
        Swc1 => format!("ctx.mem.write_u32({}, ctx.fw({t}));", addr(i)),
        Sdc1 => format!("ctx.mem.write_u64({}, ctx.fl({t}));", addr(i)),
        Cop1 => cop1(i, pc),
        Mfc0 => set(t, &format!("ctx.mfc0({d})")),
        Mtc0 => format!("ctx.mtc0({d}, {});", r(t)),
        Tlb => format!("ctx.tlb({});", i.word & 0x3F),
        Syscall | Break | Unknown => {
            format!("ctx.unimplemented({pc:#010x}, {:#010x});", i.word)
        }
        J | Jal | Jr | Jalr | Beq | Bne | Blez | Bgtz | Bltz | Bgez | Bltzl | Bgezl | Bltzal
        | Bgezal | Bltzall | Bgezall | Beql | Bnel | Blezl | Bgtzl | Bc1f | Bc1t | Bc1fl
        | Bc1tl => {
            unreachable!("control flow handled by the block emitter")
        }
    }
}

/// COP1 arithmetic, conversions and compares.
fn cop1(i: &Inst, pc: u32) -> String {
    let fmt = i.rs; // 16 S, 17 D, 20 W, 21 L
    let ft = i.rt;
    let fs = i.rd;
    let fd = i.sa;
    let funct = i.word & 0x3F;
    let unimpl = format!("ctx.unimplemented({pc:#010x}, {:#010x});", i.word);
    // Typed read and write for the S and D formats.
    let (get, put) = match fmt {
        16 => ("fs", "set_fs"),
        17 => ("fd", "set_fd"),
        _ => ("", ""),
    };
    let g = |n: u8| format!("ctx.{get}({n})");
    let p = |e: String| format!("ctx.{put}({fd}, {e});");
    // The source widened to f64 (exact for singles).
    let wide = |n: u8| match fmt {
        16 => format!("(ctx.fs({n}) as f64)"),
        _ => format!("ctx.fd({n})"),
    };
    let float = fmt == 16 || fmt == 17;
    let round = |mode: &str, long: bool| {
        let (f, set) = if long {
            ("cvt_l", "set_fl")
        } else {
            ("cvt_w", "set_fw")
        };
        format!("ctx.{set}({fd}, {RT}::{f}({}, {mode}));", wide(fs))
    };
    match funct {
        0 if float => p(format!("{} + {}", g(fs), g(ft))),
        1 if float => p(format!("{} - {}", g(fs), g(ft))),
        2 if float => p(format!("{} * {}", g(fs), g(ft))),
        3 if float => p(format!("{} / {}", g(fs), g(ft))),
        4 if float => p(format!("{}.sqrt()", g(fs))),
        5 if float => p(format!("{}.abs()", g(fs))),
        6 if fmt == 16 => format!("ctx.set_fw({fd}, ctx.fw({fs}));"),
        6 if fmt == 17 => format!("ctx.set_fl({fd}, ctx.fl({fs}));"),
        7 if float => p(format!("-{}", g(fs))),
        8..=15 if float => {
            let mode = match funct & 3 {
                0 => "Nearest",
                1 => "Zero",
                2 => "Up",
                _ => "Down",
            };
            round(&format!("{RT}::Round::{mode}"), funct < 12)
        }
        // CVT.S
        32 => match fmt {
            17 => format!("ctx.set_fs({fd}, ctx.fd({fs}) as f32);"),
            20 => format!("ctx.set_fs({fd}, ctx.fw({fs}) as i32 as f32);"),
            21 => format!("ctx.set_fs({fd}, ctx.fl({fs}) as i64 as f32);"),
            _ => unimpl,
        },
        // CVT.D
        33 => match fmt {
            16 => format!("ctx.set_fd({fd}, ctx.fs({fs}) as f64);"),
            20 => format!("ctx.set_fd({fd}, ctx.fw({fs}) as i32 as f64);"),
            21 => format!("ctx.set_fd({fd}, ctx.fl({fs}) as i64 as f64);"),
            _ => unimpl,
        },
        // CVT.W and CVT.L use the rounding mode in FCR31.
        36 if float => round(&format!("{RT}::Round::from_fcr31(ctx.fcr31)"), false),
        37 if float => round(&format!("{RT}::Round::from_fcr31(ctx.fcr31)"), true),
        48..=63 if float => format!(
            "ctx.set_c1({RT}::fcmp({}, {}, {}));",
            wide(fs),
            wide(ft),
            funct & 15
        ),
        _ => unimpl,
    }
}

/// Condition of a conditional branch, evaluated before its delay slot.
fn condition(i: &Inst, pc: u32) -> String {
    use Op::*;
    let (s, t) = (r(i.rs), r(i.rt));
    match i.op {
        Beq | Beql => format!("{s} == {t}"),
        Bne | Bnel => format!("{s} != {t}"),
        Blez | Blezl => format!("({s} as i64) <= 0"),
        Bgtz | Bgtzl => format!("({s} as i64) > 0"),
        Bltz | Bltzl | Bltzal | Bltzall => format!("({s} as i64) < 0"),
        Bgez | Bgezl | Bgezal | Bgezall => format!("({s} as i64) >= 0"),
        Bc1t | Bc1tl => "ctx.c1()".to_string(),
        Bc1f | Bc1fl => "!ctx.c1()".to_string(),
        _ => format!("ctx.unimplemented({pc:#010x}, {:#010x})", i.word),
    }
}

struct Emitter<'a> {
    f: &'a Function,
    end: u32,
    out: String,
}

impl Emitter<'_> {
    fn contains(&self, a: u32) -> bool {
        a >= self.f.addr && a < self.end
    }

    fn inst(&self, a: u32) -> Inst {
        Inst::decode(self.f.words[((a - self.f.addr) / 4) as usize])
    }

    fn line(&mut self, indent: usize, s: &str) {
        if !s.is_empty() {
            let _ = writeln!(self.out, "{:indent$}{s}", "", indent = indent * 4);
        }
    }

    fn comment(&mut self, indent: usize, a: u32, i: &Inst) {
        self.line(indent, &format!("// {a:08x}: {:08x} {:?}", i.word, i.op));
    }

    /// Moves control to `target`: inside the function a jump to its block,
    /// outside it a tail call.
    fn goto(&mut self, indent: usize, target: u32) {
        if self.contains(target) {
            self.line(indent, &format!("pc = {target:#010x}; continue;"));
        } else {
            self.line(indent, &format!("ctx.call({target:#010x}); return;"));
        }
    }

    fn delay_slot(&mut self, indent: usize, a: u32) -> Result<(), TranslateError> {
        if !self.contains(a) {
            return Err(TranslateError::new(
                a,
                "delay slot is past the function end",
            ));
        }
        let i = self.inst(a);
        if i.flow() != Flow::Next {
            return Err(TranslateError::new(a, "control transfer in a delay slot"));
        }
        self.comment(indent, a, &i);
        self.line(indent, &simple(&i, a));
        Ok(())
    }

    fn block(&mut self, start: u32, starts: &BTreeSet<u32>) -> Result<(), TranslateError> {
        self.line(3, &format!("{start:#010x} => {{"));
        let mut a = start;
        loop {
            if a >= self.end {
                // Ran off the end of the function: continue in whatever follows.
                self.goto(4, a);
                break;
            }
            let i = self.inst(a);
            self.comment(4, a, &i);
            let ret = a.wrapping_add(8);
            match i.flow() {
                Flow::Next => {
                    let code = simple(&i, a);
                    self.line(4, &code);
                    a += 4;
                    if starts.contains(&a) {
                        self.line(4, &format!("pc = {a:#010x}; continue;"));
                        break;
                    }
                    continue;
                }
                Flow::Branch { likely, link } => {
                    let target = i.branch_target(a);
                    self.line(4, &format!("let take: bool = {};", condition(&i, a)));
                    if link {
                        self.line(4, &format!("ctx.r[31] = {};", sx_lit(ret)));
                    }
                    if likely {
                        self.line(4, "if take {");
                        self.delay_slot(5, a + 4)?;
                        self.goto(5, target);
                        self.line(4, "}");
                    } else {
                        self.delay_slot(4, a + 4)?;
                        self.line(4, "if take {");
                        self.goto(5, target);
                        self.line(4, "}");
                    }
                    self.goto(4, ret);
                }
                Flow::Jump { link: false } => {
                    self.delay_slot(4, a + 4)?;
                    self.goto(4, i.jump_target(a));
                }
                Flow::Jump { link: true } => {
                    self.line(4, &format!("ctx.r[31] = {};", sx_lit(ret)));
                    self.delay_slot(4, a + 4)?;
                    self.line(4, &format!("ctx.call({:#010x});", i.jump_target(a)));
                    self.goto(4, ret);
                }
                Flow::JumpReg { link } => {
                    self.line(4, &format!("let target = {} as u32;", r(i.rs)));
                    if link {
                        self.line(4, &set(i.rd, &sx_lit(ret)));
                    }
                    self.delay_slot(4, a + 4)?;
                    if link {
                        self.line(4, "ctx.call(target);");
                        self.goto(4, ret);
                    } else if i.rs == 31 {
                        self.line(4, "let _ = target; return;");
                    } else {
                        // Jump tables and other register jumps: dispatched by
                        // the default arm below.
                        self.line(4, "pc = target; continue;");
                    }
                }
            }
            break;
        }
        self.line(3, "}");
        Ok(())
    }
}

/// Block start addresses of `f`.
fn block_starts(f: &Function, end: u32) -> BTreeSet<u32> {
    let mut starts = BTreeSet::new();
    starts.insert(f.addr);
    let inside = |a: u32| a >= f.addr && a < end;
    for &t in &f.jump_targets {
        if inside(t) && t % 4 == 0 {
            starts.insert(t);
        }
    }
    for (n, &w) in f.words.iter().enumerate() {
        let a = f.addr + 4 * n as u32;
        let i = Inst::decode(w);
        let after = a.wrapping_add(8);
        match i.flow() {
            Flow::Next => continue,
            Flow::Branch { .. } => {
                let t = i.branch_target(a);
                if inside(t) {
                    starts.insert(t);
                }
            }
            Flow::Jump { link: false } => {
                let t = i.jump_target(a);
                if inside(t) {
                    starts.insert(t);
                }
            }
            Flow::Jump { link: true } | Flow::JumpReg { .. } => {}
        }
        if inside(after) {
            starts.insert(after);
        }
    }
    starts
}

/// Rust source for one guest function.
pub fn function(f: &Function) -> Result<String, TranslateError> {
    if f.addr % 4 != 0 {
        return Err(TranslateError::new(
            f.addr,
            "function address is not word aligned",
        ));
    }
    let end = f
        .addr
        .checked_add(4 * f.words.len() as u32)
        .ok_or_else(|| TranslateError::new(f.addr, "function wraps the address space"))?;
    let starts = block_starts(f, end);
    let mut e = Emitter {
        f,
        end,
        out: String::new(),
    };
    e.line(0, &format!("/// Guest function at {:#010x}.", f.addr));
    if let Some(name) = &f.name {
        e.line(0, &format!("/// Symbol: `{name}`."));
    }
    e.line(
        0,
        "#[allow(unused_mut, unused_variables, unused_assignments, unused_comparisons, unused_parens, unreachable_code, clippy::all)]",
    );
    e.line(
        0,
        &format!("pub fn {}(ctx: &mut {RT}::Ctx) {{", fn_name(f.addr)),
    );
    e.line(1, &format!("let mut pc: u32 = {:#010x};", f.addr));
    e.line(1, "loop {");
    e.line(2, "match pc {");
    for &s in &starts {
        e.block(s, &starts)?;
    }
    e.line(3, "_ => {");
    e.line(
        4,
        &format!("if pc < {:#010x} || pc >= {end:#010x} {{", f.addr),
    );
    e.line(5, "ctx.call(pc);");
    e.line(5, "return;");
    e.line(4, "}");
    e.line(4, &format!("{RT}::bad_pc({:#010x}, pc);", f.addr));
    e.line(3, "}");
    e.line(2, "}");
    e.line(1, "}");
    e.line(0, "}");
    Ok(e.out)
}

#[cfg(test)]
mod tests {
    use super::{block_starts, function};
    use crate::Function;

    const BASE: u32 = 0x8000_1000;
    const NOP: u32 = 0;
    const JR_RA: u32 = 0x03E0_0008;

    /// `beq $0, $0` at `pc` to `target` (always taken).
    fn b(pc: u32, target: u32) -> u32 {
        let off = (target.wrapping_sub(pc + 4) as i32) >> 2;
        0x1000_0000 | (off as u32 & 0xFFFF)
    }

    /// `bne $a0, $0` at `pc` to `target` (taken only when $a0 is not zero).
    fn bnez_a0(pc: u32, target: u32) -> u32 {
        let off = (target.wrapping_sub(pc + 4) as i32) >> 2;
        0x1480_0000 | (off as u32 & 0xFFFF)
    }

    /// `j target`.
    fn j(target: u32) -> u32 {
        0x0800_0000 | (target >> 2 & 0x03FF_FFFF)
    }

    fn end(f: &Function) -> u32 {
        f.addr + 4 * f.words.len() as u32
    }

    fn starts(f: &Function) -> Vec<u32> {
        block_starts(f, end(f)).into_iter().collect()
    }

    #[test]
    fn a_jump_out_of_the_function_starts_no_block_there() {
        const OUTSIDE: u32 = 0x8000_2000;
        let f = Function::new(BASE, vec![j(OUTSIDE), NOP, JR_RA, NOP]);
        assert_eq!(starts(&f), [BASE, BASE + 8]);
        let src = function(&f).unwrap();
        assert!(src.contains("ctx.call(0x80002000); return;"), "{src}");
    }

    #[test]
    fn a_branch_out_of_the_function_starts_no_block_there() {
        // A branch back to just before the entry, and one to the first word
        // past the end.
        let f = Function::new(
            BASE,
            vec![
                b(BASE, BASE - 4),
                NOP,
                b(BASE + 8, BASE + 0x18),
                NOP,
                JR_RA,
                NOP,
            ],
        );
        assert_eq!(starts(&f), [BASE, BASE + 8, BASE + 0x10]);
    }

    #[test]
    fn analysis_targets_outside_or_unaligned_start_no_block() {
        let mut f = Function::new(BASE, vec![NOP, NOP, JR_RA, NOP]);
        let past_end = end(&f);
        f.jump_targets = vec![BASE - 4, past_end, BASE + 6, BASE + 4];
        assert_eq!(starts(&f), [BASE, BASE + 4]);
    }

    #[test]
    fn a_branch_whose_delay_slot_is_the_last_word_falls_through_by_a_call() {
        // nop ; bne $a0, $0, BASE ; nop (delay slot, last word)
        let f = Function::new(BASE, vec![NOP, bnez_a0(BASE + 4, BASE), NOP]);
        assert_eq!(starts(&f), [BASE], "no block starts past the end");
        let src = function(&f).unwrap();
        // Not taken, control goes on in whatever follows the function.
        assert!(src.contains("ctx.call(0x8000100c); return;"), "{src}");
    }

    #[test]
    fn a_branch_in_the_last_word_is_refused_for_its_missing_delay_slot() {
        let f = Function::new(BASE, vec![NOP, b(BASE + 4, BASE)]);
        assert_eq!(starts(&f), [BASE]);
        let e = function(&f).unwrap_err();
        assert_eq!(e.addr, BASE + 8);
        assert_eq!(e.msg, "delay slot is past the function end");
    }

    #[test]
    fn a_jump_in_the_last_word_is_refused_for_its_missing_delay_slot() {
        let f = Function::new(BASE, vec![NOP, j(BASE)]);
        let e = function(&f).unwrap_err();
        assert_eq!(e.addr, BASE + 8);
        assert_eq!(e.msg, "delay slot is past the function end");
    }
}
