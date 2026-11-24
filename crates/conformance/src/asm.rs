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
