//! Interpreter for audio command lists, in the command sets the system
//! library's audio code uses: the standard set, and the set of the
//! alternative `n_audio` library, whose microcode works on a fixed DMEM
//! layout and packs buffer offsets and counts into each command.
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

use crate::synth::Streams;

const DMEM: usize = 0x1000;
const MASK: usize = DMEM - 1;

// Command flags as the audio library sets them.
const A_INIT: u8 = 0x01;
const A_LOOP: u8 = 0x02;
const A_LEFT: u8 = 0x02;
const A_VOL: u8 = 0x04;
const A_AUX: u8 = 0x08;
/// ADPCM with two-bit residuals (5-byte frames), in the third command set.
const A_SMALL: u8 = 0x04;

/// Host stand-in for the RSP running the audio microcode.
///
/// Keep one per game: like the RSP, it keeps segment bases, buffer settings,
/// volumes and the ADPCM codebook between tasks.
pub struct AudioRsp {
    n_audio: bool,
    /// Run the stream commands one game's `n_audio` microcode adds.
    streams: bool,
    /// Their state: the synthesis window and each stream's history.
    stream_state: Streams,
    /// The running microcode's data, which holds the synthesis window.
    ucode_data: u32,
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
    /// Third command set: envelope volumes (left, right, wet; Q16) and the
    /// steps they move by every 8 samples.
    env: [u16; 3],
    env_step: [u16; 3],
    /// Opcodes already warned of as not implemented, so each warns once.
    warned: [bool; 256],
}

impl Default for AudioRsp {
    fn default() -> Self {
        Self::new()
    }
}

// Fixed DMEM layout of the `n_audio` microcode: every command works on one
// subframe of 184 samples.
const N_COUNT: u16 = 0x170;
const N_MAIN: u16 = 0x4F0;
const N_MAIN2: u16 = 0x660;
const N_DRY_LEFT: u16 = 0x9D0;
const N_DRY_RIGHT: u16 = 0xB40;
const N_WET_LEFT: u16 = 0xCB0;
const N_WET_RIGHT: u16 = 0xE20;

impl AudioRsp {
    /// For games using the standard audio library.
    pub fn new() -> Self {
        Self {
            n_audio: false,
            streams: false,
            stream_state: Streams::default(),
            ucode_data: 0,
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
            env: [0; 3],
            env_step: [0; 3],
            warned: [false; 256],
        }
    }

    /// For games using the alternative `n_audio` library.
    pub fn n_audio() -> Self {
        Self {
            n_audio: true,
            ..Self::new()
        }
    }

    /// The `n_audio` set as one game's own microcode extends it, with
    /// commands 7 and 8: the last step of decoding its compressed (MP3)
    /// streams, see the `synth` module. Only a game that asks for it gets
    /// this; for any other, 7 and 8 stay unknown commands. Name the running
    /// microcode's data with [`AudioRsp::set_microcode_data`].
    pub fn n_audio_streams() -> Self {
        Self {
            streams: true,
            ..Self::n_audio()
        }
    }

    /// The data of the microcode the coming tasks run (the task's own
    /// pointer to it), where the stream commands find their window.
    pub fn set_microcode_data(&mut self, data: u32) {
        self.ucode_data = data;
    }

    /// Runs the `len`-byte command list at `list` in guest RAM, as the RSP
    /// does for an audio task. A command it does not implement is skipped,
    /// with a warning naming it the first time each opcode is seen.
    pub fn run(&mut self, ram: &mut Memory, list: u32, len: u32) {
        // A third command set (opcodes up to 0x19, fixed fields, no
        // segments) is told apart by its list: the other two stop at 0x0F.
        let third = !self.n_audio
            && (list..list + len)
                .step_by(8)
                .any(|at| (0x10..=0x19).contains(&(ram.read_u32(at) >> 24)));
        for at in (list..list + len).step_by(8) {
            let (w0, w1) = (ram.read_u32(at), ram.read_u32(at + 4));
            if third {
                self.third_command(ram, w0, w1);
            } else if self.n_audio {
                self.n_command(ram, w0, w1);
            } else {
                self.command(ram, w0, w1);
            }
        }
    }

    fn command(&mut self, ram: &mut Memory, w0: u32, w1: u32) {
        let flags = (w0 >> 16) as u8;
        let lo0 = w0 as u16;
        let (hi1, lo1) = ((w1 >> 16) as u16, w1 as u16);
        let (inp, out, count) = (self.inp, self.out, self.count);
        match w0 >> 24 {
            0 => {}
            1 => self.adpcm(ram, flags & !A_SMALL, self.addr(w1), inp, out, count),
            2 => self.clear(lo0, lo1),
            3 => {
                let outs = [out, self.dry_right, self.wet_left, self.wet_right];
                self.envmix_exp(ram, flags, self.addr(w1), inp, outs, count);
            }
            4 => self.load(ram, self.addr(w1), inp, count),
            5 => self.resample(ram, flags, lo0, self.addr(w1), inp, out, count),
            6 => self.save(ram, self.addr(w1), out, count),
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
            10 => self.dmem_move(lo0, hi1, lo1),
            11 => self.load_book(ram, self.addr(w1), lo0),
            12 => self.mix(lo0 as i16, hi1, lo1, count),
            13 => self.interleave(out, hi1, lo1, count),
            14 => self.pole_filter(ram, flags, lo0, (inp, out, count), self.addr(w1)),
            15 => self.loop_addr = self.addr(w1),
            _ => self.unknown("audio", w0, w1),
        }
    }

    /// The `n_audio` command set. Opcodes keep their standard meanings, but
    /// buffers are offsets from the main buffer, counts are fixed at one
    /// subframe, addresses are physical, and there is no SETBUFF or SEGMENT.
    /// Opcode 14 is the pole filter, the form games have been seen to send.
    fn n_command(&mut self, ram: &mut Memory, w0: u32, w1: u32) {
        let flags = (w0 >> 16) as u8;
        let lo0 = w0 as u16;
        let (hi1, lo1) = ((w1 >> 16) as u16, w1 as u16);
        let a0 = w0 & 0xFF_FFFF;
        let a1 = w1 & 0xFF_FFFF;
        // Offset and count fields of LOADBUFF and SAVEBUFF.
        let (off12, count12) = (N_MAIN + (w0 & 0xFFF) as u16, ((w0 >> 12) & 0xFFF) as u16);
        match w0 >> 24 {
            0 => {}
            1 => {
                let flags = (w1 >> 28) as u8;
                let count = ((w1 >> 16) & 0xFFF) as u16;
                let inp = N_MAIN + ((w1 >> 12) & 0xF) as u16;
                let out = N_MAIN + (w1 & 0xFFF) as u16;
                self.adpcm(ram, flags & !A_SMALL, a0, inp, out, count);
            }
            2 => self.clear(N_MAIN.wrapping_add(lo0), lo1 & 0xFFF),
            3 => {
                self.vol[1] = lo0 as i16;
                self.envmix_lin(ram, flags, a1);
            }
            4 => self.load(ram, a1, off12, count12),
            5 => {
                let flags = (w1 >> 30) as u8;
                let pitch = (w1 >> 14) as u16;
                let inp = N_MAIN + ((w1 >> 2) & 0xFFF) as u16;
                let out = if w1 & 3 != 0 { N_MAIN2 } else { N_MAIN };
                self.resample(ram, flags, pitch, a0, inp, out, N_COUNT);
            }
            6 => self.save(ram, a1, off12, count12),
            9 if flags & A_VOL != 0 && flags & A_LEFT != 0 => {
                (self.vol[0], self.dry, self.wet) = (lo0 as i16, hi1 as i16, lo1 as i16)
            }
            9 if flags & A_VOL != 0 => (self.target[1], self.rate[1]) = (lo0 as i16, w1 as i32),
            9 => (self.target[0], self.rate[0]) = (lo0 as i16, w1 as i32),
            10 => self.dmem_move(
                N_MAIN.wrapping_add(lo0),
                N_MAIN.wrapping_add(hi1),
                round_up4(lo1),
            ),
            11 => self.load_book(ram, a1, lo0),
            12 => self.mix(
                lo0 as i16,
                N_MAIN.wrapping_add(hi1),
                N_MAIN.wrapping_add(lo1),
                N_COUNT,
            ),
            13 => self.interleave(N_MAIN, N_DRY_LEFT, N_DRY_RIGHT, N_COUNT),
            // That game's microcode runs a biquad instead when the first
            // two coefficients are not both zero (its streams' low-pass).
            14 if self.streams && (self.book[0], self.book[1]) != (0, 0) => {
                self.biquad(ram, flags, N_MAIN + (lo0 & 0xFFF), a1);
            }
            14 => {
                let buf = if w1 >> 24 == 0 { N_MAIN } else { N_MAIN2 };
                self.pole_filter(ram, flags, lo0, (buf, buf, N_COUNT), a1);
            }
            15 => self.loop_addr = a1,
            7 if self.streams => self.stream_state.frame(ram, self.ucode_data, a1),
            8 if self.streams => self.stream_state.set_state(a1),
            _ => self.unknown("n_audio", w0, w1),
        }
    }

    /// The third command set. Buffers are set with SETBUFF as in the
    /// standard set, but addresses are physical, LOADBUFF and SAVEBUFF carry
    /// their own buffer and count, and the envelope is set up by two
    /// commands and ramps linearly.
    fn third_command(&mut self, ram: &mut Memory, w0: u32, w1: u32) {
        let flags = (w0 >> 16) as u8;
        let lo0 = w0 as u16;
        let (hi1, lo1) = ((w1 >> 16) as u16, w1 as u16);
        let a1 = w1 & 0xFF_FFFF;
        let count16 = u16::from(flags) * 16;
        let (inp, out, count) = (self.inp, self.out, self.count);
        match w0 >> 24 {
            0 => {}
            1 => self.adpcm(ram, flags, a1, inp, out, count),
            2 => self.clear(lo0, lo1 & 0xFFF),
            // Unknown commands that change no sound buffer anyone has seen.
            3 | 0x19 => {}
            4 => {
                for i in (0..count16).step_by(2) {
                    let v = self.ld(hi1.wrapping_add(i)) as i32;
                    self.add(lo1.wrapping_add(i), v);
                }
            }
            5 => self.resample(ram, flags, lo0, a1, inp, out, count),
            6 => {
                // Nearest earlier sample, no state.
                let step = u32::from(lo0) * 2;
                let mut pos = w1 & 0xFFFF;
                for j in 0..count / 2 {
                    let v = self.ld(inp.wrapping_add(((pos >> 16) as u16).wrapping_mul(2)));
                    self.st(out.wrapping_add(2 * j), v);
                    pos += step;
                }
            }
            // shortcut: the effect filter is left out (samples pass
            // unfiltered); add it if a game's echo sounds wrong.
            7 => {}
            8 => (self.inp, self.out, self.count) = (lo0, hi1, lo1),
            9 => {
                for k in 0..u16::from(flags) {
                    self.dmem_move(lo0, hi1.wrapping_add(128 * k), 128);
                }
            }
            10 => self.dmem_move(lo0, hi1, round_up4(lo1)),
            11 => self.load_book(ram, a1, lo0),
            12 => self.mix(lo0 as i16, hi1, lo1, count16),
            13 => self.interleave(lo0, hi1, lo1, count16),
            14 => {
                // Gain Q4.4.
                let gain = i32::from(flags as i8);
                for i in (0..lo0 & 0xFFF).step_by(2) {
                    let v = (self.ld(hi1.wrapping_add(i)) as i32 * gain) >> 4;
                    self.st(hi1.wrapping_add(i), clamp(v as i64));
                }
            }
            15 => self.loop_addr = a1,
            0x11 => {
                for i in 0..lo0 {
                    let v = self.ld(hi1.wrapping_add(i.wrapping_mul(4)));
                    self.st(lo1.wrapping_add(i.wrapping_mul(2)), v);
                }
            }
            0x12 => {
                self.env[2] = u16::from(flags) << 8;
                self.env_step = [hi1, lo1, lo0];
            }
            0x13 => self.envmix_steps(w0, w1, count),
            0x14 => self.load(ram, a1, (w0 & 0xFFF) as u16, ((w0 >> 12) & 0xFFF) as u16),
            0x15 => self.save(ram, a1, (w0 & 0xFFF) as u16, ((w0 >> 12) & 0xFFF) as u16),
            0x16 => (self.env[0], self.env[1]) = (hi1, lo1),
            0x17 => self.s8_decode(ram, flags, a1, inp, out, count),
            _ => self.unknown("audio", w0, w1),
        }
    }

    /// Skips a command the game sent that this interpreter does not run,
    /// warning once per opcode so a game cannot flood the log.
    fn unknown(&mut self, set: &str, w0: u32, w1: u32) {
        let op = (w0 >> 24) as usize;
        if !std::mem::replace(&mut self.warned[op], true) {
            eprintln!("audio: {set} command {op:#x} not implemented ({w0:08x} {w1:08x}), skipped");
        }
    }

    /// Envelope mixer of the third set: each input sample, scaled by the
    /// left and right volumes (unsigned Q16), is added to the dry buffers and,
    /// scaled again by the wet volume, to the wet ones. Volumes move by their
    /// steps after every 8 samples. `w0` holds the input buffer (in 16-byte
    /// units), the sample count, a left/right swap of the wet outputs and
    /// four bits that invert outputs; `w1` the four outputs (16-byte units).
    fn envmix_steps(&mut self, w0: u32, w1: u32, _count: u16) {
        let inp = ((w0 >> 16) & 0xFF) as u16 * 16;
        let n = (((w0 >> 8) & 0xFF) as u16).div_ceil(8) * 8;
        let flip = |bit: u32, mask: i32| if w0 & bit != 0 { mask } else { 0 };
        let (x0, x1, x2, x3) = (flip(2, -1), flip(1, -1), flip(8, -4), flip(4, -2));
        let mut outs = [24, 16, 8, 0].map(|s| ((w1 >> s) & 0xFF) as u16 * 16);
        if w0 & 0x10 != 0 {
            outs.swap(2, 3);
        }
        for i in 0..n {
            let x = self.ld(inp + 2 * i) as i32;
            let [vl, vr, vw] = self.env.map(i32::from);
            let l = ((x * vl) >> 16) ^ x0;
            let r = ((x * vr) >> 16) ^ x1;
            let l2 = ((l * vw) >> 16) ^ x2;
            let r2 = ((r * vw) >> 16) ^ x3;
            for (o, v) in outs.into_iter().zip([l, r, l2, r2]) {
                let v = clamp(v as i64) as i32;
                self.add(o + 2 * i, v);
            }
            if i % 8 == 7 {
                for k in 0..3 {
                    self.env[k] = self.env[k].wrapping_add(self.env_step[k]);
                }
            }
        }
    }

    /// 8-bit samples to 16-bit, laid out like ADPCM output: the 16 samples
    /// decoded before (or the loop start's), then the new ones.
    fn s8_decode(
        &mut self,
        ram: &mut Memory,
        flags: u8,
        state: u32,
        inp: u16,
        out: u16,
        count: u16,
    ) {
        let mut last = [0i16; 16];
        if flags & A_INIT == 0 {
            let src = if flags & A_LOOP != 0 {
                self.loop_addr
            } else {
                state
            };
            for (i, v) in last.iter_mut().enumerate() {
                *v = ram.read_u16(src + 2 * i as u32) as i16;
            }
        }
        // DMEM addresses wrap: the RSP keeps only the low 12 bits.
        for (i, &v) in last.iter().enumerate() {
            self.st(out.wrapping_add(2 * i as u16), v);
        }
        let n = count.div_ceil(32) * 16;
        let new = out.wrapping_add(32);
        for i in 0..n {
            let b = self.dmem[inp.wrapping_add(i) as usize & MASK] as i8;
            self.st(new.wrapping_add(2 * i), i16::from(b) << 8);
        }
        let end = new.wrapping_add(n.wrapping_mul(2));
        for i in 0..16u16 {
            let at = end.wrapping_sub(32).wrapping_add(2 * i);
            ram.write_u16(state + 2 * i as u32, self.ld(at) as u16);
        }
    }

    // DMEM indexes below wrap before the mask, as the RSP's 12-bit DMEM
    // addressing does.
    fn clear(&mut self, at: u16, count: u16) {
        (0..count).for_each(|i| self.dmem[at.wrapping_add(i) as usize & MASK] = 0);
    }

    fn load(&mut self, ram: &Memory, src: u32, at: u16, count: u16) {
        for i in 0..count {
            self.dmem[at.wrapping_add(i) as usize & MASK] = ram.read_u8(src + i as u32);
        }
    }

    fn save(&self, ram: &mut Memory, dst: u32, at: u16, count: u16) {
        for i in 0..count {
            ram.write_u8(dst + i as u32, self.dmem[at.wrapping_add(i) as usize & MASK]);
        }
    }

    fn dmem_move(&mut self, from: u16, to: u16, count: u16) {
        let bytes: Vec<u8> = (0..count)
            .map(|i| self.dmem[from.wrapping_add(i) as usize & MASK])
            .collect();
        for (i, b) in bytes.into_iter().enumerate() {
            self.dmem[(to as usize + i) & MASK] = b;
        }
    }

    fn load_book(&mut self, ram: &Memory, src: u32, bytes: u16) {
        for (i, c) in self.book.iter_mut().take(bytes as usize / 2).enumerate() {
            *c = ram.read_u16(src + 2 * i as u32) as i16;
        }
    }

    /// Saturating `out += in * gain` (gain Q15) over `count` bytes.
    fn mix(&mut self, gain: i16, inp: u16, out: u16, count: u16) {
        for i in (0..count).step_by(2) {
            let v = (self.ld(inp.wrapping_add(i)) as i32 * gain as i32) >> 15;
            self.add(out.wrapping_add(i), v);
        }
    }

    /// Left and right buffers of `count` bytes each into one stereo buffer.
    fn interleave(&mut self, out: u16, left: u16, right: u16, count: u16) {
        let lr: Vec<(i16, i16)> = (0..count / 2)
            .map(|i| {
                (
                    self.ld(left.wrapping_add(2 * i)),
                    self.ld(right.wrapping_add(2 * i)),
                )
            })
            .collect();
        for (i, (l, r)) in lr.into_iter().enumerate() {
            let at = out.wrapping_add((4 * i) as u16);
            self.st(at, l);
            self.st(at.wrapping_add(2), r);
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
    fn adpcm(&mut self, ram: &mut Memory, flags: u8, state: u32, inp: u16, out: u16, count: u16) {
        let frames = (count as usize).div_ceil(32);
        // The output starts with the last frame decoded before (or the loop
        // start's): the game resamples from part-way into it.
        let mut last = [0i16; 16];
        if flags & A_INIT == 0 {
            let src = if flags & A_LOOP != 0 {
                self.loop_addr
            } else {
                state
            };
            for (i, v) in last.iter_mut().enumerate() {
                *v = ram.read_u16(src + 2 * i as u32) as i16;
            }
        }
        for (i, &v) in last.iter().enumerate() {
            self.st(out.wrapping_add(2 * i as u16), v);
        }
        let (mut p2, mut p1) = (last[14] as i64, last[15] as i64);
        let out = out.wrapping_add(32);
        let (mut inp, mut out) = (inp, out);
        for _ in 0..frames {
            let header = self.dmem[inp as usize & MASK];
            inp = inp.wrapping_add(1);
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
                if flags & A_SMALL != 0 {
                    // Two-bit residuals, four to a byte, high bits first.
                    for k in 0..2 {
                        let b = self.dmem[inp as usize & MASK];
                        inp = inp.wrapping_add(1);
                        for j in 0..4 {
                            let v = ((b << (2 * j)) as i8) >> 6;
                            r[4 * k + j] = (v as i64) << shift;
                        }
                    }
                } else {
                    for k in 0..4 {
                        let b = self.dmem[inp as usize & MASK];
                        inp = inp.wrapping_add(1);
                        r[2 * k] = (((b as i8) >> 4) as i64) << shift;
                        r[2 * k + 1] = ((((b << 4) as i8) >> 4) as i64) << shift;
                    }
                }
                let mut o = [0i64; 8];
                for i in 0..8 {
                    let mut acc = c0[i] * p2 + c1[i] * p1 + (r[i] << 11);
                    for j in 0..i {
                        acc += c1[j] * r[i - 1 - j];
                    }
                    o[i] = clamp(acc >> 11) as i64;
                    self.st(out, o[i] as i16);
                    out = out.wrapping_add(2);
                }
                (p2, p1) = (o[6], o[7]);
            }
        }
        for i in 0..16u16 {
            ram.write_u16(
                state + 2 * i as u32,
                self.ld(out.wrapping_sub(32).wrapping_add(2 * i)) as u16,
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
    #[allow(clippy::too_many_arguments)]
    fn resample(
        &mut self,
        ram: &mut Memory,
        flags: u8,
        pitch: u16,
        state: u32,
        inp: u16,
        out: u16,
        count: u16,
    ) {
        let mut s: Vec<i32> = vec![0; 4];
        let mut pos = 0u64;
        if flags & A_INIT == 0 {
            for (i, h) in s.iter_mut().enumerate() {
                *h = ram.read_u16(state + 2 * i as u32) as i16 as i32;
            }
            pos = ram.read_u16(state + 8) as u64;
        }
        let step = pitch as u64 * 2;
        let n_out = count / 2;
        let n_in = ((pos + step * n_out as u64) >> 16) as u16;
        s.extend((0..n_in).map(|i| self.ld(inp.wrapping_add(i.wrapping_mul(2))) as i32));
        for j in 0..n_out {
            let i = (pos >> 16) as usize;
            let t = (pos & 0xFFFF) as f32 / 65536.0;
            let [a, b, c, d] = [s[i], s[i + 1], s[i + 2], s[i + 3]].map(|v| v as f32);
            let v = b + 0.5
                * t
                * (c - a + t * (2.0 * a - 5.0 * b + 4.0 * c - d + t * (3.0 * (b - c) + d - a)));
            self.st(out.wrapping_add(2 * j), clamp(v.round() as i64));
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
    fn envmix_exp(
        &mut self,
        ram: &mut Memory,
        flags: u8,
        state: u32,
        inp: u16,
        outs: [u16; 4],
        count: u16,
    ) {
        let mut vol = self.vol.map(|v| (v as i64) << 16);
        let (mut target, mut rate, mut dry, mut wet) = (self.target, self.rate, self.dry, self.wet);
        if flags & A_INIT == 0 {
            let w = |i: u32| ram.read_u32(state + 4 * i);
            vol = [w(0) as i32 as i64, w(1) as i32 as i64];
            target = [w(2) as i16, w(3) as i16];
            rate = [w(4) as i32, w(5) as i32];
            (dry, wet) = (w(6) as i16, w(7) as i16);
        }
        for i in 0..count / 2 {
            let x = self.ld(inp.wrapping_add(2 * i)) as i64;
            for ch in 0..2 {
                let g = (x * (vol[ch] >> 16)) >> 15;
                self.add(
                    outs[ch].wrapping_add(2 * i),
                    ((g * dry as i64) >> 15) as i32,
                );
                if flags & A_AUX != 0 {
                    self.add(
                        outs[2 + ch].wrapping_add(2 * i),
                        ((g * wet as i64) >> 15) as i32,
                    );
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

    /// `n_audio` envelope mixer: the main buffer, scaled by left and right
    /// volumes that move linearly towards their targets by `rate / 8` (Q16)
    /// per sample, is added with the dry gain to the dry buffers and with the
    /// wet gain to the wet buffers.
    ///
    /// State: the same layout as the standard mixer, with steps for rates.
    fn envmix_lin(&mut self, ram: &mut Memory, flags: u8, state: u32) {
        let mut vol = self.vol.map(|v| (v as i64) << 16);
        let mut target = self.target.map(|t| (t as i64) << 16);
        let mut step = self.rate.map(|r| (r / 8) as i64);
        let (mut dry, mut wet) = (self.dry, self.wet);
        if flags & A_INIT == 0 {
            let w = |i: u32| ram.read_u32(state + 4 * i) as i32 as i64;
            vol = [w(0), w(1)];
            target = [w(2) << 16, w(3) << 16];
            step = [w(4), w(5)];
            (dry, wet) = (w(6) as i16, w(7) as i16);
        }
        let outs = [N_DRY_LEFT, N_DRY_RIGHT, N_WET_LEFT, N_WET_RIGHT];
        for i in 0..N_COUNT / 2 {
            for ch in 0..2 {
                vol[ch] += step[ch];
                let reached = if step[ch] <= 0 {
                    vol[ch] <= target[ch]
                } else {
                    vol[ch] >= target[ch]
                };
                if reached {
                    (vol[ch], step[ch]) = (target[ch], 0);
                }
            }
            let x = self.ld(N_MAIN + 2 * i) as i32;
            for (k, out) in outs.into_iter().enumerate() {
                let g = if k < 2 { dry } else { wet } as i64;
                let gain = clamp(((vol[k % 2] >> 16) * g + 0x4000) >> 15) as i32;
                self.add(out + 2 * i, (x * gain) >> 15);
            }
        }
        let words = [
            vol[0],
            vol[1],
            target[0] >> 16,
            target[1] >> 16,
            step[0],
            step[1],
            dry as i64,
            wet as i64,
        ];
        for (i, w) in words.into_iter().enumerate() {
            ram.write_u32(state + 4 * i as u32, w as u32);
        }
    }

    /// Biquad over one subframe at `buf`, in place, as one game's microcode
    /// runs its filter command: `y[n] = b0 x[n] + b1 x[n-1] + b0 x[n-2]`
    /// (Q15, coefficients 0 and 1 of the codebook) `+ a1 y[n-1] + a2 y[n-2]`
    /// (Q14, coefficients 8 and 9). The state block keeps `x[n-2], x[n-1],
    /// y[n-2], y[n-1]`.
    fn biquad(&mut self, ram: &mut Memory, flags: u8, buf: u16, state: u32) {
        let b = [self.book[0], self.book[1]].map(i32::from);
        let a = [self.book[8], self.book[9]].map(i32::from);
        assert!(
            b[1] != 0 || self.book[2] != 0,
            "biquad with no b1: a filter form not implemented"
        );
        // Multiply and round as the vector unit does in Q15 (VMULF), in i64
        // so -1 * -1 cannot overflow, and clamped as VMULF clamps that one
        // product to 0x7FFF.
        let q15 = |c: i32, v: i32| {
            i32::from(clamp((i64::from(c) * i64::from(v) * 2 + 0x8000) >> 16))
        };
        let (mut x, mut y) = ([0i32; 2], [0i32; 2]);
        if flags & A_INIT == 0 {
            let h = |k: u32| i32::from(ram.read_u16(state + 2 * k) as i16);
            (x, y) = ([h(0), h(1)], [h(2), h(3)]);
        }
        for i in 0..N_COUNT / 2 {
            let at = buf + 2 * i;
            let xn = i32::from(self.ld(at));
            let num = q15(b[0], xn) + q15(b[1], x[1]) + q15(b[0], x[0]);
            let yn = clamp(i64::from(num + 2 * q15(a[0], y[1]) + 2 * q15(a[1], y[0])));
            self.st(at, yn);
            (x, y) = ([x[1], xn], [y[1], i32::from(yn)]);
        }
        for (k, v) in [x[0], x[1], y[0], y[1]].into_iter().enumerate() {
            ram.write_u16(state + 2 * k as u32, v as u16);
        }
    }

    /// Two-pole filter over one subframe of `buf`, in place, with the two
    /// rows of 8 coefficients loaded into the codebook and `gain` (Q14) on
    /// the input. Like ADPCM decoding it works 8 samples at a time from the
    /// last two outputs.
    ///
    /// State: the last 4 output samples.
    fn pole_filter(
        &mut self,
        ram: &mut Memory,
        flags: u8,
        gain: u16,
        (inp, out, count): (u16, u16, u16),
        state: u32,
    ) {
        let gain = gain as i64;
        let h1: Vec<i64> = self.book[0..8].iter().map(|&c| c as i64).collect();
        let h2: Vec<i64> = self.book[8..16].iter().map(|&c| c as i64).collect();
        let h2g: Vec<i64> = h2.iter().map(|&c| (c * gain) >> 14).collect();
        let (mut l1, mut l2) = (0i64, 0i64);
        if flags & A_INIT == 0 {
            l1 = ram.read_u16(state + 4) as i16 as i64;
            l2 = ram.read_u16(state + 6) as i16 as i64;
        }
        for f in 0..count.div_ceil(16) {
            let (src, at) = (inp.wrapping_add(16 * f), out.wrapping_add(16 * f));
            let x: Vec<i64> = (0..8)
                .map(|i| self.ld(src.wrapping_add(2 * i)) as i64)
                .collect();
            for i in 0..8 {
                let mut acc = x[i] * gain + h1[i] * l1 + h2[i] * l2;
                for k in 0..i {
                    acc += h2g[k] * x[i - 1 - k];
                }
                self.st(at.wrapping_add(2 * i as u16), clamp(acc >> 14));
            }
            (l1, l2) = (
                self.ld(at.wrapping_add(12)) as i64,
                self.ld(at.wrapping_add(14)) as i64,
            );
        }
        let end = out.wrapping_add(count.div_ceil(16).wrapping_mul(16));
        for i in 0..4u16 {
            ram.write_u16(
                state + 2 * i as u32,
                self.ld(end.wrapping_sub(8).wrapping_add(2 * i)) as u16,
            );
        }
    }
}

/// A byte count rounded up to a multiple of 4 in 16 bits, as the microcode
/// computes it: a count near 0xFFFF wraps to 0, a no-op.
fn round_up4(count: u16) -> u16 {
    count.wrapping_add(3) & !3
}

fn clamp(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEAR_TOP: u16 = 0xFFF0;
    const COUNT: u16 = 0x20;

    #[test]
    fn dmem_ranges_near_the_top_of_u16_wrap_to_dmem_start() {
        let mut rsp = AudioRsp::new();
        rsp.dmem.fill(0xAA);
        rsp.clear(NEAR_TOP, COUNT);
        // 0xFFF0..0x10010 wraps to DMEM 0xFF0..0x1000 and 0x000..0x010.
        assert!(rsp.dmem[0xFF0..].iter().all(|&b| b == 0));
        assert!(rsp.dmem[..0x10].iter().all(|&b| b == 0));
        assert_eq!(rsp.dmem[0x10], 0xAA);
        rsp.dmem[0xFF0] = 7;
        rsp.dmem_move(NEAR_TOP, 0x100, COUNT);
        assert_eq!(rsp.dmem[0x100], 7);
        let mut ram = Memory::new(1 << 16);
        ram.write_u8(0x100, 9);
        rsp.load(&ram, 0x100, NEAR_TOP, COUNT);
        assert_eq!(rsp.dmem[0xFF0], 9);
        rsp.save(&mut ram, 0x200, NEAR_TOP, COUNT);
        assert_eq!(ram.read_u8(0x200), 9);
    }

    #[test]
    fn s8_decode_near_the_top_of_u16_wraps() {
        let mut rsp = AudioRsp::new();
        let mut ram = Memory::new(1 << 16);
        rsp.dmem[0xFE0] = 0x12;
        rsp.s8_decode(&mut ram, A_INIT, 0x400, NEAR_TOP - 0x10, NEAR_TOP, COUNT);
        // The new samples start 32 bytes after the history, wrapped.
        assert_eq!(rsp.ld(0x010), 0x1200);
        // A valid range still decodes where it did.
        rsp.dmem[0x100] = 0x34;
        rsp.s8_decode(&mut ram, A_INIT, 0x400, 0x100, 0x200, COUNT);
        assert_eq!(rsp.ld(0x220), 0x3400);
    }

    #[test]
    fn biquad_of_minus_one_by_minus_one_clamps_as_vmulf() {
        let mut rsp = AudioRsp::new();
        let mut ram = Memory::new(1 << 16);
        rsp.book[0] = i16::MIN;
        rsp.book[1] = 0x100;
        rsp.st(N_MAIN, i16::MIN);
        rsp.biquad(&mut ram, A_INIT, N_MAIN, 0x400);
        // b0 * x = -1 * -1 is 0x7FFF, not -0x8000.
        assert_eq!(rsp.ld(N_MAIN), i16::MAX);
        // An ordinary product rounds as before: 0.5 * 0.5 = 0.25.
        rsp.book[0] = 0x4000;
        rsp.st(N_MAIN, 0x4000);
        rsp.biquad(&mut ram, A_INIT, N_MAIN, 0x400);
        assert_eq!(rsp.ld(N_MAIN), 0x2000);
    }

    /// Game fields near the top of u16 for every DMEM address and count.
    const TOP: u16 = 0xFFFE;
    const STATE: u32 = 0x400;

    fn op(op: u32, flags: u8, lo0: u16) -> u32 {
        op << 24 | u32::from(flags) << 16 | u32::from(lo0)
    }

    fn w1(hi1: u16, lo1: u16) -> u32 {
        u32::from(hi1) << 16 | u32::from(lo1)
    }

    #[test]
    fn n_audio_fields_near_the_top_of_u16_wrap() {
        let mut rsp = AudioRsp::n_audio();
        let mut ram = Memory::new(1 << 16);
        rsp.dmem.fill(0xAA);
        rsp.n_command(&mut ram, op(2, 0, NEAR_TOP), u32::from(TOP));
        // N_MAIN + 0xFFF0 wraps to 0x4E0.
        assert_eq!(rsp.dmem[0x4E0], 0);
        // 0xFFE bytes from 0x4E0 leave only 0x4DE and 0x4DF as they were.
        assert_eq!(rsp.dmem[0x4DE], 0xAA);
        let before = rsp.dmem.clone();
        // A count of 0xFFFE rounds up to 0x10000, which wraps to 0: no move.
        rsp.n_command(&mut ram, op(10, 0, NEAR_TOP), w1(NEAR_TOP, TOP));
        assert_eq!(rsp.dmem, before);
        rsp.n_command(&mut ram, op(12, 0, 0x7FFF), w1(NEAR_TOP, TOP));
    }

    #[test]
    fn standard_fields_near_the_top_of_u16_wrap() {
        let mut rsp = AudioRsp::new();
        let mut ram = Memory::new(1 << 16);
        let before = rsp.dmem.clone();
        rsp.command(&mut ram, op(10, 0, NEAR_TOP), w1(NEAR_TOP, TOP));
        assert_eq!(rsp.dmem, before, "a count that wraps to 0 moves nothing");
        rsp.command(&mut ram, op(12, 0, 0x7FFF), w1(NEAR_TOP, TOP));
        // SETBUFF with input, output and count all near the top.
        rsp.command(&mut ram, op(8, 0, NEAR_TOP), w1(NEAR_TOP, TOP));
        rsp.command(&mut ram, op(13, 0, 0), w1(NEAR_TOP, TOP));
        rsp.command(&mut ram, op(1, A_INIT, 0), STATE);
        rsp.command(&mut ram, op(3, A_INIT | A_AUX, 0), STATE);
        rsp.command(&mut ram, op(5, A_INIT, 0xFFFF), STATE);
        rsp.command(&mut ram, op(14, A_INIT, 0x4000), STATE);
    }

    #[test]
    fn third_set_fields_near_the_top_of_u16_wrap() {
        let mut rsp = AudioRsp::new();
        let mut ram = Memory::new(1 << 16);
        let before = rsp.dmem.clone();
        rsp.third_command(&mut ram, op(10, 0, NEAR_TOP), w1(NEAR_TOP, TOP));
        assert_eq!(rsp.dmem, before, "a count that wraps to 0 moves nothing");
        rsp.third_command(&mut ram, op(4, 0xFF, 0), w1(NEAR_TOP, TOP));
        rsp.third_command(&mut ram, op(9, 0xFF, NEAR_TOP), w1(NEAR_TOP, TOP));
        rsp.third_command(&mut ram, op(14, 0x10, 0xFFF), w1(NEAR_TOP, TOP));
        rsp.third_command(&mut ram, op(0x11, 0, 0xFFFF), w1(NEAR_TOP, TOP));
        rsp.third_command(&mut ram, op(8, 0, NEAR_TOP), w1(NEAR_TOP, TOP));
        rsp.third_command(&mut ram, op(6, 0, 0xFFFF), 0xFFFF);
    }

    #[test]
    fn unknown_commands_are_skipped_with_one_warning_each() {
        let mut ram = Memory::new(1 << 16);
        let mut rsp = AudioRsp::new();
        rsp.command(&mut ram, op(0x10, 0, 0), 0);
        rsp.command(&mut ram, op(0x10, 0, 0), 0);
        assert!(rsp.warned[0x10]);
        rsp.third_command(&mut ram, op(0x18, 0, 0), 0);
        assert!(rsp.warned[0x18]);
        let mut rsp = AudioRsp::n_audio();
        rsp.n_command(&mut ram, op(7, 0, 0), 0);
        assert!(rsp.warned[7]);
        assert_eq!(rsp.warned.iter().filter(|&&w| w).count(), 1);
    }
}
