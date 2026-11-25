//! Big-endian guest RAM.

use std::collections::HashMap;
use std::sync::Arc;

use crate::sx32;

/// What lies outside RAM, for hosts that run real games: the RSP's memory,
/// the cartridge ROM, and every other hardware register as plain storage
/// (writes are kept, reads give the last value written or zero). Device
/// behaviour belongs in the host, which replaces the system routines that
/// depend on it.
#[derive(Clone)]
pub struct Io {
    /// RSP data and instruction memory, 8 KiB at physical 0x0400_0000.
    pub sp_mem: Vec<u8>,
    /// Cartridge ROM at physical 0x1000_0000, read-only.
    pub rom: Arc<[u8]>,
    /// Other registers, by word address.
    pub regs: HashMap<u32, u32>,
}

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
    io: Option<Box<Io>>,
}

impl Memory {
    /// Zeroed RAM of `size` bytes (4 MiB, or 8 MiB with the expansion).
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
            io: None,
        }
    }

    /// RAM with the given contents, in big-endian order.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes, io: None }
    }

    /// Maps the space outside RAM. Without this, such accesses panic.
    pub fn set_io(&mut self, io: Io) {
        self.io = Some(Box::new(io));
    }

    /// The space outside RAM, if mapped.
    pub fn io_mut(&mut self) -> Option<&mut Io> {
        self.io.as_deref_mut()
    }

    #[cold]
    #[inline(never)]
    fn io_get(&self, vaddr: u32, p: usize, out: &mut [u8]) {
        let Some(io) = self.io.as_deref().filter(|_| unmapped_ok(vaddr)) else {
            out_of_range(vaddr)
        };
        for (k, b) in out.iter_mut().enumerate() {
            let a = p + k;
            *b = match a {
                0x0400_0000..=0x0400_1FFF => io.sp_mem[a - 0x0400_0000],
                0x1000_0000..=0x1FBF_FFFF => io.rom.get(a - 0x1000_0000).copied().unwrap_or(0),
                _ => {
                    let w = io.regs.get(&(a as u32 & !3)).copied().unwrap_or(0);
                    (w >> (8 * (3 - (a & 3)))) as u8
                }
            };
        }
    }

    #[cold]
    #[inline(never)]
    fn io_put(&mut self, vaddr: u32, p: usize, data: &[u8]) {
        let Some(io) = self.io.as_deref_mut().filter(|_| unmapped_ok(vaddr)) else {
            out_of_range(vaddr)
        };
        for (k, &b) in data.iter().enumerate() {
            let a = p + k;
            match a {
                0x0400_0000..=0x0400_1FFF => io.sp_mem[a - 0x0400_0000] = b,
                0x1000_0000..=0x1FBF_FFFF => {}
                _ => {
                    let w = io.regs.entry(a as u32 & !3).or_insert(0);
                    let sh = 8 * (3 - (a & 3));
                    *w = (*w & !(0xFF << sh)) | (u32::from(b) << sh);
                }
            }
        }
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
    fn phys(vaddr: u32, len: usize) -> usize {
        let p = (vaddr & 0x1FFF_FFFF) as usize;
        if p & (len - 1) != 0 {
            misaligned(vaddr, len);
        }
        p
    }

    #[inline(always)]
    fn get<const N: usize>(&self, vaddr: u32) -> [u8; N] {
        let p = Self::phys(vaddr, N);
        let mut b = [0; N];
        if p + N <= self.bytes.len() {
            b.copy_from_slice(&self.bytes[p..p + N]);
        } else {
            self.io_get(vaddr, p, &mut b);
        }
        b
    }

    #[inline(always)]
    fn put<const N: usize>(&mut self, vaddr: u32, b: [u8; N]) {
        let p = Self::phys(vaddr, N);
        if p + N <= self.bytes.len() {
            self.bytes[p..p + N].copy_from_slice(&b);
        } else {
            self.io_put(vaddr, p, &b);
        }
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

/// Hardware outside RAM is only reached through the unmapped windows
/// (KSEG0 `0x8000_0000`, KSEG1 `0xA000_0000`). Anything else past RAM is a
/// TLB address, which is not modelled, or a wild pointer: stop there
/// instead of storing it as a register.
fn unmapped_ok(vaddr: u32) -> bool {
    (0x8000_0000..0xC000_0000).contains(&vaddr)
}

/// Extra words for a crash at an address that is really an instruction or
/// data read as a pointer.
pub(crate) fn wild_hint(addr: u32) -> &'static str {
    if addr >> 16 == 0x27BD {
        " (looks like the instruction addiu sp, sp, n: a pointer overwritten with code)"
    } else if !(0x8000_0000..0xC000_0000).contains(&addr) {
        " (not a KSEG0/KSEG1 address: a wild or overwritten pointer, or TLB-mapped memory)"
    } else {
        ""
    }
}

#[cold]
#[inline(never)]
fn out_of_range(vaddr: u32) -> ! {
    panic!(
        "guest access outside RAM at {vaddr:#010x}{}",
        wild_hint(vaddr)
    )
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
    #[should_panic(expected = "0x27bdffe8 (looks like the instruction addiu sp")]
    fn pointer_overwritten_with_code_stops() {
        let mut m = ram();
        m.set_io(Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(vec![]),
            regs: HashMap::new(),
        });
        m.read_u32(0x27BD_FFE8);
    }

    #[test]
    #[should_panic(expected = "not a KSEG0/KSEG1 address")]
    fn mapped_segment_past_ram_stops() {
        let mut m = ram();
        m.set_io(Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(vec![]),
            regs: HashMap::new(),
        });
        m.write_u32(0xC000_0040, 1);
    }

    #[test]
    fn io_space_when_mapped() {
        let mut m = ram();
        m.set_io(Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(vec![1u8, 2, 3, 4]),
            regs: HashMap::new(),
        });
        m.write_u32(0xA480_0018, 0x1234_5678);
        assert_eq!(m.read_u32(0xA480_0018), 0x1234_5678);
        assert_eq!(m.read_u8(0xA480_001B), 0x78);
        assert_eq!(m.read_u32(0xB000_0000), 0x0102_0304);
        m.write_u16(0xA400_0010, 0xBEEF);
        assert_eq!(m.read_u16(0xA400_0010), 0xBEEF);
    }

    #[test]
    #[should_panic(expected = "outside RAM")]
    fn unmapped_io_panics() {
        ram().read_u32(0xA480_0018);
    }

    #[test]
    #[should_panic(expected = "misaligned")]
    fn misaligned_word_panics() {
        ram().read_u32(0x8000_0002);
    }
}
