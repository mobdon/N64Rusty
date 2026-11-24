//! A tiny MIPS encoder for writing synthetic test programs by hand.
//! Branch offsets are in instructions, relative to the delay slot.

pub const NOP: u32 = 0;
pub const RA: u32 = 31;

pub fn rtype(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}

pub fn itype(op: u32, rs: u32, rt: u32, imm: i32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm as u32 & 0xFFFF)
}

pub fn regimm(code: u32, rs: u32, off: i32) -> u32 {
    itype(1, rs, code, off)
}

pub fn jtype(op: u32, target: u32) -> u32 {
    (op << 26) | ((target >> 2) & 0x03FF_FFFF)
}

pub fn jr(rs: u32) -> u32 {
    rtype(0x08, rs, 0, 0, 0)
}

pub fn jalr(rd: u32, rs: u32) -> u32 {
    rtype(0x09, rs, 0, rd, 0)
}

pub fn addiu(rt: u32, rs: u32, imm: i32) -> u32 {
    itype(0x09, rs, rt, imm)
}

pub fn or(rd: u32, rs: u32, rt: u32) -> u32 {
    rtype(0x25, rs, rt, rd, 0)
}

pub fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
    rtype(0x21, rs, rt, rd, 0)
}

pub fn sll(rd: u32, rt: u32, sa: u32) -> u32 {
    rtype(0x00, 0, rt, rd, sa)
}

pub fn beq(rs: u32, rt: u32, off: i32) -> u32 {
    itype(0x04, rs, rt, off)
}

pub fn bne(rs: u32, rt: u32, off: i32) -> u32 {
    itype(0x05, rs, rt, off)
}

pub fn j(target: u32) -> u32 {
    jtype(0x02, target)
}

pub fn jal(target: u32) -> u32 {
    jtype(0x03, target)
}

/// COP1 arithmetic: `fmt` 16 S, 17 D, 20 W, 21 L.
pub fn cop1(fmt: u32, ft: u32, fs: u32, fd: u32, funct: u32) -> u32 {
    (0x11 << 26) | (fmt << 21) | (ft << 16) | (fs << 11) | (fd << 6) | funct
}

/// COP1 move: `sub` 0 MFC1, 1 DMFC1, 2 CFC1, 4 MTC1, 5 DMTC1, 6 CTC1.
pub fn cop1_move(sub: u32, rt: u32, fs: u32) -> u32 {
    (0x11 << 26) | (sub << 21) | (rt << 16) | (fs << 11)
}

/// BC1F (0), BC1T (1), BC1FL (2), BC1TL (3).
pub fn bc1(kind: u32, off: i32) -> u32 {
    (0x11 << 26) | (0x08 << 21) | (kind << 16) | (off as u32 & 0xFFFF)
}
