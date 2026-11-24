//! Decoder for the VR4300 integer instruction set (MIPS III).

/// Instruction mnemonics the translator knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Op {
    // SPECIAL
    Sll,
    Srl,
    Sra,
    Sllv,
    Srlv,
    Srav,
    Jr,
    Jalr,
    Syscall,
    Break,
    Sync,
    Mfhi,
    Mthi,
    Mflo,
    Mtlo,
    Dsllv,
    Dsrlv,
    Dsrav,
    Mult,
    Multu,
    Div,
    Divu,
    Dmult,
    Dmultu,
    Ddiv,
    Ddivu,
    Add,
    Addu,
    Sub,
    Subu,
    And,
    Or,
    Xor,
    Nor,
    Slt,
    Sltu,
    Dadd,
    Daddu,
    Dsub,
    Dsubu,
    Dsll,
    Dsrl,
    Dsra,
    Dsll32,
    Dsrl32,
    Dsra32,
    // REGIMM
    Bltz,
    Bgez,
    Bltzl,
    Bgezl,
    Bltzal,
    Bgezal,
    Bltzall,
    Bgezall,
    // Main opcodes
    J,
    Jal,
    Beq,
    Bne,
    Blez,
    Bgtz,
    Addi,
    Addiu,
    Slti,
    Sltiu,
    Andi,
    Ori,
    Xori,
    Lui,
    Beql,
    Bnel,
    Blezl,
    Bgtzl,
    Daddi,
    Daddiu,
    Ldl,
    Ldr,
    Lb,
    Lh,
    Lwl,
    Lw,
    Lbu,
    Lhu,
    Lwr,
    Lwu,
    Sb,
    Sh,
    Swl,
    Sw,
    Sdl,
    Sdr,
    Swr,
    Cache,
    Ld,
    Sd,
    // COP1 condition branches. Decoded so that blocks and delay slots are
    // laid out correctly; the condition itself is not implemented yet.
    Bc1f,
    Bc1t,
    Bc1fl,
    Bc1tl,
    // COP1 moves, loads and stores.
    Mfc1,
    Dmfc1,
    Cfc1,
    Mtc1,
    Dmtc1,
    Ctc1,
    Lwc1,
    Ldc1,
    Swc1,
    Sdc1,
    /// COP1 arithmetic, conversion or compare: format in `rs`, operation in
    /// the funct field.
    Cop1,
    /// Anything not decoded yet (coprocessors, traps, LL/SC, ...).
    Unknown,
}

/// How an instruction changes control flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Falls through to the next instruction.
    Next,
    /// Conditional branch with a delay slot.
    Branch {
        /// "Likely": the delay slot runs only when the branch is taken.
        likely: bool,
        /// Writes the return address to `$ra` whether taken or not.
        link: bool,
    },
    /// J or JAL to an absolute target.
    Jump { link: bool },
    /// JR or JALR to a register.
    JumpReg { link: bool },
}

/// One decoded instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inst {
    pub word: u32,
    pub op: Op,
    pub rs: u8,
    pub rt: u8,
    pub rd: u8,
    pub sa: u8,
}

impl Inst {
    /// Decodes one big-endian instruction word.
    pub fn decode(word: u32) -> Self {
        let rs = ((word >> 21) & 31) as u8;
        let rt = ((word >> 16) & 31) as u8;
        let rd = ((word >> 11) & 31) as u8;
        let sa = ((word >> 6) & 31) as u8;
        let op = match word >> 26 {
            0x00 => match word & 0x3F {
                0x00 => Op::Sll,
                0x02 => Op::Srl,
                0x03 => Op::Sra,
                0x04 => Op::Sllv,
                0x06 => Op::Srlv,
                0x07 => Op::Srav,
                0x08 => Op::Jr,
                0x09 => Op::Jalr,
                0x0C => Op::Syscall,
                0x0D => Op::Break,
                0x0F => Op::Sync,
                0x10 => Op::Mfhi,
                0x11 => Op::Mthi,
                0x12 => Op::Mflo,
                0x13 => Op::Mtlo,
                0x14 => Op::Dsllv,
                0x16 => Op::Dsrlv,
                0x17 => Op::Dsrav,
                0x18 => Op::Mult,
                0x19 => Op::Multu,
                0x1A => Op::Div,
                0x1B => Op::Divu,
                0x1C => Op::Dmult,
                0x1D => Op::Dmultu,
                0x1E => Op::Ddiv,
                0x1F => Op::Ddivu,
                0x20 => Op::Add,
                0x21 => Op::Addu,
                0x22 => Op::Sub,
                0x23 => Op::Subu,
                0x24 => Op::And,
                0x25 => Op::Or,
                0x26 => Op::Xor,
                0x27 => Op::Nor,
                0x2A => Op::Slt,
                0x2B => Op::Sltu,
                0x2C => Op::Dadd,
                0x2D => Op::Daddu,
                0x2E => Op::Dsub,
                0x2F => Op::Dsubu,
                0x38 => Op::Dsll,
                0x3A => Op::Dsrl,
                0x3B => Op::Dsra,
                0x3C => Op::Dsll32,
                0x3E => Op::Dsrl32,
                0x3F => Op::Dsra32,
                _ => Op::Unknown,
            },
            0x01 => match rt {
                0x00 => Op::Bltz,
                0x01 => Op::Bgez,
                0x02 => Op::Bltzl,
                0x03 => Op::Bgezl,
                0x10 => Op::Bltzal,
                0x11 => Op::Bgezal,
                0x12 => Op::Bltzall,
                0x13 => Op::Bgezall,
                _ => Op::Unknown,
            },
            0x11 => match rs {
                0x00 => Op::Mfc1,
                0x01 => Op::Dmfc1,
                0x02 => Op::Cfc1,
                0x04 => Op::Mtc1,
                0x05 => Op::Dmtc1,
                0x06 => Op::Ctc1,
                0x08 => match rt & 3 {
                    0 => Op::Bc1f,
                    1 => Op::Bc1t,
                    2 => Op::Bc1fl,
                    _ => Op::Bc1tl,
                },
                0x10 | 0x11 | 0x14 | 0x15 => Op::Cop1,
                _ => Op::Unknown,
            },
            0x31 => Op::Lwc1,
            0x35 => Op::Ldc1,
            0x39 => Op::Swc1,
            0x3D => Op::Sdc1,
            0x02 => Op::J,
            0x03 => Op::Jal,
            0x04 => Op::Beq,
            0x05 => Op::Bne,
            0x06 => Op::Blez,
            0x07 => Op::Bgtz,
            0x08 => Op::Addi,
            0x09 => Op::Addiu,
            0x0A => Op::Slti,
            0x0B => Op::Sltiu,
            0x0C => Op::Andi,
            0x0D => Op::Ori,
            0x0E => Op::Xori,
            0x0F => Op::Lui,
            0x14 => Op::Beql,
            0x15 => Op::Bnel,
            0x16 => Op::Blezl,
            0x17 => Op::Bgtzl,
            0x18 => Op::Daddi,
            0x19 => Op::Daddiu,
            0x1A => Op::Ldl,
            0x1B => Op::Ldr,
            0x20 => Op::Lb,
            0x21 => Op::Lh,
            0x22 => Op::Lwl,
            0x23 => Op::Lw,
            0x24 => Op::Lbu,
            0x25 => Op::Lhu,
            0x26 => Op::Lwr,
            0x27 => Op::Lwu,
            0x28 => Op::Sb,
            0x29 => Op::Sh,
            0x2A => Op::Swl,
            0x2B => Op::Sw,
            0x2C => Op::Sdl,
            0x2D => Op::Sdr,
            0x2E => Op::Swr,
            0x2F => Op::Cache,
            0x37 => Op::Ld,
            0x3F => Op::Sd,
            _ => Op::Unknown,
        };
        Inst {
            word,
            op,
            rs,
            rt,
            rd,
            sa,
        }
    }

    /// The 16-bit immediate, zero-extended.
    pub fn uimm(&self) -> u16 {
        self.word as u16
    }

    /// The 16-bit immediate, sign-extended.
    pub fn simm(&self) -> i64 {
        self.word as u16 as i16 as i64
    }

    /// Target of a conditional branch at `pc`.
    pub fn branch_target(&self, pc: u32) -> u32 {
        pc.wrapping_add(4).wrapping_add((self.simm() << 2) as u32)
    }

    /// Target of J/JAL at `pc`: inside the 256 MiB region of the delay slot.
    pub fn jump_target(&self, pc: u32) -> u32 {
        (pc.wrapping_add(4) & 0xF000_0000) | ((self.word & 0x03FF_FFFF) << 2)
    }

    /// The general-purpose register this instruction writes, if any
    /// (ignoring `$ra` written by linking branches and jumps).
    pub fn dest(&self) -> Option<u8> {
        use Op::*;
        let d = match self.op {
            Sll | Srl | Sra | Sllv | Srlv | Srav | Jalr | Mfhi | Mflo | Dsllv | Dsrlv | Dsrav
            | Add | Addu | Sub | Subu | And | Or | Xor | Nor | Slt | Sltu | Dadd | Daddu | Dsub
            | Dsubu | Dsll | Dsrl | Dsra | Dsll32 | Dsrl32 | Dsra32 => self.rd,
            Addi | Addiu | Slti | Sltiu | Andi | Ori | Xori | Lui | Daddi | Daddiu | Ldl | Ldr
            | Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Lwu | Ld | Mfc1 | Dmfc1 | Cfc1 => self.rt,
            // COP0 moves such as MFC0 write rt; be conservative.
            Unknown if self.word >> 26 == 0x10 => self.rt,
            _ => return None,
        };
        (d != 0).then_some(d)
    }

    /// How this instruction changes control flow.
    pub fn flow(&self) -> Flow {
        use Op::*;
        match self.op {
            Beq | Bne | Blez | Bgtz | Bltz | Bgez => Flow::Branch {
                likely: false,
                link: false,
            },
            Bc1f | Bc1t => Flow::Branch {
                likely: false,
                link: false,
            },
            Beql | Bnel | Blezl | Bgtzl | Bltzl | Bgezl | Bc1fl | Bc1tl => Flow::Branch {
                likely: true,
                link: false,
            },
            Bltzal | Bgezal => Flow::Branch {
                likely: false,
                link: true,
            },
            Bltzall | Bgezall => Flow::Branch {
                likely: true,
                link: true,
            },
            J => Flow::Jump { link: false },
            Jal => Flow::Jump { link: true },
            Jr => Flow::JumpReg { link: false },
            Jalr => Flow::JumpReg { link: true },
            _ => Flow::Next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_common_forms() {
        // addiu $sp, $sp, -0x18
        let i = Inst::decode(0x27BD_FFE8);
        assert_eq!((i.op, i.rs, i.rt, i.simm()), (Op::Addiu, 29, 29, -0x18));
        // jr $ra
        let i = Inst::decode(0x03E0_0008);
        assert_eq!((i.op, i.rs), (Op::Jr, 31));
        assert_eq!(i.flow(), Flow::JumpReg { link: false });
        // nop is sll $0, $0, 0
        assert_eq!(Inst::decode(0).op, Op::Sll);
        // bgezal $4, ...
        let i = Inst::decode(0x0491_0003);
        assert_eq!(i.op, Op::Bgezal);
        assert_eq!(i.branch_target(0x8000_1000), 0x8000_1010);
    }

    #[test]
    fn jump_target_uses_delay_slot_region() {
        // jal 0x80001234
        let i = Inst::decode(0x0C00_048D);
        assert_eq!(i.op, Op::Jal);
        assert_eq!(i.jump_target(0x8000_0000), 0x8000_1234);
    }
}
