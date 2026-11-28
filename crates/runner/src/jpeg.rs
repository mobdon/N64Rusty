//! The RSP's JPEG task, in Rust: the CPU has already Huffman-decoded each
//! macroblock into quantised coefficients (zigzag order, DC prediction
//! applied); the task dequantises, runs the inverse DCT, converts YCbCr to
//! RGBA 5551 and writes the pixels over the start of each macroblock.
//!
//! Task data: buffer address, macroblock count, mode (0: two luma blocks,
//! 16x8 pixels; 2: four luma blocks, 16x16), then the Y, Cb and Cr
//! quantisation tables (64 u16 each, zigzag order).

use n64rusty_runtime::Memory;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// Bytes of one 8x8 block of 16-bit coefficients.
const BLOCK_BYTES: u32 = 0x80;
/// The physical address bits of a KSEG0 address.
const PHYS_MASK: u32 = 0x1FFF_FFFF;
/// Bytes of one quantisation table: 64 u16 entries.
const TABLE_BYTES: u32 = 2 * 64;
/// The SP DMA takes a 24-bit RAM address and ignores its low 3 bits, so the
/// task's addresses are read the same way: always aligned and inside RAM.
const SP_DRAM_ADDR_MASK: u32 = 0x00FF_FFF8;

/// Runs the task whose data is at `data`.
pub fn run(mem: &mut Memory, data: u32) {
    let rd = |m: &Memory, k: u32| (m.read_u32(data + 4 * k) & SP_DRAM_ADDR_MASK) | 0x8000_0000;
    let (addr, count, mode) = (rd(mem, 0), mem.read_u32(data + 4), mem.read_u32(data + 8));
    let tables = [rd(mem, 3), rd(mem, 4), rd(mem, 5)];
    if let Some(t) = tables
        .iter()
        .find(|&&t| !fits_in_ram(mem, t, 1, TABLE_BYTES))
    {
        eprintln!(
            "warning: JPEG task at {data:#010x} skipped: its quantisation table at {t:#010x} runs past RAM"
        );
        return;
    }
    let q: Vec<[f32; 64]> = tables
        .iter()
        .map(|&t| std::array::from_fn(|i| f32::from(mem.read_u16(t + 2 * i as u32))))
        .collect();
    let luma = if mode == 0 { 2 } else { 4 };
    let stride = BLOCK_BYTES * (luma + 2);
    if !fits_in_ram(mem, addr, count, stride) {
        eprintln!(
            "warning: JPEG task at {data:#010x} skipped: {count} macroblocks of {stride:#x} bytes at {addr:#010x} run past RAM"
        );
        return;
    }
    for mb in 0..count {
        let base = addr + mb * stride;
        let block = |m: &Memory, k: u32, table: usize| {
            let mut c = [0f32; 64];
            for (i, &n) in ZIGZAG.iter().enumerate() {
                c[n] = f32::from(m.read_u16(base + BLOCK_BYTES * k + 2 * i as u32) as i16)
                    * q[table][i];
            }
            idct(&c)
        };
        let ys: Vec<[f32; 64]> = (0..luma).map(|k| block(mem, k, 0)).collect();
        let (cb, cr) = (block(mem, luma, 1), block(mem, luma + 1, 2));
        let height = if mode == 0 { 8 } else { 16 };
        for y in 0..height {
            for x in 0..16usize {
                let yb = &ys[(x / 8) + 2 * (y / 8)];
                let l = yb[(y % 8) * 8 + x % 8] + 128.0;
                let ci = (y * 8 / height) * 8 + x / 2;
                let (u, v) = (cb[ci], cr[ci]);
                let px = |f: f32| (f.round().clamp(0.0, 255.0) as u16) >> 3;
                let (r, g, b) = (
                    px(l + 1.402 * v),
                    px(l - 0.344_136 * u - 0.714_136 * v),
                    px(l + 1.772 * u),
                );
                mem.write_u16(
                    base + 2 * (y * 16 + x) as u32,
                    r << 11 | g << 6 | b << 1 | 1,
                );
            }
        }
    }
}

/// Whether `count` blocks of `stride` bytes from KSEG0 address `addr` all
/// lie in the game's RAM, so a bad task cannot run past it (or into the
/// mods' RAM above it) or loop for ages.
fn fits_in_ram(mem: &Memory, addr: u32, count: u32, stride: u32) -> bool {
    let start = u64::from(addr & PHYS_MASK);
    let end = start + u64::from(count) * u64::from(stride);
    end <= mem.bytes().len().min(crate::ram_size()) as u64
}

/// Inverse DCT of one 8x8 block (natural order), level-shifted: luma comes
/// out as 0..255, chroma centred on 0.
fn idct(c: &[f32; 64]) -> [f32; 64] {
    use std::f32::consts::PI;
    let cos: [[f32; 8]; 8] = std::array::from_fn(|x| {
        std::array::from_fn(|u| {
            let s = if u == 0 {
                std::f32::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            s * ((2 * x + 1) as f32 * u as f32 * PI / 16.0).cos() / 2.0
        })
    });
    std::array::from_fn(|i| {
        let (y, x) = (i / 8, i % 8);
        let mut sum = 0.0;
        for v in 0..8 {
            for u in 0..8 {
                sum += cos[x][u] * cos[y][v] * c[v * 8 + u];
            }
        }
        sum
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_blocks_decode_to_flat_colours() {
        let mut mem = Memory::new(0x10000);
        let (data, buf, q) = (0x8000_0000, 0x8000_1000, 0x8000_0100);
        mem.write_u32(data, buf & 0x1FFF_FFFF);
        mem.write_u32(data + 4, 1);
        mem.write_u32(data + 8, 2);
        for k in 0..3 {
            mem.write_u32(data + 12 + 4 * k, q & 0x1FFF_FFFF);
        }
        for i in 0..64 {
            mem.write_u16(q + 2 * i, 2);
        }
        // Luma DC 0 (mid grey) in the top-left block, 512 (+128 after the
        // 1/8 scale and table 2: white) in the others; no colour.
        for k in 1..4 {
            mem.write_u16(buf + 0x80 * k, 512);
        }
        run(&mut mem, data);
        assert_eq!(mem.read_u16(buf), 0x8421); // grey: 16 in each channel
        assert_eq!(mem.read_u16(buf + 2 * 15), 0xFFFF); // white, top right
    }

    #[test]
    fn a_task_that_runs_past_ram_is_skipped() {
        let mut mem = Memory::new(0x10000);
        let (data, buf) = (0x8000_0000u32, 0x8000_1000u32);
        mem.write_u32(data, buf & PHYS_MASK);
        mem.write_u32(data + 8, 2);
        mem.write_u16(buf, 512);
        for count in [u32::MAX, 0x100] {
            mem.write_u32(data + 4, count);
            run(&mut mem, data);
            assert_eq!(mem.read_u16(buf), 512, "{count} macroblocks: left alone");
        }
        // An address near the top of the 32-bit space cannot wrap into RAM.
        mem.write_u32(data, 0x1FFF_FF00);
        mem.write_u32(data + 4, 1);
        run(&mut mem, data);
        assert!(!fits_in_ram(&mem, 0x9FFF_FF00, 1, 0x300));
    }

    #[test]
    fn a_quantisation_table_past_ram_is_skipped() {
        let mut mem = Memory::new(0x10000);
        let (data, buf) = (0x8000_0000u32, 0x8000_1000u32);
        mem.write_u32(data, buf & PHYS_MASK);
        mem.write_u32(data + 4, 1);
        mem.write_u32(data + 8, 2);
        mem.write_u32(data + 12, 0x0001_0000 - TABLE_BYTES / 2);
        mem.write_u16(buf, 512);
        run(&mut mem, data);
        assert_eq!(mem.read_u16(buf), 512, "left alone");
    }

    #[test]
    fn an_odd_address_reads_aligned_as_the_sp_dma_does() {
        let mut mem = Memory::new(0x10000);
        let (data, buf) = (0x8000_0000u32, 0x8000_1000u32);
        mem.write_u32(data, (buf & PHYS_MASK) | 1);
        mem.write_u32(data + 4, 1);
        mem.write_u32(data + 8, 2);
        mem.write_u32(data + 12, 0x0000_2001);
        mem.write_u32(data + 16, 0x4000_2000);
        mem.write_u32(data + 20, 0x0000_2003);
        // Before, the odd addresses panicked as misaligned reads.
        run(&mut mem, data);
    }

    #[test]
    fn mods_ram_is_not_the_games() {
        // Above the largest game RAM is only ever the mods' RAM.
        let mem = Memory::new(crate::RAM_SIZE_EXPANDED + 0x1000);
        let above = 0x8000_0000 | crate::RAM_SIZE_EXPANDED as u32;
        assert!(!fits_in_ram(&mem, above, 1, 0x100));
        assert!(fits_in_ram(&mem, 0x8000_0000, 1, 0x100));
    }
}
