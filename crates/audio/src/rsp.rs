//! Interpreter for audio command lists in the standard audio microcode's
//! command set.
//!
//! Each command is two big-endian words: the top byte of the first word is
//! the opcode. Commands work on a 4 KiB data memory (DMEM) holding 16-bit
//! big-endian samples, and move data to and from guest RAM.
//!
//! The per-voice state blocks the game allocates in RAM (ADPCM, resampler,
//! envelope mixer) are opaque to the game, so the resampler and envelope
//! mixer keep their own layout in them. The ADPCM state is not opaque: the
//! game copies loop states from its sample banks, so it holds the last 16
//! decoded samples as on hardware.

use n64rusty_runtime::Memory;

const DMEM: usize = 0x1000;
const MASK: usize = DMEM - 1;

// Command flags as the audio library sets them.
const A_INIT: u8 = 0x01;
const A_LOOP: u8 = 0x02;
const A_LEFT: u8 = 0x02;
const A_VOL: u8 = 0x04;
const A_AUX: u8 = 0x08;

/// Host stand-in for the RSP running the audio microcode.
///
/// Keep one per game: like the RSP, it keeps segment bases, buffer settings,
/// volumes and the ADPCM codebook between tasks.
pub struct AudioRsp {
    dmem: Box<[u8; DMEM]>,
    segments: [u32; 16],
    inp: u16,
    out: u16,
    count: u16,
    dry_right: u16,
    wet_left: u16,
    wet_right: u16,
    vol: [i16; 2],
    target: [i16; 2],
    rate: [i32; 2],
    dry: i16,
    wet: i16,
    loop_addr: u32,
    book: [i16; 256],
}

impl Default for AudioRsp {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRsp {
    pub fn new() -> Self {
        Self {
            dmem: Box::new([0; DMEM]),
            segments: [0; 16],
            inp: 0,
            out: 0,
            count: 0,
            dry_right: 0,
            wet_left: 0,
            wet_right: 0,
            vol: [0; 2],
            target: [0; 2],
            rate: [0; 2],
            dry: 0,
            wet: 0,
            loop_addr: 0,
            book: [0; 256],
        }
    }

    /// Runs the `len`-byte command list at `list` in guest RAM, as the RSP
    /// does for an audio task. Panics on a command it does not implement,
    /// naming it, so the first one a game uses is easy to find.
    pub fn run(&mut self, ram: &mut Memory, list: u32, len: u32) {
        for at in (list..list + len).step_by(8) {
            self.command(ram, ram.read_u32(at), ram.read_u32(at + 4));
        }
    }

    fn command(&mut self, ram: &mut Memory, w0: u32, w1: u32) {
        let flags = (w0 >> 16) as u8;
        let lo0 = w0 as u16;
        let (hi1, lo1) = ((w1 >> 16) as u16, w1 as u16);
        match w0 >> 24 {
            0 => {}
            1 => self.adpcm(ram, flags, self.addr(w1)),
            2 => (0..lo1).for_each(|i| self.dmem[(lo0 + i) as usize & MASK] = 0),
            3 => self.envmixer(ram, flags, self.addr(w1)),
            4 => {
                let src = self.addr(w1);
                for i in 0..self.count {
                    self.dmem[(self.inp + i) as usize & MASK] = ram.read_u8(src + i as u32);
                }
            }
            5 => self.resample(ram, flags, lo0, self.addr(w1)),
            6 => {
                let dst = self.addr(w1);
                for i in 0..self.count {
                    ram.write_u8(dst + i as u32, self.dmem[(self.out + i) as usize & MASK]);
                }
            }
            7 => self.segments[(w1 >> 24) as usize & 0xF] = w1 & 0xFF_FFFF,
            8 if flags & A_AUX != 0 => {
                (self.dry_right, self.wet_left, self.wet_right) = (lo0, hi1, lo1)
            }
            8 => (self.inp, self.out, self.count) = (lo0, hi1, lo1),
            9 if flags & A_AUX != 0 => (self.dry, self.wet) = (lo0 as i16, lo1 as i16),
            9 => {
                let ch = usize::from(flags & A_LEFT == 0);
                if flags & A_VOL != 0 {
                    self.vol[ch] = lo0 as i16;
                } else {
                    (self.target[ch], self.rate[ch]) = (lo0 as i16, w1 as i32);
                }
            }
            10 => {
                let bytes: Vec<u8> = (0..lo1)
                    .map(|i| self.dmem[(lo0 + i) as usize & MASK])
                    .collect();
                for (i, b) in bytes.into_iter().enumerate() {
                    self.dmem[(hi1 as usize + i) & MASK] = b;
                }
            }
            11 => {
                let src = self.addr(w1);
                for (i, c) in self.book.iter_mut().take(lo0 as usize / 2).enumerate() {
                    *c = ram.read_u16(src + 2 * i as u32) as i16;
                }
            }
            12 => {
                let gain = lo0 as i16 as i32;
                for i in (0..self.count).step_by(2) {
                    let v = (self.ld(hi1 + i) as i32 * gain) >> 15;
                    self.add(lo1 + i, v);
                }
            }
            13 => {
                let n = self.count / 2;
                let lr: Vec<(i16, i16)> = (0..n)
                    .map(|i| (self.ld(hi1 + 2 * i), self.ld(lo1 + 2 * i)))
                    .collect();
                for (i, (l, r)) in lr.into_iter().enumerate() {
                    let at = self.out + 4 * i as u16;
                    self.st(at, l);
                    self.st(at + 2, r);
                }
            }
            15 => self.loop_addr = self.addr(w1),
            op => panic!("audio command {op:#x} not implemented ({w0:08x} {w1:08x})"),
        }
    }

    /// Segmented address to guest address.
    fn addr(&self, a: u32) -> u32 {
        self.segments[(a >> 24) as usize & 0xF] + (a & 0xFF_FFFF)
    }

    fn ld(&self, a: u16) -> i16 {
        let a = a as usize;
        i16::from_be_bytes([self.dmem[a & MASK], self.dmem[(a + 1) & MASK]])
    }

    fn st(&mut self, a: u16, v: i16) {
        let [h, l] = v.to_be_bytes();
        self.dmem[a as usize & MASK] = h;
        self.dmem[(a as usize + 1) & MASK] = l;
    }

    /// Saturating add of `v` to the sample at `a`.
    fn add(&mut self, a: u16, v: i32) {
        self.st(a, clamp(self.ld(a) as i64 + v as i64));
    }

    /// VADPCM decode: each 9-byte frame is a header (scale shift, predictor
    /// index) and 16 four-bit residuals, decoded 8 at a time with the
    /// predictor's two rows of 8 coefficients (Q11) from the codebook.
    fn adpcm(&mut self, ram: &mut Memory, flags: u8, state: u32) {
        let frames = (self.count as usize).div_ceil(32);
        if frames == 0 {
            return;
        }
        let (mut p2, mut p1) = (0i64, 0i64);
        if flags & A_INIT == 0 {
            let src = if flags & A_LOOP != 0 {
                self.loop_addr
            } else {
                state
            };
            p2 = ram.read_u16(src + 28) as i16 as i64;
            p1 = ram.read_u16(src + 30) as i16 as i64;
        }
        let (mut inp, mut out) = (self.inp, self.out);
        for _ in 0..frames {
            let header = self.dmem[inp as usize & MASK];
            inp += 1;
            let (shift, pred) = (header >> 4, (header & 0xF) as usize);
            let c0: Vec<i64> = self.book[pred * 16..pred * 16 + 8]
                .iter()
                .map(|&c| c as i64)
                .collect();
            let c1: Vec<i64> = self.book[pred * 16 + 8..pred * 16 + 16]
                .iter()
                .map(|&c| c as i64)
                .collect();
            for _ in 0..2 {
                let mut r = [0i64; 8];
                for k in 0..4 {
                    let b = self.dmem[inp as usize & MASK];
                    inp += 1;
                    r[2 * k] = (((b as i8) >> 4) as i64) << shift;
                    r[2 * k + 1] = ((((b << 4) as i8) >> 4) as i64) << shift;
                }
                let mut o = [0i64; 8];
                for i in 0..8 {
                    let mut acc = c0[i] * p2 + c1[i] * p1 + (r[i] << 11);
                    for j in 0..i {
                        acc += c1[j] * r[i - 1 - j];
                    }
                    o[i] = clamp(acc >> 11) as i64;
                    self.st(out, o[i] as i16);
                    out += 2;
                }
                (p2, p1) = (o[6], o[7]);
            }
        }
        for i in 0..16u16 {
            ram.write_u16(
                state + 2 * i as u32,
                self.ld(out.wrapping_sub(32) + 2 * i) as u16,
            );
        }
    }

    /// Pitch shift by `pitch` (Q1.15, 0x8000 = unchanged) with 4-point
    /// Catmull-Rom interpolation.
    ///
    /// shortcut: hardware uses a filter table built into the microcode that
    /// is not reproduced here, so output is close but not bit-exact; replace
    /// the interpolation if a test against hardware captures ever needs it.
    ///
    /// State: 4 history samples then the 16-bit position fraction.
    fn resample(&mut self, ram: &mut Memory, flags: u8, pitch: u16, state: u32) {
        let mut s: Vec<i32> = vec![0; 4];
        let mut pos = 0u64;
        if flags & A_INIT == 0 {
            for (i, h) in s.iter_mut().enumerate() {
                *h = ram.read_u16(state + 2 * i as u32) as i16 as i32;
            }
            pos = ram.read_u16(state + 8) as u64;
        }
        let step = pitch as u64 * 2;
        let n_out = self.count / 2;
        let n_in = ((pos + step * n_out as u64) >> 16) as u16;
        s.extend((0..n_in).map(|i| self.ld(self.inp + 2 * i) as i32));
        for j in 0..n_out {
            let i = (pos >> 16) as usize;
            let t = (pos & 0xFFFF) as f32 / 65536.0;
            let [a, b, c, d] = [s[i], s[i + 1], s[i + 2], s[i + 3]].map(|v| v as f32);
            let v = b + 0.5
                * t
                * (c - a + t * (2.0 * a - 5.0 * b + 4.0 * c - d + t * (3.0 * (b - c) + d - a)));
            self.st(self.out + 2 * j, clamp(v.round() as i64));
            pos += step;
        }
        let n = n_in as usize;
        for (i, &h) in s[n..n + 4].iter().enumerate() {
            ram.write_u16(state + 2 * i as u32, h as u16);
        }
        ram.write_u16(state + 8, pos as u16);
    }

    /// Volume envelope and stereo mix: each mono input sample is scaled by
    /// the left and right volumes, then added with the dry gain to the main
    /// left/right buffers and, with `A_AUX`, with the wet gain to the effect
    /// buffers. Volumes ramp every 8 samples by multiplying by their rate
    /// (Q16) until they reach their target.
    ///
    /// State: volume (Q16) for left and right, targets, rates, dry, wet.
    fn envmixer(&mut self, ram: &mut Memory, flags: u8, state: u32) {
        let mut vol = self.vol.map(|v| (v as i64) << 16);
        let (mut target, mut rate, mut dry, mut wet) = (self.target, self.rate, self.dry, self.wet);
        if flags & A_INIT == 0 {
            let w = |i: u32| ram.read_u32(state + 4 * i);
            vol = [w(0) as i32 as i64, w(1) as i32 as i64];
            target = [w(2) as i16, w(3) as i16];
            rate = [w(4) as i32, w(5) as i32];
            (dry, wet) = (w(6) as i16, w(7) as i16);
        }
        let outs = [self.out, self.dry_right, self.wet_left, self.wet_right];
        for i in 0..self.count / 2 {
            let x = self.ld(self.inp + 2 * i) as i64;
            for ch in 0..2 {
                let g = (x * (vol[ch] >> 16)) >> 15;
                self.add(outs[ch] + 2 * i, ((g * dry as i64) >> 15) as i32);
                if flags & A_AUX != 0 {
                    self.add(outs[2 + ch] + 2 * i, ((g * wet as i64) >> 15) as i32);
                }
            }
            if i % 8 == 7 {
                for ch in 0..2 {
                    let (v, t) = ((vol[ch] * rate[ch] as i64) >> 16, (target[ch] as i64) << 16);
                    vol[ch] = if rate[ch] >= 0x1_0000 {
                        v.min(t)
                    } else {
                        v.max(t)
                    };
                }
            }
        }
        let words = [
            vol[0] as u32,
            vol[1] as u32,
            target[0] as u32,
            target[1] as u32,
            rate[0] as u32,
            rate[1] as u32,
            dry as u32,
            wet as u32,
        ];
        for (i, w) in words.into_iter().enumerate() {
            ram.write_u32(state + 4 * i as u32, w);
        }
    }
}

fn clamp(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}
