//! A plain VR4300 integer interpreter: the reference the translator is
//! tested against, and the runner's fallback for code with no translation.
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
    /// Floating-point registers as 64-bit cells.
    pub f: [u64; 32],
    /// FCR31.
    pub fcr31: u32,
    /// Status FR bit.
    pub fr: bool,
    /// Big-endian RAM, addressed by virtual address with the top 3 bits
    /// dropped.
    pub mem: Vec<u8>,
    /// TLB pages as (virtual base, length, physical base): addresses
    /// outside KSEG0/KSEG1 that one covers are reached through it.
    pub map: Vec<(u32, u32, u32)>,
    /// The page of `map` that matched last: most accesses fall in it.
    last: std::cell::Cell<(u32, u32, u32)>,
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
            f: [0; 32],
            fcr31: 0,
            fr: true,
            mem,
            map: Vec::new(),
            last: std::cell::Cell::new((0, 0, 0)),
        }
    }

    /// `addr` as a KSEG0 address when a TLB page covers it.
    fn virt(&self, addr: u32) -> u32 {
        if addr.wrapping_sub(0x8000_0000) < 0x4000_0000 {
            return addr;
        }
        let (base, len, phys) = self.last.get();
        if addr.wrapping_sub(base) < len {
            return 0x8000_0000 | (phys + (addr - base));
        }
        match self
            .map
            .iter()
            .find(|&&(base, len, _)| addr.wrapping_sub(base) < len)
        {
            Some(&(base, len, phys)) => {
                self.last.set((base, len, phys));
                0x8000_0000 | (phys + (addr - base))
            }
            None => addr,
        }
    }

    /// Replaces the TLB pages (and forgets the page that matched last).
    pub fn set_map(&mut self, map: Vec<(u32, u32, u32)>) {
        self.map = map;
        self.last.set((0, 0, 0));
    }

    fn phys(mem: &[u8], addr: u32, size: u32, pc: u32) -> Result<usize, Stop> {
        let p = addr & 0x1FFF_FFFF;
        if p % size != 0 || (p + size) as usize > mem.len() {
            return Err(Stop::BadAccess { pc, addr });
        }
        Ok(p as usize)
    }

    fn load(mem: &[u8], addr: u32, size: u32, pc: u32) -> Result<u64, Stop> {
        let p = Self::phys(mem, addr, size, pc)?;
        Ok(mem[p..p + size as usize]
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    fn store(mem: &mut [u8], addr: u32, size: u32, v: u64, pc: u32) -> Result<(), Stop> {
        let p = Self::phys(mem, addr, size, pc)?;
        for k in 0..size as usize {
            let shift = 8 * (size as usize - 1 - k);
            mem[p + k] = (v >> shift) as u8;
        }
        Ok(())
    }

    fn byte(mem: &[u8], addr: u32, pc: u32) -> Result<u8, Stop> {
        Ok(Self::load(mem, addr, 1, pc)? as u8)
    }

    // Floating-point register file. With FR clear the register file is
    // sixteen 64-bit pairs: single n lives in half (n & 1) of pair n / 2.
    fn get_w(&self, n: u32) -> u32 {
        let (cell, half) = if self.fr { (n, 0) } else { (n & !1, n & 1) };
        (self.f[cell as usize] >> (32 * half)) as u32
    }

    fn put_w(&mut self, n: u32, v: u32) {
        let (cell, half) = if self.fr { (n, 0) } else { (n & !1, n & 1) };
        let shift = 32 * half;
        let keep = !(0xFFFF_FFFFu64 << shift);
        let c = &mut self.f[cell as usize];
        *c = (*c & keep) | (u64::from(v) << shift);
    }

    fn get_l(&self, n: u32) -> u64 {
        self.f[if self.fr { n } else { n & !1 } as usize]
    }

    fn put_l(&mut self, n: u32, v: u64) {
        self.f[if self.fr { n } else { n & !1 } as usize] = v;
    }

    /// Integer conversion with an explicit rounding: 0 nearest-even,
    /// 1 toward zero, 2 up, 3 down. Out of range or NaN gives the maximum
    /// positive value, the MIPS default for an invalid conversion.
    fn to_int(x: f64, mode: u32, bits: u32) -> u64 {
        let r = match mode {
            0 => {
                let fl = x.floor();
                let diff = x - fl;
                if diff > 0.5 || (diff == 0.5 && fl % 2.0 != 0.0) {
                    fl + 1.0
                } else {
                    fl
                }
            }
            1 => x.trunc(),
            2 => x.ceil(),
            _ => x.floor(),
        };
        let limit = if bits == 32 {
            2147483648.0
        } else {
            9223372036854775808.0
        };
        let max = if bits == 32 {
            0x7FFF_FFFF
        } else {
            0x7FFF_FFFF_FFFF_FFFF
        };
        if !(r >= -limit && r < limit) {
            return max;
        }
        if bits == 32 {
            u64::from(r as i32 as u32)
        } else {
            r as i64 as u64
        }
    }

    fn cop1(&mut self, word: u32, unimpl: Stop) -> Result<(), Stop> {
        let fmt = (word >> 21) & 31;
        let ft = (word >> 16) & 31;
        let fs = (word >> 11) & 31;
        let fd = (word >> 6) & 31;
        let funct = word & 63;
        let single = fmt == 16;
        let double = fmt == 17;
        if !(single || double) {
            // W and L sources: only conversions to S and D.
            let v = match fmt {
                20 => f64::from(self.get_w(fs) as i32),
                21 => {
                    let l = self.get_l(fs) as i64;
                    match funct {
                        32 => {
                            self.put_w(fd, (l as f32).to_bits());
                            return Ok(());
                        }
                        33 => l as f64,
                        _ => return Err(unimpl),
                    }
                }
                _ => return Err(unimpl),
            };
            match funct {
                // i32 -> f64 is exact; f64 -> f32 then rounds once.
                32 => self.put_w(fd, (v as f32).to_bits()),
                33 => self.put_l(fd, v.to_bits()),
                _ => return Err(unimpl),
            }
            return Ok(());
        }
        let a32 = f32::from_bits(self.get_w(fs));
        let b32 = f32::from_bits(self.get_w(ft));
        let a64 = f64::from_bits(self.get_l(fs));
        let b64 = f64::from_bits(self.get_l(ft));
        let (a, b) = if single {
            (f64::from(a32), f64::from(b32))
        } else {
            (a64, b64)
        };
        macro_rules! arith {
            ($op:tt) => {
                if single {
                    self.put_w(fd, (a32 $op b32).to_bits())
                } else {
                    self.put_l(fd, (a64 $op b64).to_bits())
                }
            };
        }
        match funct {
            0 => arith!(+),
            1 => arith!(-),
            2 => arith!(*),
            3 => arith!(/),
            4 => {
                if single {
                    self.put_w(fd, a32.sqrt().to_bits())
                } else {
                    self.put_l(fd, a64.sqrt().to_bits())
                }
            }
            // ABS and NEG act on the sign bit only.
            5 => {
                if single {
                    self.put_w(fd, self.get_w(fs) & 0x7FFF_FFFF)
                } else {
                    self.put_l(fd, self.get_l(fs) & 0x7FFF_FFFF_FFFF_FFFF)
                }
            }
            6 => {
                if single {
                    self.put_w(fd, self.get_w(fs))
                } else {
                    self.put_l(fd, self.get_l(fs))
                }
            }
            7 => {
                if single {
                    self.put_w(fd, self.get_w(fs) ^ 0x8000_0000)
                } else {
                    self.put_l(fd, self.get_l(fs) ^ 0x8000_0000_0000_0000)
                }
            }
            8..=11 => self.put_l(fd, Self::to_int(a, funct & 3, 64)),
            12..=15 => self.put_w(fd, Self::to_int(a, funct & 3, 32) as u32),
            32 if double => self.put_w(fd, (a64 as f32).to_bits()),
            33 if single => self.put_l(fd, a.to_bits()),
            36 => self.put_w(fd, Self::to_int(a, self.fcr31 & 3, 32) as u32),
            37 => self.put_l(fd, Self::to_int(a, self.fcr31 & 3, 64)),
            48..=63 => {
                let unordered = a.is_nan() || b.is_nan();
                let c = (funct & 1 != 0 && unordered)
                    || (funct & 2 != 0 && !unordered && a == b)
                    || (funct & 4 != 0 && !unordered && a < b);
                if c {
                    self.fcr31 |= 1 << 23;
                } else {
                    self.fcr31 &= !(1 << 23);
                }
            }
            _ => return Err(unimpl),
        }
        Ok(())
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

    /// Continues at `pc`, with no branch pending.
    pub fn jump_to(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
    }

    /// Moves past the instruction at `pc` without running it (the caller ran
    /// it), keeping a pending branch.
    pub fn skip(&mut self) {
        self.pc = self.next_pc;
        self.next_pc = self.next_pc.wrapping_add(4);
    }

    /// Executes the instruction at `pc`.
    pub fn step(&mut self) -> Result<(), Stop> {
        let mut mem = std::mem::take(&mut self.mem);
        let r = self.step_on(&mut mem);
        self.mem = mem;
        r
    }

    /// Executes the instruction at `pc` against `mem` instead of the
    /// interpreter's own RAM (same addressing: virtual address with the top
    /// three bits dropped).
    pub fn step_on(&mut self, mem: &mut [u8]) -> Result<(), Stop> {
        let pc = self.pc;
        let word = Self::load(mem, self.virt(pc), 4, pc)? as u32;
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
        // Only loads and stores (LDL and LDR at 0x1A and 0x1B, the rest from
        // 0x20) use it; working it out for every instruction cost a TLB
        // search on most.
        let ea = if opc >= 0x1A {
            self.virt((s as u32).wrapping_add(simm as u32))
        } else {
            0
        };
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
                let v = Self::load(mem, ea, 1, pc)?;
                self.set(rt, v as u8 as i8 as i64 as u64)
            }
            0x21 => {
                let v = Self::load(mem, ea, 2, pc)?;
                self.set(rt, v as u16 as i16 as i64 as u64)
            }
            0x23 => {
                let v = Self::load(mem, ea, 4, pc)?;
                self.set(rt, se32(v as u32))
            }
            0x24 => {
                let v = Self::load(mem, ea, 1, pc)?;
                self.set(rt, v)
            }
            0x25 => {
                let v = Self::load(mem, ea, 2, pc)?;
                self.set(rt, v)
            }
            0x27 => {
                let v = Self::load(mem, ea, 4, pc)?;
                self.set(rt, v)
            }
            0x37 => {
                let v = Self::load(mem, ea, 8, pc)?;
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
                        regb[k as usize] = Self::byte(mem, ea + k, pc)?;
                    }
                } else {
                    // Bytes base..=ea go to the bottom of the register.
                    for k in 0..=off {
                        regb[(size - 1 - off + k) as usize] = Self::byte(mem, base + k, pc)?;
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
            0x28 => Self::store(mem, ea, 1, t, pc)?,
            0x29 => Self::store(mem, ea, 2, t, pc)?,
            0x2B => Self::store(mem, ea, 4, t, pc)?,
            0x3F => Self::store(mem, ea, 8, t, pc)?,
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
                        Self::store(mem, ea + k, 1, u64::from(regb[k as usize]), pc)?;
                    }
                } else {
                    for k in 0..=off {
                        let b = regb[(size - 1 - off + k) as usize];
                        Self::store(mem, base + k, 1, u64::from(b), pc)?;
                    }
                }
            }
            0x2F => {}
            0x11 => match rs {
                0 => self.set(rt, se32(self.get_w(rd))),
                1 => self.set(rt, self.get_l(rd)),
                2 => {
                    let v = match rd {
                        0 => 0x0A00,
                        31 => self.fcr31,
                        _ => 0,
                    };
                    self.set(rt, se32(v))
                }
                4 => self.put_w(rd, t as u32),
                5 => self.put_l(rd, t),
                6 => {
                    if rd == 31 {
                        self.fcr31 = t as u32;
                    }
                }
                8 => {
                    let c = self.fcr31 & (1 << 23) != 0;
                    let want_true = rt & 1 != 0;
                    branch = Some((c == want_true, rt & 2 != 0));
                }
                16 | 17 | 20 | 21 => self.cop1(word, unimpl)?,
                _ => return Err(unimpl),
            },
            0x31 => {
                let v = Self::load(mem, ea, 4, pc)?;
                self.put_w(rt, v as u32)
            }
            0x35 => {
                let v = Self::load(mem, ea, 8, pc)?;
                self.put_l(rt, v)
            }
            0x39 => Self::store(mem, ea, 4, u64::from(self.get_w(rt)), pc)?,
            0x3D => Self::store(mem, ea, 8, self.get_l(rt), pc)?,
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
