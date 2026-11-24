//! Big-endian guest RAM.

use crate::sx32;

/// Guest RAM, stored big-endian as on the console.
///
/// Virtual addresses are mapped by dropping the top three bits, which covers
/// the cached (KSEG0, `0x8000_0000`) and uncached (KSEG1, `0xA000_0000`)
/// windows. The TLB is not modelled yet. Accesses outside RAM and misaligned
/// accesses panic with the address, since on hardware they raise exceptions
/// that recompiled code is not expected to rely on.
#[derive(Clone)]
pub struct Memory {
    bytes: Vec<u8>,
}

impl Memory {
    /// Zeroed RAM of `size` bytes (4 MiB, or 8 MiB with the expansion).
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
        }
    }

    /// RAM with the given contents, in big-endian order.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The raw big-endian contents.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable raw big-endian contents.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    #[inline(always)]
    fn phys(&self, vaddr: u32, len: usize) -> usize {
        let p = (vaddr & 0x1FFF_FFFF) as usize;
        if p & (len - 1) != 0 {
            misaligned(vaddr, len);
        }
        if p + len > self.bytes.len() {
            out_of_range(vaddr);
        }
        p
    }

    #[inline(always)]
    fn get<const N: usize>(&self, vaddr: u32) -> [u8; N] {
        let p = self.phys(vaddr, N);
        let mut b = [0; N];
        b.copy_from_slice(&self.bytes[p..p + N]);
        b
    }

    #[inline(always)]
    fn put<const N: usize>(&mut self, vaddr: u32, b: [u8; N]) {
        let p = self.phys(vaddr, N);
        self.bytes[p..p + N].copy_from_slice(&b);
    }

    #[inline(always)]
    pub fn read_u8(&self, a: u32) -> u8 {
        self.get::<1>(a)[0]
    }
    #[inline(always)]
    pub fn read_u16(&self, a: u32) -> u16 {
        u16::from_be_bytes(self.get(a))
    }
    #[inline(always)]
    pub fn read_u32(&self, a: u32) -> u32 {
        u32::from_be_bytes(self.get(a))
    }
    #[inline(always)]
    pub fn read_u64(&self, a: u32) -> u64 {
        u64::from_be_bytes(self.get(a))
    }
    #[inline(always)]
    pub fn write_u8(&mut self, a: u32, v: u8) {
        self.put(a, [v]);
    }
    #[inline(always)]
    pub fn write_u16(&mut self, a: u32, v: u16) {
        self.put(a, v.to_be_bytes());
    }
    #[inline(always)]
    pub fn write_u32(&mut self, a: u32, v: u32) {
        self.put(a, v.to_be_bytes());
    }
    #[inline(always)]
    pub fn write_u64(&mut self, a: u32, v: u64) {
        self.put(a, v.to_be_bytes());
    }

    /// LWL: loads the bytes from `a` to the end of its word into the high
    /// end of the register. Always sign-extends, since bit 31 is loaded.
    pub fn lwl(&self, a: u32, old: u64) -> u64 {
        let w = self.read_u32(a & !3);
        let sh = (a & 3) * 8;
        let mask = u32::MAX << sh;
        sx32((old as u32 & !mask) | (w << sh))
    }

    /// LWR: loads the bytes from the start of the word up to `a` into the
    /// low end of the register. Sign-extends only when the whole word is
    /// loaded (bit 31 written); otherwise the upper 32 bits are kept, as the
    /// VR4300 manual describes.
    pub fn lwr(&self, a: u32, old: u64) -> u64 {
        let w = self.read_u32(a & !3);
        let sh = (3 - (a & 3)) * 8;
        let mask = u32::MAX >> sh;
        let v = (old as u32 & !mask) | (w >> sh);
        if sh == 0 {
            sx32(v)
        } else {
            (old & 0xFFFF_FFFF_0000_0000) | v as u64
        }
    }

    /// LDL: 64-bit form of [`Memory::lwl`].
    pub fn ldl(&self, a: u32, old: u64) -> u64 {
        let d = self.read_u64(a & !7);
        let sh = (a & 7) * 8;
        let mask = u64::MAX << sh;
        (old & !mask) | (d << sh)
    }

    /// LDR: 64-bit form of [`Memory::lwr`].
    pub fn ldr(&self, a: u32, old: u64) -> u64 {
        let d = self.read_u64(a & !7);
        let sh = (7 - (a & 7)) * 8;
        let mask = u64::MAX >> sh;
        (old & !mask) | (d >> sh)
    }

    /// SWL: stores the high end of the register into `a` up to the end of
    /// its word.
    pub fn swl(&mut self, a: u32, v: u64) {
        let w = self.read_u32(a & !3);
        let sh = (a & 3) * 8;
        let mask = u32::MAX >> sh;
        self.write_u32(a & !3, (w & !mask) | ((v as u32) >> sh));
    }

    /// SWR: stores the low end of the register from the start of the word
    /// up to `a`.
    pub fn swr(&mut self, a: u32, v: u64) {
        let w = self.read_u32(a & !3);
        let sh = (3 - (a & 3)) * 8;
        let mask = u32::MAX << sh;
        self.write_u32(a & !3, (w & !mask) | ((v as u32) << sh));
    }

    /// SDL: 64-bit form of [`Memory::swl`].
    pub fn sdl(&mut self, a: u32, v: u64) {
        let d = self.read_u64(a & !7);
        let sh = (a & 7) * 8;
        let mask = u64::MAX >> sh;
        self.write_u64(a & !7, (d & !mask) | (v >> sh));
    }

    /// SDR: 64-bit form of [`Memory::swr`].
    pub fn sdr(&mut self, a: u32, v: u64) {
        let d = self.read_u64(a & !7);
        let sh = (7 - (a & 7)) * 8;
        let mask = u64::MAX << sh;
        self.write_u64(a & !7, (d & !mask) | (v << sh));
    }
}

#[cold]
#[inline(never)]
fn misaligned(vaddr: u32, len: usize) -> ! {
    panic!("misaligned {len}-byte guest access at {vaddr:#010x}")
}

#[cold]
#[inline(never)]
fn out_of_range(vaddr: u32) -> ! {
    panic!("guest access outside RAM at {vaddr:#010x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram() -> Memory {
        let mut m = Memory::new(32);
        m.bytes_mut()[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        m
    }

    #[test]
    fn big_endian_and_segments() {
        let m = ram();
        assert_eq!(m.read_u32(0x8000_0000), 0x1122_3344);
        assert_eq!(m.read_u16(0xA000_0002), 0x3344);
        assert_eq!(m.read_u64(0x8000_0000), 0x1122_3344_5566_7788);
    }

    #[test]
    fn unaligned_word_loads() {
        let m = ram();
        let old = 0xAAAA_AAAA_AABB_CCDD;
        assert_eq!(m.lwl(0x8000_0001, old), 0x0000_0000_2233_44DD);
        assert_eq!(m.lwr(0x8000_0001, old), 0xAAAA_AAAA_AABB_1122);
        assert_eq!(m.lwr(0x8000_0003, old), 0x0000_0000_1122_3344);
        assert_eq!(m.lwl(0x8000_0000, old), 0x0000_0000_1122_3344);
    }

    #[test]
    fn unaligned_word_stores() {
        let mut m = ram();
        m.swl(0x8000_0001, 0xA1B2_C3D4);
        assert_eq!(m.read_u32(0x8000_0000), 0x11A1_B2C3);
        let mut m = ram();
        m.swr(0x8000_0001, 0xA1B2_C3D4);
        assert_eq!(m.read_u32(0x8000_0000), 0xC3D4_3344);
    }

    #[test]
    fn unaligned_double_access() {
        let mut m = ram();
        assert_eq!(m.ldl(0x8000_0003, 0xFF), 0x4455_6677_8800_00FF);
        assert_eq!(m.ldr(0x8000_0003, u64::MAX), 0xFFFF_FFFF_1122_3344);
        m.sdl(0x8000_0006, 0xA1B2_C3D4_E5F6_0718);
        assert_eq!(m.read_u64(0x8000_0000), 0x1122_3344_5566_A1B2);
        m.sdr(0x8000_0001, 0xA1B2_C3D4_E5F6_0718);
        assert_eq!(m.read_u64(0x8000_0000), 0x0718_3344_5566_A1B2);
    }

    #[test]
    #[should_panic(expected = "misaligned")]
    fn misaligned_word_panics() {
        ram().read_u32(0x8000_0002);
    }
}
