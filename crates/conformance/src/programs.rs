//! Synthetic test programs. Shared by the build script (which translates
//! them) and the tests (which run them on the reference CPU).

use crate::asm::*;

/// Where the program under test starts.
pub const CODE: u32 = 0x8000_1000;
/// Where a second function (a callee) starts.
pub const CALLEE: u32 = 0x8000_2000;
/// Base register value for loads and stores; the data area is
/// [`DATA_SPAN`] bytes centred on it, `DATA - 0x800 .. DATA + 0x800`, and is
/// filled with random bytes.
pub const DATA: u32 = 0x8000_8800;
/// Bytes of the random data area around [`DATA`].
pub const DATA_SPAN: u32 = 0x1000;
/// The return address planted in `$ra`; reaching it ends the run.
pub const SENTINEL: u32 = 0x8000_0F00;
/// RAM size used by the tests.
pub const RAM: usize = 0x1_0000;

/// One synthetic program, translated by the build script and run by the tests.
pub struct Program {
    /// Unique name, used in failure messages and to pair the program with its
    /// translation.
    pub name: String,
    /// Functions as (address, words). The first is the entry point.
    pub funcs: Vec<(u32, Vec<u32>)>,
    /// Registers given fixed values (sign-extended) instead of random ones.
    pub fixed: Vec<(u32, u32)>,
}

fn single(name: String, body: Vec<u32>) -> Program {
    let mut words = body;
    words.extend([jr(RA), NOP]);
    Program {
        name,
        funcs: vec![(CODE, words)],
        fixed: vec![],
    }
}

fn mem(name: String, body: Vec<u32>) -> Program {
    let mut p = single(name, body);
    p.fixed.push((4, DATA));
    p
}

/// Every synthetic program, in the order the build script translates them.
pub fn all() -> Vec<Program> {
    let mut v = Vec::new();

    // Three-register ALU and variable shifts, including writes to $zero and
    // a destination that is also a source.
    let three = [
        ("sllv", 0x04),
        ("srlv", 0x06),
        ("srav", 0x07),
        ("dsllv", 0x14),
        ("dsrlv", 0x16),
        ("dsrav", 0x17),
        ("add", 0x20),
        ("addu", 0x21),
        ("sub", 0x22),
        ("subu", 0x23),
        ("and", 0x24),
        ("or", 0x25),
        ("xor", 0x26),
        ("nor", 0x27),
        ("slt", 0x2A),
        ("sltu", 0x2B),
        ("dadd", 0x2C),
        ("daddu", 0x2D),
        ("dsub", 0x2E),
        ("dsubu", 0x2F),
    ];
    for (name, funct) in three {
        for (d, s, t) in [(2, 4, 5), (4, 4, 5), (5, 4, 5), (0, 4, 5), (3, 6, 6)] {
            v.push(single(
                format!("{name} ${d}, ${s}, ${t}"),
                vec![rtype(funct, s, t, d, 0)],
            ));
        }
    }

    // Shifts by an immediate.
    let shifts = [
        ("sll", 0x00),
        ("srl", 0x02),
        ("sra", 0x03),
        ("dsll", 0x38),
        ("dsrl", 0x3A),
        ("dsra", 0x3B),
        ("dsll32", 0x3C),
        ("dsrl32", 0x3E),
        ("dsra32", 0x3F),
    ];
    for (name, funct) in shifts {
        for sa in [0, 1, 4, 16, 31] {
            v.push(single(
                format!("{name} $2, $5, {sa}"),
                vec![rtype(funct, 0, 5, 2, sa)],
            ));
        }
    }

    // Multiply and divide, read back through HI and LO.
    let muldiv = [
        ("mult", 0x18),
        ("multu", 0x19),
        ("div", 0x1A),
        ("divu", 0x1B),
        ("dmult", 0x1C),
        ("dmultu", 0x1D),
        ("ddiv", 0x1E),
        ("ddivu", 0x1F),
    ];
    for (name, funct) in muldiv {
        for (s, t) in [(4, 5), (5, 4), (6, 6), (4, 0)] {
            v.push(single(
                format!("{name} ${s}, ${t}"),
                vec![
                    rtype(funct, s, t, 0, 0),
                    rtype(0x10, 0, 0, 2, 0),
                    rtype(0x12, 0, 0, 3, 0),
                ],
            ));
        }
    }
    v.push(single(
        "mthi/mtlo".into(),
        vec![rtype(0x11, 4, 0, 0, 0), rtype(0x13, 5, 0, 0, 0)],
    ));

    // Immediate ALU.
    let imms = [
        ("addi", 0x08),
        ("addiu", 0x09),
        ("slti", 0x0A),
        ("sltiu", 0x0B),
        ("andi", 0x0C),
        ("ori", 0x0D),
        ("xori", 0x0E),
        ("lui", 0x0F),
        ("daddi", 0x18),
        ("daddiu", 0x19),
    ];
    for (name, op) in imms {
        for imm in [0, 1, -1, 0x7FFF, -0x8000, 0x1234, -0x1234] {
            for (t, s) in [(2, 4), (4, 4), (0, 4)] {
                v.push(single(
                    format!("{name} ${t}, ${s}, {imm}"),
                    vec![itype(op, s, t, imm)],
                ));
            }
        }
    }

    // Aligned loads and stores.
    let loads = [
        ("lb", 0x20, 1),
        ("lh", 0x21, 2),
        ("lw", 0x23, 4),
        ("lbu", 0x24, 1),
        ("lhu", 0x25, 2),
        ("lwu", 0x27, 4),
        ("ld", 0x37, 8),
        ("sb", 0x28, 1),
        ("sh", 0x29, 2),
        ("sw", 0x2B, 4),
        ("sd", 0x3F, 8),
    ];
    for (name, op, size) in loads {
        for k in [0, 1, -3, 17, -100] {
            let imm = k * size;
            for t in [2, 0, 4] {
                v.push(mem(
                    format!("{name} ${t}, {imm}($4)"),
                    vec![itype(op, 4, t, imm)],
                ));
            }
        }
    }

    // Unaligned loads and stores, every byte offset.
    let unaligned = [
        ("lwl", 0x22),
        ("lwr", 0x26),
        ("ldl", 0x1A),
        ("ldr", 0x1B),
        ("swl", 0x2A),
        ("swr", 0x2E),
        ("sdl", 0x2C),
        ("sdr", 0x2D),
    ];
    for (name, op) in unaligned {
        for imm in [0, 1, 2, 3, 4, 5, 6, 7, -1, -6] {
            v.push(mem(
                format!("{name} $2, {imm}($4)"),
                vec![itype(op, 4, 2, imm)],
            ));
        }
    }
    for off in 0..8 {
        // The usual pairs for an unaligned word and doubleword.
        v.push(mem(
            format!("lwl/lwr pair at +{off}"),
            vec![itype(0x22, 4, 2, off), itype(0x26, 4, 2, off + 3)],
        ));
        v.push(mem(
            format!("ldl/ldr pair at +{off}"),
            vec![itype(0x1A, 4, 2, off), itype(0x1B, 4, 2, off + 7)],
        ));
        v.push(mem(
            format!("swl/swr pair at +{off}"),
            vec![itype(0x2A, 4, 5, off), itype(0x2E, 4, 5, off + 3)],
        ));
        v.push(mem(
            format!("sdl/sdr pair at +{off}"),
            vec![itype(0x2C, 4, 5, off), itype(0x2D, 4, 5, off + 7)],
        ));
    }
    v.push(mem(
        "cache and sync".into(),
        vec![itype(0x2F, 4, 0, 0), 0x0F],
    ));

    // Conditional branches. The delay slot changes the register the
    // condition reads, so reading it after the delay slot would show.
    // $ra is saved in $9 first because the linking forms overwrite it.
    type Encode = fn(i32) -> u32;
    let branches: [(&str, Encode); 20] = [
        ("beq", |o| itype(0x04, 4, 5, o)),
        ("bne", |o| itype(0x05, 4, 5, o)),
        ("blez", |o| itype(0x06, 4, 0, o)),
        ("bgtz", |o| itype(0x07, 4, 0, o)),
        ("beql", |o| itype(0x14, 4, 5, o)),
        ("bnel", |o| itype(0x15, 4, 5, o)),
        ("blezl", |o| itype(0x16, 4, 0, o)),
        ("bgtzl", |o| itype(0x17, 4, 0, o)),
        ("bltz", |o| regimm(0x00, 4, o)),
        ("bgez", |o| regimm(0x01, 4, o)),
        ("bltzl", |o| regimm(0x02, 4, o)),
        ("bgezl", |o| regimm(0x03, 4, o)),
        ("bltzal", |o| regimm(0x10, 4, o)),
        ("bgezal", |o| regimm(0x11, 4, o)),
        ("bltzall", |o| regimm(0x12, 4, o)),
        ("bgezall", |o| regimm(0x13, 4, o)),
        ("beq $0,$0", |o| beq(0, 0, o)),
        ("bne $0,$0", |o| bne(0, 0, o)),
        ("beq same reg", |o| beq(4, 4, o)),
        ("bnel same reg", |o| itype(0x15, 4, 4, o)),
    ];
    for (name, br) in &branches {
        for (suffix, fixed) in [("", vec![]), (" (equal)", vec![(4, 0x1234), (5, 0x1234)])] {
            v.push(Program {
                name: format!("{name}{suffix}"),
                funcs: vec![(
                    CODE,
                    vec![
                        or(9, RA, 0),
                        br(3),
                        addiu(4, 4, 1),
                        addiu(6, 0, 7),
                        addiu(7, 0, 8),
                        addiu(8, 0, 9),
                        jr(9),
                        NOP,
                    ],
                )],
                fixed,
            });
        }
    }
    // A branch that leaves the function is a tail call.
    v.push(Program {
        name: "branch out of function".into(),
        funcs: vec![
            (
                CODE,
                vec![
                    bne(4, 5, (CALLEE as i32 - CODE as i32 - 4) / 4),
                    addiu(2, 0, 1),
                    jr(RA),
                    NOP,
                ],
            ),
            (CALLEE, vec![addiu(3, 0, 2), jr(RA), NOP]),
        ],
        fixed: vec![],
    });

    // A counted loop whose counter is decremented in the delay slot.
    v.push(single(
        "loop with delay slot decrement".into(),
        vec![
            addiu(4, 0, 10),
            addiu(5, 5, 3),
            bne(4, 0, -2),
            addiu(4, 4, -1),
        ],
    ));

    // A branch target that is another branch's delay slot.
    for (suffix, fixed) in [("", vec![]), (" (equal)", vec![(4, 7), (5, 7)])] {
        v.push(single(
            format!("branch into a delay slot{suffix}"),
            vec![
                beq(4, 5, 2),
                addiu(6, 0, 1),
                bne(0, 4, 2),
                addiu(7, 7, 2),
                addiu(8, 0, 3),
            ],
        ));
        v.last_mut().unwrap().fixed = fixed;
    }

    // Jumps.
    v.push(single(
        "j forward".into(),
        vec![
            j(CODE + 16),
            addiu(2, 0, 1),
            addiu(3, 0, 1),
            addiu(5, 0, 1),
            addiu(6, 0, 1),
        ],
    ));
    let callee = vec![addiu(2, 2, 5), sll(3, 2, 1), jr(RA), addiu(4, 0, 3)];
    v.push(Program {
        name: "jal with delay slot and return".into(),
        funcs: vec![
            (
                CODE,
                vec![
                    or(16, RA, 0),
                    jal(CALLEE),
                    addiu(2, 0, 1),
                    addu(5, 2, 3),
                    jr(16),
                    addu(6, 31, 0),
                ],
            ),
            (CALLEE, callee.clone()),
        ],
        fixed: vec![],
    });
    v.push(Program {
        name: "jalr through a register".into(),
        funcs: vec![
            (
                CODE,
                vec![
                    or(16, RA, 0),
                    jalr(RA, 8),
                    addiu(8, 0, 0),
                    addu(5, 2, 3),
                    jr(16),
                    NOP,
                ],
            ),
            (CALLEE, callee.clone()),
        ],
        fixed: vec![(8, CALLEE)],
    });
    v.push(Program {
        name: "j as a tail call".into(),
        funcs: vec![(CODE, vec![j(CALLEE), addiu(2, 0, 2)]), (CALLEE, callee)],
        fixed: vec![],
    });

    // Floating point. Doubles use even registers (odd ones are undefined
    // for doubles when FR is clear); singles also use odd registers.
    let arith = [
        ("add", 0),
        ("sub", 1),
        ("mul", 2),
        ("div", 3),
        ("sqrt", 4),
        ("abs", 5),
        ("mov", 6),
        ("neg", 7),
        ("round.l", 8),
        ("trunc.l", 9),
        ("ceil.l", 10),
        ("floor.l", 11),
        ("round.w", 12),
        ("trunc.w", 13),
        ("ceil.w", 14),
        ("floor.w", 15),
        ("cvt.w", 36),
        ("cvt.l", 37),
    ];
    for (name, funct) in arith {
        for (fmt, suffix) in [(16, "s"), (17, "d")] {
            let regs: &[(u32, u32, u32)] = if fmt == 16 {
                &[(0, 2, 4), (2, 2, 4), (1, 3, 5), (6, 7, 6)]
            } else {
                &[(0, 2, 4), (2, 2, 4), (6, 8, 6)]
            };
            for &(fd, fs, ft) in regs {
                // Results of .l conversions are doubles: keep them even.
                if ((8..=11).contains(&funct) || funct == 37) && fd % 2 == 1 {
                    continue;
                }
                v.push(single(
                    format!("{name}.{suffix} $f{fd}, $f{fs}, $f{ft}"),
                    vec![cop1(fmt, ft, fs, fd, funct)],
                ));
            }
        }
    }
    let converts = [
        ("cvt.s.d", 17, 32),
        ("cvt.s.w", 20, 32),
        ("cvt.s.l", 21, 32),
        ("cvt.d.s", 16, 33),
        ("cvt.d.w", 20, 33),
        ("cvt.d.l", 21, 33),
    ];
    for (name, fmt, funct) in converts {
        for (fd, fs) in [(0, 2), (2, 2), (4, 6)] {
            v.push(single(
                format!("{name} $f{fd}, $f{fs}"),
                vec![cop1(fmt, 0, fs, fd, funct)],
            ));
        }
        if fmt == 16 || fmt == 20 {
            v.push(single(
                format!("{name} $f1, $f3"),
                vec![cop1(fmt, 0, 3, if funct == 32 { 1 } else { 0 }, funct)],
            ));
        }
    }
    for cond in 0..16 {
        for (fmt, suffix) in [(16, "s"), (17, "d")] {
            for (fs, ft) in [(2, 4), (4, 4)] {
                v.push(single(
                    format!("c.{cond}.{suffix} $f{fs}, $f{ft}"),
                    vec![cop1(fmt, ft, fs, 0, 48 + cond)],
                ));
            }
        }
    }
    for (name, kind) in [("bc1f", 0), ("bc1t", 1), ("bc1fl", 2), ("bc1tl", 3)] {
        // Directly on the random condition bit, and after a compare.
        for pre in [vec![], vec![cop1(16, 4, 2, 0, 60)]] {
            let mut body = pre.clone();
            body.extend([bc1(kind, 2), addiu(2, 2, 1), addiu(3, 0, 7), addiu(5, 0, 9)]);
            let label = if pre.is_empty() { "" } else { " after c.lt.s" };
            v.push(single(format!("{name}{label}"), body));
        }
    }
    for (name, sub, regs) in [
        ("mfc1", 0, &[(2u32, 3u32), (2, 4), (0, 5)][..]),
        ("dmfc1", 1, &[(2, 4), (3, 6)][..]),
        ("cfc1", 2, &[(2, 31), (3, 0)][..]),
        ("mtc1", 4, &[(4, 3), (4, 6)][..]),
        ("dmtc1", 5, &[(4, 6), (5, 0)][..]),
        ("ctc1", 6, &[(4, 31)][..]),
    ] {
        for &(rt, fs) in regs {
            v.push(single(
                format!("{name} ${rt}, $f{fs}"),
                vec![cop1_move(sub, rt, fs)],
            ));
        }
    }
    // ctc1 then a rounding conversion that reads the new mode.
    v.push(single(
        "ctc1 then cvt.w.s".into(),
        vec![cop1_move(6, 4, 31), NOP, cop1(16, 0, 2, 0, 36)],
    ));
    for (name, op, size) in [
        ("lwc1", 0x31, 4),
        ("ldc1", 0x35, 8),
        ("swc1", 0x39, 4),
        ("sdc1", 0x3D, 8),
    ] {
        for k in [0, 1, -5] {
            for ft in [2, 3, 6] {
                if size == 8 && ft % 2 == 1 {
                    continue;
                }
                v.push(mem(
                    format!("{name} $f{ft}, {}($4)", k * size),
                    vec![itype(op, 4, ft, k * size)],
                ));
            }
        }
    }

    v
}
