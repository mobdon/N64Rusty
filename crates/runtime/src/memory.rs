//! Big-endian guest RAM.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// A flash save chip, if the game turns out to have one.
    pub flash: Flash,
}

/// Start of KSEG0, the cached window onto physical memory. KSEG1, the
/// uncached one, follows it at `0xA000_0000`.
const KSEG0: u32 = 0x8000_0000;
/// Length of KSEG0 and KSEG1 together.
const DIRECT_LEN: u32 = 0x4000_0000;
/// Bits of a KSEG0/KSEG1 address that give the physical address.
pub(crate) const PHYS_MASK: u32 = 0x1FFF_FFFF;

/// Whether `vaddr` is in KSEG0 or KSEG1, which map to physical memory
/// without the TLB.
#[inline(always)]
pub(crate) fn direct(vaddr: u32) -> bool {
    vaddr.wrapping_sub(KSEG0) < DIRECT_LEN
}

/// End of the physical window RDRAM can fill (the RDRAM registers start
/// here). Addresses below it but past the RAM installed are no memory.
const RDRAM_WINDOW_END: usize = 0x03F0_0000;

/// Physical address of the flash chip's status (read) on the PI bus.
const FLASH_STATUS: usize = 0x0800_0000;
/// Physical address of the flash chip's command register (write).
const FLASH_COMMAND: usize = 0x0801_0000;

/// The command side of a flash save chip: games write commands to its
/// register and read its status word. Commands that change the stored
/// bytes are queued for the host, which keeps the bytes and does the DMA.
#[derive(Clone, Default, Debug)]
pub struct Flash {
    /// Set by the first command: this game's save memory is flash.
    pub used: bool,
    /// Top byte of the last mode command: 0xD2 status, 0xE1 id,
    /// 0xF0 read, 0xB4 fill the page buffer.
    pub mode: u8,
    status: u8,
    erase: (usize, usize),
    /// Changes to the stored bytes, in order, for the host to apply.
    pub ops: Vec<FlashOp>,
}

/// A change to a flash chip's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlashOp {
    /// Set `len` bytes from `offset` to 0xFF.
    Erase {
        /// First byte cleared.
        offset: usize,
        /// Number of bytes cleared.
        len: usize,
    },
    /// Write the page buffer (128 bytes) at `offset`.
    Program {
        /// First byte written.
        offset: usize,
    },
}

impl Flash {
    /// Size of the 1 Mbit chip.
    pub const SIZE: usize = 0x2_0000;
    /// Identity read back in id mode: the usual 1 Mbit part.
    pub const ID: [u8; 8] = [0x11, 0x11, 0x80, 0x01, 0x00, 0xC2, 0x00, 0x1E];

    /// Bytes in one page, the unit of a program command.
    const PAGE_LEN: usize = 128;
    /// Bytes cleared by a sector erase: 128 pages.
    const SECTOR_LEN: usize = 128 * Self::PAGE_LEN;
    /// Bits of a command word that hold the page number.
    const PAGE_MASK: u32 = 0xFFFF;

    fn command(&mut self, w: u32) {
        self.used = true;
        let page = (w & Self::PAGE_MASK) as usize * Self::PAGE_LEN;
        match w >> 24 {
            0x4B => {
                self.erase = if page < Self::SIZE {
                    // The part (and libultra osFlashSectorErase) clears the whole sector holding the page.
                    (page - page % Self::SECTOR_LEN, Self::SECTOR_LEN)
                } else {
                    // Nothing to erase, so the next 0x78 changes no bytes.
                    flash_past_end("sector erase", page, Self::SECTOR_LEN);
                    (0, 0)
                }
            }
            0x3C => self.erase = (0, Self::SIZE),
            0x78 => {
                let (offset, len) = self.erase;
                self.ops.push(FlashOp::Erase { offset, len });
                self.status = 0x08;
            }
            0xA5 => {
                if fits(page, Self::PAGE_LEN) {
                    self.ops.push(FlashOp::Program { offset: page });
                } else {
                    flash_past_end("page program", page, Self::PAGE_LEN);
                }
                // Status still reads as done, so a game polling it does not hang.
                self.status = 0x04;
            }
            m => self.mode = m as u8,
        }
    }

    /// The word read at the status address: the status, or the id.
    fn word(&self, k: usize) -> u32 {
        let w = if self.mode == 0xE1 {
            Self::ID
        } else {
            [0x11, 0x11, 0x80, self.status, 0x11, 0x11, 0x80, self.status]
        };
        u32::from_be_bytes(w[k % 2 * 4..k % 2 * 4 + 4].try_into().unwrap())
    }
}

/// Whether `len` bytes from `offset` lie inside the flash chip.
fn fits(offset: usize, len: usize) -> bool {
    offset + len <= Flash::SIZE
}

/// Warns about a flash command whose page lies past the chip; the command
/// is ignored, since what the real part does with such a page is not known.
#[cold]
fn flash_past_end(what: &str, offset: usize, len: usize) {
    eprintln!(
        "warning: flash {what} at {offset:#x}+{len:#x} is past the {:#x}-byte chip, ignored",
        Flash::SIZE
    );
}

/// Guest RAM, stored big-endian as on the console.
///
/// Virtual addresses are mapped by dropping the top three bits, which covers
/// the cached (KSEG0, `0x8000_0000`) and uncached (KSEG1, `0xA000_0000`)
/// windows; other addresses go through the TLB pages the game has written, if
/// one covers them. Accesses outside RAM and misaligned
/// accesses panic with the address, since on hardware they raise exceptions
/// that recompiled code is not expected to rely on.
pub struct Memory {
    bytes: Vec<u8>,
    io: Option<Box<Io>>,
    /// Valid TLB pages as (virtual base, length, physical base); see
    /// [`Memory::set_map`].
    map: Vec<(u32, u32, u32)>,
    /// Index in `map` of the page that matched last.
    last: AtomicUsize,
    /// See [`Memory::map_version`].
    map_version: u64,
}

impl Clone for Memory {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            io: self.io.clone(),
            map: self.map.clone(),
            last: AtomicUsize::new(self.last.load(Ordering::Relaxed)),
            map_version: self.map_version,
        }
    }
}

impl Memory {
    /// Zeroed RAM of `size` bytes (4 MiB, or 8 MiB with the expansion).
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
            io: None,
            map: Vec::new(),
            last: AtomicUsize::new(0),
            map_version: 0,
        }
    }

    /// RAM with the given contents, in big-endian order.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            io: None,
            map: Vec::new(),
            last: AtomicUsize::new(0),
            map_version: 0,
        }
    }

    /// Sets the TLB pages addresses outside KSEG0/KSEG1 go through, as
    /// (virtual base, length, physical base).
    pub fn set_map(&mut self, map: Vec<(u32, u32, u32)>) {
        self.map = map;
        self.map_version += 1;
    }

    /// How many times [`Memory::set_map`] has been called: a copy of the
    /// pages taken at one version is still right while it stays the same.
    pub fn map_version(&self) -> u64 {
        self.map_version
    }

    /// The pages given to [`Memory::set_map`].
    pub fn map(&self) -> &[(u32, u32, u32)] {
        &self.map
    }

    /// The KSEG0 address of the same byte as `vaddr` when a TLB page maps
    /// it; any other address unchanged.
    #[inline(always)]
    pub fn kseg0(&self, vaddr: u32) -> u32 {
        if self.map.is_empty() || direct(vaddr) {
            return vaddr;
        }
        self.mapped(vaddr)
    }

    #[inline(never)]
    fn mapped(&self, vaddr: u32) -> u32 {
        self.page(vaddr)
            .map_or(vaddr, |(base, _, phys)| KSEG0 | (phys + (vaddr - base)))
    }

    /// The TLB page that covers `vaddr`, if any; the one that matched last
    /// is tried first, since most accesses fall in it.
    pub fn page(&self, vaddr: u32) -> Option<(u32, u32, u32)> {
        let k = self.last.load(Ordering::Relaxed);
        if let Some(&p) = self.map.get(k).filter(|p| vaddr.wrapping_sub(p.0) < p.1) {
            return Some(p);
        }
        let k = self
            .map
            .iter()
            .position(|&(base, len, _)| vaddr.wrapping_sub(base) < len)?;
        self.last.store(k, Ordering::Relaxed);
        Some(self.map[k])
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
        // RDRAM past what is installed is not memory: libultra's
        // osGetMemSize probes there and must not read back what it wrote.
        if p < RDRAM_WINDOW_END {
            past_ram(vaddr, "read as 0");
            out.fill(0);
            return;
        }
        for (k, b) in out.iter_mut().enumerate() {
            let a = p + k;
            *b = match a {
                0x0400_0000..=0x0400_1FFF => io.sp_mem[a - 0x0400_0000],
                FLASH_STATUS..=0x0800_0007 if io.flash.used => {
                    let w = io.flash.word((a - FLASH_STATUS) / 4);
                    (w >> (8 * (3 - (a & 3)))) as u8
                }
                0x1000_0000..=0x1FBF_FFFF => io.rom.get(a - 0x1000_0000).copied().unwrap_or(0),
                // SP_STATUS: HALT. Each RSP task is done at once, so the RSP
                // is always idle; what the game wrote there are set/clear
                // commands, not the status.
                0x0404_0010..=0x0404_0013 => u8::from(a == 0x0404_0013),
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
        if p < RDRAM_WINDOW_END {
            past_ram(vaddr, "write dropped");
            return;
        }
        if let Ok(w) = <[u8; 4]>::try_from(data) {
            let w = u32::from_be_bytes(w);
            match p {
                // PI_WR_LEN: a cartridge copy started with the registers
                // themselves, from PI_CART_ADDR to PI_DRAM_ADDR. Done at once.
                0x0460_000C => {
                    let reg = |a: u32| io.regs.get(&a).copied().unwrap_or(0) as usize;
                    let (ram, cart) = (reg(0x0460_0000) & 0xFF_FFFF, reg(0x0460_0004));
                    let n = (w as usize & 0xFF_FFFF) + 1;
                    // Skipped copies already warned inside pi_dma.
                    pi_dma(&io.rom, &mut self.bytes, cart & 0x1FFF_FFFF, ram, n);
                }
                FLASH_COMMAND => io.flash.command(w),
                // Clearing the status.
                FLASH_STATUS if io.flash.used => io.flash.status = 0,
                _ => {}
            }
        }
        for (k, &b) in data.iter().enumerate() {
            let a = p + k;
            match a {
                0x0400_0000..=0x0400_1FFF => io.sp_mem[a - 0x0400_0000] = b,
                // PI_STATUS: a write resets or clears the interrupt; it
                // reads as idle, since copies are done at once.
                0x0460_0010..=0x0460_0013 => {}
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
    fn phys(&self, vaddr: u32, len: usize) -> usize {
        let p = (self.kseg0(vaddr) & PHYS_MASK) as usize;
        if p & (len - 1) != 0 {
            misaligned(vaddr, len);
        }
        p
    }

    #[inline(always)]
    fn get<const N: usize>(&self, vaddr: u32) -> [u8; N] {
        let p = self.phys(vaddr, N);
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
        let p = self.phys(vaddr, N);
        if p + N <= self.bytes.len() {
            self.bytes[p..p + N].copy_from_slice(&b);
        } else {
            self.io_put(vaddr, p, &b);
        }
    }

    /// The byte at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn read_u8(&self, a: u32) -> u8 {
        self.get::<1>(a)[0]
    }
    /// The big-endian halfword at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn read_u16(&self, a: u32) -> u16 {
        u16::from_be_bytes(self.get(a))
    }
    /// The big-endian word at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn read_u32(&self, a: u32) -> u32 {
        u32::from_be_bytes(self.get(a))
    }
    /// The big-endian doubleword at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn read_u64(&self, a: u32) -> u64 {
        u64::from_be_bytes(self.get(a))
    }
    /// Stores `v` as the byte at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn write_u8(&mut self, a: u32, v: u8) {
        self.put(a, [v]);
    }
    /// Stores `v` as the big-endian halfword at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn write_u16(&mut self, a: u32, v: u16) {
        self.put(a, v.to_be_bytes());
    }
    /// Stores `v` as the big-endian word at guest address `a`, which must be aligned to its size.
    #[inline(always)]
    pub fn write_u32(&mut self, a: u32, v: u32) {
        self.put(a, v.to_be_bytes());
    }
    /// Stores `v` as the big-endian doubleword at guest address `a`, which must be aligned to its size.
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

/// Physical address of the cartridge ROM on the PI bus.
const CART_ROM: usize = 0x1000_0000;

/// A PI copy of `n` bytes from physical cartridge address `cart` to RAM
/// offset `ram`. Only the cartridge ROM is modelled, so a copy from another
/// PI domain or one that runs past the end of ROM or RAM is skipped with a
/// warning instead of copying part of it. Returns whether the copy was made.
fn pi_dma(rom: &[u8], bytes: &mut [u8], cart: usize, ram: usize, n: usize) -> bool {
    let Some(src) = cart.checked_sub(CART_ROM) else {
        eprintln!("warning: PI copy from {cart:#010x}, below the cartridge ROM, skipped");
        return false;
    };
    match (rom.get(src..src + n), bytes.get_mut(ram..ram + n)) {
        (Some(s), Some(d)) => {
            d.copy_from_slice(s);
            true
        }
        _ => {
            eprintln!(
                "warning: PI copy of {n:#x} bytes from ROM {src:#x} to RAM {ram:#x} is past the \
                 end of ROM ({:#x}) or RAM ({:#x}), skipped",
                rom.len(),
                bytes.len()
            );
            false
        }
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
    direct(vaddr)
}

/// Extra words for a crash at an address that is really an instruction or
/// data read as a pointer.
pub(crate) fn wild_hint(addr: u32) -> &'static str {
    if addr >> 16 == 0x27BD {
        " (looks like the instruction addiu sp, sp, n: a pointer overwritten with code)"
    } else if !direct(addr) {
        " (not a KSEG0/KSEG1 address: a wild or overwritten pointer, or TLB-mapped memory)"
    } else {
        ""
    }
}

/// Warns about an access to RDRAM addresses past the RAM installed. It is
/// not an error, since the memory-size probe makes such accesses on purpose.
#[cold]
fn past_ram(vaddr: u32, what: &str) {
    eprintln!("warning: guest access at {vaddr:#010x} is past the end of RAM, {what}");
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
            flash: Flash::default(),
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
            flash: Flash::default(),
        });
        m.write_u32(0xC000_0040, 1);
    }

    #[test]
    fn flash_commands_and_status() {
        let mut m = ram();
        m.set_io(Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(vec![]),
            regs: HashMap::new(),
            flash: Flash::default(),
        });
        // Before any command the domain is plain registers (SRAM games).
        assert_eq!(m.read_u32(0xA800_0000), 0);
        m.write_u32(0xA801_0000, 0xE100_0000);
        assert_eq!(m.read_u32(0xA800_0000), 0x1111_8001);
        assert_eq!(m.read_u32(0xA800_0004), 0x00C2_001E);
        m.write_u32(0xA801_0000, 0x4B00_0080);
        m.write_u32(0xA801_0000, 0x7800_0000);
        m.write_u32(0xA801_0000, 0xD200_0000);
        assert_eq!(m.read_u32(0xA800_0000), 0x1111_8008);
        m.write_u32(0xA800_0000, 0);
        assert_eq!(m.read_u32(0xA800_0000), 0x1111_8000);
        m.write_u32(0xA801_0000, 0xB400_0000);
        m.write_u32(0xA801_0000, 0xA500_0081);
        let f = &m.io_mut().unwrap().flash;
        assert_eq!(f.mode, 0xB4);
        assert_eq!(
            f.ops,
            [
                FlashOp::Erase {
                    offset: 0x4000,
                    len: 0x4000
                },
                FlashOp::Program { offset: 0x4080 }
            ]
        );
    }

    #[test]
    fn flash_pages_past_the_chip_are_ignored() {
        let mut f = Flash::default();
        // Last whole sector and last page: still inside the chip.
        f.command(0x4B00_0380);
        f.command(0x7800_0000);
        f.command(0xA500_03FF);
        // A page inside the last sector erases that whole sector.
        f.command(0x4B00_03FF);
        f.command(0x7800_0000);
        // A sector and a page past the end.
        f.command(0x4B00_0400);
        f.command(0x7800_0000);
        f.command(0xA500_FFFF);
        assert_eq!(f.status, 0x04);
        assert_eq!(
            f.ops,
            [
                FlashOp::Erase {
                    offset: 0x1_C000,
                    len: 0x4000
                },
                FlashOp::Program { offset: 0x1_FF80 },
                FlashOp::Erase {
                    offset: 0x1_C000,
                    len: 0x4000
                },
                FlashOp::Erase { offset: 0, len: 0 },
            ]
        );
    }

    fn rom_io(rom: Vec<u8>) -> Io {
        Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(rom),
            regs: HashMap::new(),
            flash: Flash::default(),
        }
    }

    /// Starts a PI copy of `n` bytes from physical `cart` to RAM `ram`.
    fn pi_copy(m: &mut Memory, cart: u32, ram: u32, n: u32) {
        m.write_u32(0xA460_0000, ram);
        m.write_u32(0xA460_0004, cart);
        m.write_u32(0xA460_000C, n - 1);
    }

    #[test]
    fn pi_copy_from_rom() {
        let mut m = ram();
        m.set_io(rom_io(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        pi_copy(&mut m, 0x1000_0004, 0x10, 4);
        assert_eq!(m.read_u32(0x8000_0010), 0x0506_0708);
    }

    #[test]
    fn pi_copy_out_of_range_is_skipped() {
        let mut m = ram();
        m.set_io(rom_io(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        let before = m.bytes().to_vec();
        // Below the cartridge ROM (the flash domain), past the end of ROM,
        // and past the end of RAM.
        pi_copy(&mut m, 0x0800_0000, 0, 4);
        pi_copy(&mut m, 0x1000_0006, 0, 4);
        pi_copy(&mut m, 0x1000_0000, 0x1E, 4);
        assert_eq!(m.bytes(), &before[..]);
    }

    #[test]
    fn pi_dma_reports_whether_it_copied() {
        let rom = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut ram = [0u8; 0x20];
        // Below the cartridge ROM, past the end of ROM, past the end of RAM.
        assert!(!pi_dma(&rom, &mut ram, 0x0800_0000, 0, 4));
        assert!(!pi_dma(&rom, &mut ram, 0x1000_0006, 0, 4));
        assert!(!pi_dma(&rom, &mut ram, 0x1000_0000, 0x1E, 4));
        assert_eq!(ram, [0; 0x20]);
        assert!(pi_dma(&rom, &mut ram, 0x1000_0004, 0x10, 4));
        assert_eq!(ram[0x10..0x14], [5, 6, 7, 8]);
    }

    #[test]
    fn io_space_when_mapped() {
        let mut m = ram();
        m.set_io(Io {
            sp_mem: vec![0; 0x2000],
            rom: Arc::from(vec![1u8, 2, 3, 4]),
            regs: HashMap::new(),
            flash: Flash::default(),
        });
        m.write_u32(0xA480_0018, 0x1234_5678);
        assert_eq!(m.read_u32(0xA480_0018), 0x1234_5678);
        assert_eq!(m.read_u8(0xA480_001B), 0x78);
        assert_eq!(m.read_u32(0xB000_0000), 0x0102_0304);
        m.write_u16(0xA400_0010, 0xBEEF);
        assert_eq!(m.read_u16(0xA400_0010), 0xBEEF);
    }

    #[test]
    fn rdram_past_the_end_is_not_memory() {
        let mut m = ram();
        m.set_io(io());
        // Just past the 32 bytes of RAM, and the last word of the window.
        for a in [0xA000_0020, 0x803E_FFFC] {
            m.write_u32(a, 0x1234_5678);
            assert_eq!(m.read_u32(a), 0, "{a:#x}");
        }
        assert!(m.io_mut().unwrap().regs.is_empty());
        // The RDRAM registers just above the window are still registers.
        m.write_u32(0xA3F0_0000, 0x1234_5678);
        assert_eq!(m.read_u32(0xA3F0_0000), 0x1234_5678);
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

    /// Flash command words: chip erase select, erase start, status mode,
    /// sector erase select (page in the low bits).
    const FLASH_CHIP_ERASE: u32 = 0x3C00_0000;
    const FLASH_ERASE_START: u32 = 0x7800_0000;
    const FLASH_STATUS_MODE: u32 = 0xD200_0000;
    const FLASH_SECTOR_ERASE: u32 = 0x4B00_0000;
    /// Status byte once an erase is done (what libultra osFlashAllErase checks).
    const FLASH_ERASE_DONE: u8 = 0x08;

    fn io() -> Io {
        rom_io(vec![])
    }

    #[test]
    fn flash_chip_erase_clears_the_whole_chip() {
        // libultra osFlashAllErase: chip erase, erase start, then poll status.
        let mut m = ram();
        m.set_io(io());
        m.write_u32(0xA801_0000, FLASH_CHIP_ERASE);
        m.write_u32(0xA801_0000, FLASH_ERASE_START);
        m.write_u32(0xA801_0000, FLASH_STATUS_MODE);
        assert_eq!(
            m.read_u32(0xA800_0000),
            0x1111_8000 | u32::from(FLASH_ERASE_DONE)
        );
        let f = &m.io_mut().unwrap().flash;
        assert_eq!(
            f.ops,
            [FlashOp::Erase {
                offset: 0,
                len: Flash::SIZE
            }]
        );
    }

    #[test]
    fn flash_chip_erase_replaces_an_earlier_sector_erase_select() {
        let mut f = Flash::default();
        f.command(FLASH_SECTOR_ERASE | 0x80);
        f.command(FLASH_CHIP_ERASE);
        f.command(FLASH_ERASE_START);
        assert_eq!(f.status, FLASH_ERASE_DONE);
        assert_eq!(
            f.ops,
            [FlashOp::Erase {
                offset: 0,
                len: Flash::SIZE
            }]
        );
    }

    #[test]
    fn flash_chip_erase_ignores_the_page_bits() {
        let mut f = Flash::default();
        f.command(FLASH_CHIP_ERASE | 0xFFFF);
        f.command(FLASH_ERASE_START);
        assert_eq!(
            f.ops,
            [FlashOp::Erase {
                offset: 0,
                len: Flash::SIZE
            }]
        );
    }

    /// SP_STATUS through KSEG1, and its HALT bit (RCP manual / libultra rcp.h).
    const SP_STATUS: u32 = 0xA404_0010;
    const SP_STATUS_HALT: u32 = 0x1;
    /// Written to SP_STATUS: clear HALT and BROKE, as osSpTaskStartGo does.
    const SP_CLR_HALT_AND_BROKE: u32 = 0x1 | 0x4;

    #[test]
    fn sp_status_reads_halt_before_any_task() {
        let mut m = ram();
        m.set_io(io());
        assert_ne!(m.read_u32(SP_STATUS) & SP_STATUS_HALT, 0);
    }

    #[test]
    fn sp_status_reads_halt_again_once_a_started_task_is_done() {
        // Tasks finish at once here, so the RSP is halted between tasks; the
        // word the game wrote is a set/clear command, not the status.
        let mut m = ram();
        m.set_io(io());
        m.write_u32(SP_STATUS, SP_CLR_HALT_AND_BROKE);
        let status = m.read_u32(SP_STATUS);
        assert_ne!(status & SP_STATUS_HALT, 0);
        // Bit 2 of the written word is CLR_BROKE; as a status bit it would be
        // SSTEP, which nothing set. BROKE itself is not checked: a task that
        // ends with BREAK sets it on hardware.
        assert_eq!(status & 0x4, 0, "{status:#x}");
    }

    #[test]
    fn sp_status_halt_is_in_the_low_byte() {
        let mut m = ram();
        m.set_io(io());
        assert_ne!(u32::from(m.read_u8(SP_STATUS + 3)) & SP_STATUS_HALT, 0);
        assert_eq!(m.read_u8(SP_STATUS), 0);
    }
}
