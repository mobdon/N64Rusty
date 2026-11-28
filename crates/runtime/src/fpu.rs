//! Floating-point unit (COP1) state access and conversions.
//!
//! Arithmetic uses the host's IEEE 754 operations, which round to nearest.
//! The rounding mode in FCR31 is honoured by the float-to-integer
//! conversions, which is where games depend on it. Exceptions, cause and
//! flag bits are not modelled; an invalid conversion (NaN or out of range)
//! gives the MIPS default result, the largest positive integer.

use crate::Ctx;

/// FCR31 bit holding the result of the last compare.
pub const COND: u32 = 1 << 23;

/// Rounding for float-to-integer conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    /// To nearest, ties to even (FCR31 mode 0, ROUND.fmt).
    Nearest,
    /// Toward zero (mode 1, TRUNC.fmt).
    Zero,
    /// Toward +infinity (mode 2, CEIL.fmt).
    Up,
    /// Toward -infinity (mode 3, FLOOR.fmt).
    Down,
}

impl Round {
    /// The mode selected by FCR31's low two bits.
    pub fn from_fcr31(fcr31: u32) -> Self {
        match fcr31 & 3 {
            0 => Round::Nearest,
            1 => Round::Zero,
            2 => Round::Up,
            _ => Round::Down,
        }
    }

    fn apply(self, x: f64) -> f64 {
        match self {
            Round::Nearest => x.round_ties_even(),
            Round::Zero => x.trunc(),
            Round::Up => x.ceil(),
            Round::Down => x.floor(),
        }
    }
}

/// Converts to a 32-bit integer (CVT.W, ROUND.W, TRUNC.W, CEIL.W, FLOOR.W).
/// Singles are passed widened to `f64`, which is exact.
pub fn cvt_w(x: f64, mode: Round) -> u32 {
    let r = mode.apply(x);
    if !(-2_147_483_648.0..2_147_483_648.0).contains(&r) {
        0x7FFF_FFFF
    } else {
        r as i32 as u32
    }
}

/// Converts to a 64-bit integer (CVT.L, ROUND.L, ...).
pub fn cvt_l(x: f64, mode: Round) -> u64 {
    let r = mode.apply(x);
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&r) {
        0x7FFF_FFFF_FFFF_FFFF
    } else {
        r as i64 as u64
    }
}

/// Evaluates a C.cond compare. `cond` is the low four bits of the funct
/// field: bit 2 less, bit 1 equal, bit 0 unordered. Bit 3 (signalling)
/// only matters for exceptions, which are not modelled.
pub fn fcmp(a: f64, b: f64, cond: u32) -> bool {
    let unordered = a.is_nan() || b.is_nan();
    (cond & 4 != 0 && a < b) || (cond & 2 != 0 && a == b) || (cond & 1 != 0 && unordered)
}

impl Ctx {
    /// Raw 32 bits of single register `n`.
    #[inline(always)]
    pub fn fw(&self, n: usize) -> u32 {
        if self.fr || n & 1 == 0 {
            self.f[n] as u32
        } else {
            (self.f[n - 1] >> 32) as u32
        }
    }

    /// Writes the raw 32 bits of single register `n`, keeping the other half
    /// of the 64-bit register.
    #[inline(always)]
    pub fn set_fw(&mut self, n: usize, v: u32) {
        if self.fr || n & 1 == 0 {
            self.f[n] = (self.f[n] & 0xFFFF_FFFF_0000_0000) | u64::from(v);
        } else {
            self.f[n - 1] = (self.f[n - 1] & 0xFFFF_FFFF) | (u64::from(v) << 32);
        }
    }

    /// Raw 64 bits of double register `n`. With FR clear the register is
    /// the even one of the pair.
    #[inline(always)]
    pub fn fl(&self, n: usize) -> u64 {
        if self.fr {
            self.f[n]
        } else {
            self.f[n & !1]
        }
    }

    /// Writes the raw 64 bits of double register `n`.
    #[inline(always)]
    pub fn set_fl(&mut self, n: usize, v: u64) {
        if self.fr {
            self.f[n] = v;
        } else {
            self.f[n & !1] = v;
        }
    }

    /// Single register `n` as a float.
    #[inline(always)]
    pub fn fs(&self, n: usize) -> f32 {
        f32::from_bits(self.fw(n))
    }

    /// Writes single register `n` as a float.
    #[inline(always)]
    pub fn set_fs(&mut self, n: usize, v: f32) {
        self.set_fw(n, v.to_bits());
    }

    /// Double register `n` as a float.
    #[inline(always)]
    pub fn fd(&self, n: usize) -> f64 {
        f64::from_bits(self.fl(n))
    }

    /// Writes double register `n` as a float.
    #[inline(always)]
    pub fn set_fd(&mut self, n: usize, v: f64) {
        self.set_fl(n, v.to_bits());
    }

    /// The compare condition bit, read by BC1T/BC1F.
    #[inline(always)]
    pub fn c1(&self) -> bool {
        self.fcr31 & COND != 0
    }

    /// Sets or clears the compare condition bit.
    #[inline(always)]
    pub fn set_c1(&mut self, c: bool) {
        if c {
            self.fcr31 |= COND;
        } else {
            self.fcr31 &= !COND;
        }
    }

    /// CFC1: reads a floating-point control register.
    pub fn cfc1(&self, n: usize) -> u32 {
        match n {
            // Implementation and revision of the VR4300's FPU.
            0 => 0x0000_0A00,
            31 => self.fcr31,
            _ => 0,
        }
    }

    /// CTC1: writes a floating-point control register. Only FCR31 is
    /// writable.
    pub fn ctc1(&mut self, n: usize, v: u32) {
        if n == 31 {
            self.fcr31 = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Memory;

    #[test]
    fn register_modes() {
        let mut c = Ctx::new(Memory::new(16));
        c.fr = false;
        c.set_fw(4, 0x1111_1111);
        c.set_fw(5, 0x2222_2222);
        assert_eq!(c.fl(4), 0x2222_2222_1111_1111);
        assert_eq!(c.fl(5), 0x2222_2222_1111_1111);
        c.fr = true;
        c.set_fw(5, 0x3333_3333);
        assert_eq!(c.f[5] as u32, 0x3333_3333);
        assert_eq!(c.fw(4), 0x1111_1111);
    }

    #[test]
    fn conversions_follow_the_rounding_mode() {
        assert_eq!(cvt_w(2.5, Round::Nearest), 2);
        assert_eq!(cvt_w(3.5, Round::Nearest), 4);
        assert_eq!(cvt_w(-2.5, Round::Zero) as i32, -2);
        assert_eq!(cvt_w(-2.5, Round::Down) as i32, -3);
        assert_eq!(cvt_w(2.1, Round::Up), 3);
        assert_eq!(cvt_w(f64::NAN, Round::Nearest), 0x7FFF_FFFF);
        assert_eq!(cvt_w(3e9, Round::Zero), 0x7FFF_FFFF);
        assert_eq!(cvt_w(-2_147_483_648.0, Round::Zero), 0x8000_0000);
        assert_eq!(cvt_l(-1.5, Round::Nearest) as i64, -2);
        assert_eq!(Round::from_fcr31(0x0080_0003), Round::Down);
    }

    #[test]
    fn compares() {
        assert!(fcmp(1.0, 2.0, 4)); // olt
        assert!(!fcmp(f64::NAN, 2.0, 4));
        assert!(fcmp(f64::NAN, 2.0, 5)); // ult
        assert!(fcmp(0.0, -0.0, 2)); // eq: +0 == -0
        assert!(!fcmp(1.0, 1.0, 0)); // f
    }

    #[test]
    fn control_registers() {
        let mut c = Ctx::new(Memory::new(16));
        c.ctc1(31, 0x0080_0001);
        assert!(c.c1());
        c.set_c1(false);
        assert_eq!(c.cfc1(31), 1);
        assert_eq!(c.cfc1(0), 0xA00);
    }
}
