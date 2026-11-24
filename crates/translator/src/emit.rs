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
        "#[allow(unused_mut, unused_variables, unused_assignments, unused_comparisons, unreachable_code, clippy::all)]",
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
