//! A plain VR4300 integer interpreter, used only as the reference the
//! translator is tested against.
//!
//! Written for this project from the VR4300 user's manual and the MIPS III
//! instruction set description (see CREDITS.md). It shares no code with the
//! translator or the runtime: it has its own decoder, its own memory, and does
//! unaligned loads and stores byte by byte instead of with masks, so the two
//! implementations check each other. It models the classic `pc`/`next_pc`
//! pair, which is where delay slots come from.

#![forbid(unsafe_code)]

/// Interpreter state.
#[derive(Clone)]
pub struct Cpu {
    pub r: [u64; 32],
    pub hi: u64,
    pub lo: u64,
    pub pc: u32,
    next_pc: u32,
    /// Big-endian RAM, addressed by virtual address with the top 3 bits
    /// dropped.
    pub mem: Vec<u8>,
}

/// Why [`Cpu::run`] stopped early.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// An instruction this interpreter does not implement.
    Unimplemented { pc: u32, word: u32 },
    /// A memory access outside RAM or not aligned.
    BadAccess { pc: u32, addr: u32 },
    /// The step limit was hit.
    StepLimit,
}

fn se32(v: u32) -> u64 {
    i64::from(v as i32) as u64
}

impl Cpu {
    pub fn new(mem: Vec<u8>) -> Self {
        Cpu {
            r: [0; 32],
            hi: 0,
            lo: 0,
            pc: 0,
            next_pc: 4,
            mem,
        }
    }

    fn phys(&self, addr: u32, size: u32, pc: u32) -> Result<usize, Stop> {
        let p = addr & 0x1FFF_FFFF;
        if p % size != 0 || (p + size) as usize > self.mem.len() {
            return Err(Stop::BadAccess { pc, addr });
        }
        Ok(p as usize)
    }

    fn load(&self, addr: u32, size: u32, pc: u32) -> Result<u64, Stop> {
        let p = self.phys(addr, size, pc)?;
        Ok(self.mem[p..p + size as usize]
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    fn store(&mut self, addr: u32, size: u32, v: u64, pc: u32) -> Result<(), Stop> {
        let p = self.phys(addr, size, pc)?;
        for k in 0..size as usize {
            let shift = 8 * (size as usize - 1 - k);
            self.mem[p + k] = (v >> shift) as u8;
        }
        Ok(())
    }

    fn byte(&self, addr: u32, pc: u32) -> Result<u8, Stop> {
        Ok(self.load(addr, 1, pc)? as u8)
    }

    fn set(&mut self, n: u32, v: u64) {
        if n != 0 {
            self.r[n as usize] = v;
        }
    }

    /// Runs from `entry` until `pc` equals `stop` (the return address the
    /// caller planted in `$ra`), or until `max_steps` instructions.
    pub fn run(&mut self, entry: u32, stop: u32, max_steps: usize) -> Result<(), Stop> {
        self.pc = entry;
        self.next_pc = entry.wrapping_add(4);
        for _ in 0..max_steps {
            if self.pc == stop {
                return Ok(());
            }
            self.step()?;
        }
        Err(Stop::StepLimit)
    }

    /// Executes the instruction at `pc`.
    pub fn step(&mut self) -> Result<(), Stop> {
        let pc = self.pc;
        let word = self.load(pc, 4, pc)? as u32;
        // Default: the next instruction is the one in sequence.
        let mut npc = self.next_pc;
        let mut nnpc = self.next_pc.wrapping_add(4);

        let opc = word >> 26;
        let rs = (word >> 21) & 31;
        let rt = (word >> 16) & 31;
        let rd = (word >> 11) & 31;
        let sa = (word >> 6) & 31;
        let funct = word & 63;
        let imm = word & 0xFFFF;
        let simm = i64::from(imm as u16 as i16);
        let s = self.r[rs as usize];
        let t = self.r[rt as usize];
        let ea = (s as u32).wrapping_add(simm as u32);
        let link = se32(pc.wrapping_add(8));
        let btarget = pc.wrapping_add(4).wrapping_add((simm * 4) as u32);
        let unimpl = Stop::Unimplemented { pc, word };

        // Branch helper: (taken, likely).
        let mut branch: Option<(bool, bool)> = None;

        match opc {
            0 => match funct {
                0 => self.set(rd, se32((t as u32) << sa)),
                2 => self.set(rd, se32((t as u32) >> sa)),
                3 => self.set(rd, se32(((t as i64) >> sa) as u32)),
                4 => self.set(rd, se32((t as u32) << (s & 31))),
                6 => self.set(rd, se32((t as u32) >> (s & 31))),
                7 => self.set(rd, se32(((t as i64) >> (s & 31)) as u32)),
                8 => nnpc = s as u32,
                9 => {
                    self.set(rd, link);
                    nnpc = s as u32;
                }
                0x0F => {}
                0x10 => self.set(rd, self.hi),
                0x11 => self.hi = s,
                0x12 => self.set(rd, self.lo),
                0x13 => self.lo = s,
                0x14 => self.set(rd, t << (s & 63)),
                0x16 => self.set(rd, t >> (s & 63)),
                0x17 => self.set(rd, ((t as i64) >> (s & 63)) as u64),
                0x18 => {
                    let p = i64::from(s as i32) * i64::from(t as i32);
                    self.lo = se32(p as u32);
                    self.hi = se32((p >> 32) as u32);
                }
                0x19 => {
                    let p = u64::from(s as u32) * u64::from(t as u32);
                    self.lo = se32(p as u32);
                    self.hi = se32((p >> 32) as u32);
                }
                0x1A => {
                    let (n, d) = (i64::from(s as i32), i64::from(t as i32));
                    if d == 0 {
                        self.lo = if n < 0 { 1 } else { u64::MAX };
                        self.hi = se32(n as u32);
                    } else {
                        // In 64-bit arithmetic MIN / -1 does not overflow;
                        // truncating to 32 bits gives the hardware result.
                        self.lo = se32((n / d) as u32);
                        self.hi = se32((n % d) as u32);
                    }
                }
                0x1B => {
                    let (n, d) = (u64::from(s as u32), u64::from(t as u32));
                    match n.checked_div(d) {
                        None => {
                            self.lo = u64::MAX;
                            self.hi = se32(n as u32);
                        }
                        Some(q) => {
                            self.lo = se32(q as u32);
                            self.hi = se32((n - q * d) as u32);
                        }
                    }
                }
                0x1C => {
                    let p = i128::from(s as i64) * i128::from(t as i64);
                    self.lo = p as u64;
                    self.hi = (p >> 64) as u64;
                }
                0x1D => {
                    let p = u128::from(s) * u128::from(t);
                    self.lo = p as u64;
                    self.hi = (p >> 64) as u64;
                }
                0x1E => {
                    let (n, d) = (i128::from(s as i64), i128::from(t as i64));
                    if d == 0 {
                        self.lo = if n < 0 { 1 } else { u64::MAX };
                        self.hi = n as u64;
                    } else {
                        self.lo = (n / d) as u64;
                        self.hi = (n % d) as u64;
                    }
                }
                0x1F => match s.checked_div(t) {
                    None => {
                        self.lo = u64::MAX;
                        self.hi = s;
                    }
                    Some(q) => {
                        self.lo = q;
                        self.hi = s - q * t;
                    }
                },
                0x20 | 0x21 => self.set(rd, se32((s as u32).wrapping_add(t as u32))),
                0x22 | 0x23 => self.set(rd, se32((s as u32).wrapping_sub(t as u32))),
                0x24 => self.set(rd, s & t),
                0x25 => self.set(rd, s | t),
                0x26 => self.set(rd, s ^ t),
                0x27 => self.set(rd, !(s | t)),
                0x2A => self.set(rd, u64::from((s as i64) < (t as i64))),
                0x2B => self.set(rd, u64::from(s < t)),
                0x2C | 0x2D => self.set(rd, s.wrapping_add(t)),
                0x2E | 0x2F => self.set(rd, s.wrapping_sub(t)),
                0x38 => self.set(rd, t << sa),
                0x3A => self.set(rd, t >> sa),
                0x3B => self.set(rd, ((t as i64) >> sa) as u64),
                0x3C => self.set(rd, t << (sa + 32)),
                0x3E => self.set(rd, t >> (sa + 32)),
                0x3F => self.set(rd, ((t as i64) >> (sa + 32)) as u64),
                _ => return Err(unimpl),
            },
            1 => {
                let neg = (s as i64) < 0;
                let (taken, likely, links) = match rt {
                    0x00 => (neg, false, false),
                    0x01 => (!neg, false, false),
                    0x02 => (neg, true, false),
                    0x03 => (!neg, true, false),
                    0x10 => (neg, false, true),
                    0x11 => (!neg, false, true),
                    0x12 => (neg, true, true),
                    0x13 => (!neg, true, true),
                    _ => return Err(unimpl),
                };
                if links {
                    self.r[31] = link;
                }
                branch = Some((taken, likely));
            }
            2 | 3 => {
                if opc == 3 {
                    self.r[31] = link;
                }
                nnpc = (pc.wrapping_add(4) & 0xF000_0000) | ((word & 0x03FF_FFFF) << 2);
            }
            4 | 0x14 => branch = Some((s == t, opc == 0x14)),
            5 | 0x15 => branch = Some((s != t, opc == 0x15)),
            6 | 0x16 => branch = Some(((s as i64) <= 0, opc == 0x16)),
            7 | 0x17 => branch = Some(((s as i64) > 0, opc == 0x17)),
            8 | 9 => self.set(rt, se32((s as u32).wrapping_add(simm as u32))),
            0x0A => self.set(rt, u64::from((s as i64) < simm)),
            0x0B => self.set(rt, u64::from(s < simm as u64)),
            0x0C => self.set(rt, s & u64::from(imm)),
            0x0D => self.set(rt, s | u64::from(imm)),
            0x0E => self.set(rt, s ^ u64::from(imm)),
            0x0F => self.set(rt, se32(imm << 16)),
            0x18 | 0x19 => self.set(rt, s.wrapping_add(simm as u64)),
            0x20 => {
                let v = self.load(ea, 1, pc)?;
                self.set(rt, v as u8 as i8 as i64 as u64)
            }
            0x21 => {
                let v = self.load(ea, 2, pc)?;
                self.set(rt, v as u16 as i16 as i64 as u64)
            }
            0x23 => {
                let v = self.load(ea, 4, pc)?;
                self.set(rt, se32(v as u32))
            }
            0x24 => {
                let v = self.load(ea, 1, pc)?;
                self.set(rt, v)
            }
            0x25 => {
                let v = self.load(ea, 2, pc)?;
                self.set(rt, v)
            }
            0x27 => {
                let v = self.load(ea, 4, pc)?;
                self.set(rt, v)
            }
            0x37 => {
                let v = self.load(ea, 8, pc)?;
                self.set(rt, v)
            }
            // Unaligned loads, byte by byte. Register bytes are numbered
            // from the most significant end, like memory.
            0x22 | 0x26 | 0x1A | 0x1B => {
                let size: u32 = if opc == 0x22 || opc == 0x26 { 4 } else { 8 };
                let off = ea % size;
                let base = ea - off;
                let mut regb = if size == 4 {
                    let b = (t as u32).to_be_bytes();
                    let mut v = [0u8; 8];
                    v[..4].copy_from_slice(&b);
                    v
                } else {
                    t.to_be_bytes()
                };
                let left = opc == 0x22 || opc == 0x1A;
                if left {
                    // Bytes ea..end of unit go to the top of the register.
                    for k in 0..(size - off) {
                        regb[k as usize] = self.byte(ea + k, pc)?;
                    }
                } else {
                    // Bytes base..=ea go to the bottom of the register.
                    for k in 0..=off {
                        regb[(size - 1 - off + k) as usize] = self.byte(base + k, pc)?;
                    }
                }
                let v = if size == 4 {
                    let w = u32::from_be_bytes([regb[0], regb[1], regb[2], regb[3]]);
                    if left || off == 3 {
                        se32(w)
                    } else {
                        (t & 0xFFFF_FFFF_0000_0000) | u64::from(w)
                    }
                } else {
                    u64::from_be_bytes(regb)
                };
                self.set(rt, v);
            }
            0x28 => self.store(ea, 1, t, pc)?,
            0x29 => self.store(ea, 2, t, pc)?,
            0x2B => self.store(ea, 4, t, pc)?,
            0x3F => self.store(ea, 8, t, pc)?,
            // Unaligned stores, byte by byte.
            0x2A | 0x2E | 0x2C | 0x2D => {
                let size: u32 = if opc == 0x2A || opc == 0x2E { 4 } else { 8 };
                let off = ea % size;
                let base = ea - off;
                let regb: Vec<u8> = if size == 4 {
                    (t as u32).to_be_bytes().to_vec()
                } else {
                    t.to_be_bytes().to_vec()
                };
                let left = opc == 0x2A || opc == 0x2C;
                if left {
                    for k in 0..(size - off) {
                        self.store(ea + k, 1, u64::from(regb[k as usize]), pc)?;
                    }
                } else {
                    for k in 0..=off {
                        let b = regb[(size - 1 - off + k) as usize];
                        self.store(base + k, 1, u64::from(b), pc)?;
                    }
                }
            }
            0x2F => {}
            _ => return Err(unimpl),
        }

        if let Some((taken, likely)) = branch {
            if taken {
                nnpc = btarget;
            } else if likely {
                // Not taken: the delay slot is skipped.
                npc = self.next_pc.wrapping_add(4);
                nnpc = npc.wrapping_add(4);
            }
        }
        self.pc = npc;
        self.next_pc = nnpc;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x8000_0000;

    fn cpu_with(words: &[u32]) -> Cpu {
        let mut c = Cpu::new(vec![0; 0x1000]);
        for (k, w) in words.iter().enumerate() {
            c.mem[k * 4..k * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        c.r[31] = se32(0x8000_0F00);
        c
    }

    #[test]
    fn delay_slot_runs_before_branch_lands() {
        // beq $0,$0,+2 ; addiu $2,$0,1 ; addiu $3,$0,1 ; jr $ra ; nop
        let mut c = cpu_with(&[0x1000_0002, 0x2402_0001, 0x2403_0001, 0x03E0_0008, 0]);
        c.run(BASE, 0x8000_0F00, 100).unwrap();
        assert_eq!(c.r[2], 1, "delay slot ran");
        assert_eq!(c.r[3], 0, "skipped instruction did not run");
    }

    #[test]
    fn likely_not_taken_skips_delay_slot() {
        // bnel $0,$0,+1 ; addiu $2,$0,1 ; jr $ra ; nop
        let mut c = cpu_with(&[0x5400_0001, 0x2402_0001, 0x03E0_0008, 0]);
        c.run(BASE, 0x8000_0F00, 100).unwrap();
        assert_eq!(c.r[2], 0);
    }

    #[test]
    fn sra_shifts_the_whole_register() {
        // sra $2, $4, 4
        let mut c = cpu_with(&[0x0004_1103, 0x03E0_0008, 0]);
        c.r[4] = 0x0000_0001_0000_0000;
        c.run(BASE, 0x8000_0F00, 100).unwrap();
        assert_eq!(c.r[2], 0x1000_0000);
    }

    #[test]
    fn unaligned_word_load_pair() {
        // lwl $2, 1($4) ; lwr $2, 4($4) ; jr $ra ; nop
        let mut c = cpu_with(&[0x8882_0001, 0x9882_0004, 0x03E0_0008, 0]);
        c.mem[0x100..0x108].copy_from_slice(&[0x11, 0x82, 0x33, 0x44, 0x55, 0, 0, 0]);
        c.r[4] = se32(0x8000_0100);
        c.run(BASE, 0x8000_0F00, 100).unwrap();
        assert_eq!(c.r[2], se32(0x8233_4455));
    }
}
