//! The last step of MP3 decoding, the polyphase synthesis filter, as one
//! game's audio microcode does it for its compressed streams (commands 7
//! and 8 of its `n_audio` set). The game decodes each frame on the CPU down
//! to subband samples; the microcode turns them into PCM.
//!
//! Command 8 names a stream's state block in RAM. Command 7 names a frame
//! buffer: four header words, then 18 time slots of 32 subband samples
//! (16-bit, time slot by time slot). The 576 PCM samples go back over the
//! buffer from its start. The first two header words are the output gain,
//! a whole part and a fraction (Q16.16): the game stores the subband
//! samples scaled down and the microcode scales its output back up. Between frames a stream keeps the filter's
//! history (the synthesis subband filter of MPEG-1 audio, ISO/IEC 11172-3);
//! here it is kept on the host, by state block address, and restarts when
//! the game clears the block.
//!
//! The synthesis window comes from the microcode's own data, so nothing
//! game-specific is stored here: that data holds the window as rows
//! `D[j + 32k]` (k = 0 to 15) for j = 0 to 16, each row stored twice, scaled
//! so that the largest value, `D[256]`, is 32767. The other rows follow from
//! the window's symmetry. Like the microcode, the window is used as stored
//! (Q15), not scaled back to the standard's values.

use n64rusty_runtime::Memory;
use std::sync::OnceLock;

/// Offset of the window's first row in the microcode data.
const WINDOW_ROWS: u32 = 0x310;
/// Bytes from one row to the next (each row is stored twice).
const ROW_STRIDE: u32 = 0x40;
/// Where in a stream's state block the host marks that it holds that
/// stream's history; the game clears the block to start a stream.
const MARK_AT: u32 = 0x43C;
const MARK: u32 = 0x4E36_3452;

/// The synthesis window, from the microcode data at `data`.
pub(crate) fn window(ram: &Memory, data: u32) -> Box<[f32; 512]> {
    let row = |j: u32, k: u32| ram.read_u16(data + WINDOW_ROWS + j * ROW_STRIDE + 2 * k) as i16;
    assert!(
        row(0, 0) == 0 && row(0, 8) == i16::MAX,
        "no synthesis window in the microcode data at {data:#010x}"
    );
    window_from(|j, k| row(j as u32, k as u32))
}

/// The window from rows 0 to 16 (`row(j, k)` is `D[j + 32k]` as stored).
/// The window is a symmetric prototype `h`, `h[512 - i] = h[i]`, with every
/// other block of 64 values negated.
fn window_from(row: impl Fn(usize, usize) -> i16) -> Box<[f32; 512]> {
    let sign = |i: usize| if (i / 64) % 2 == 1 { -1.0 } else { 1.0 };
    let mut h = [0.0f32; 512];
    for j in 0..=16 {
        for k in 0..16 {
            let i = j + 32 * k;
            h[i] = f32::from(row(j, k)) * sign(i);
        }
    }
    for j in 17..32 {
        for k in 0..16 {
            let i = j + 32 * k;
            h[i] = h[512 - i];
        }
    }
    let mut d = Box::new([0.0f32; 512]);
    for (i, d) in d.iter_mut().enumerate() {
        *d = h[i] * sign(i) / 32768.0;
    }
    d
}

/// The matrixing cosines, `cos((16 + i)(2k + 1) pi / 64)`.
fn cosines() -> &'static [[f32; 32]; 64] {
    static N: OnceLock<[[f32; 32]; 64]> = OnceLock::new();
    N.get_or_init(|| {
        let mut n = [[0.0; 32]; 64];
        for (i, row) in n.iter_mut().enumerate() {
            for (k, c) in row.iter_mut().enumerate() {
                let a = (16 + i) as f64 * (2 * k + 1) as f64 * std::f64::consts::PI / 64.0;
                *c = a.cos() as f32;
            }
        }
        n
    })
}

/// One stream's filter history: the last 16 sets of 64 matrixed values.
pub(crate) struct Synth {
    v: Box<[f32; 1024]>,
}

impl Synth {
    fn new() -> Self {
        Self {
            v: Box::new([0.0; 1024]),
        }
    }

    /// One time slot: 32 subband samples in, 32 PCM samples out, scaled
    /// by `gain`.
    fn slot(&mut self, d: &[f32; 512], s: &[f32; 32], gain: f32, out: &mut [i16]) {
        self.v.copy_within(0..960, 64);
        for (v, n) in self.v[..64].iter_mut().zip(cosines()) {
            *v = n.iter().zip(s).map(|(n, s)| n * s).sum();
        }
        for (j, o) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for m in 0..8 {
                acc += self.v[128 * m + j] * d[64 * m + j];
                acc += self.v[128 * m + 96 + j] * d[64 * m + 32 + j];
            }
            *o = (acc * gain).round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}

/// The streams' histories, by state block address, and the window.
#[derive(Default)]
pub(crate) struct Streams {
    window: Option<(u32, Box<[f32; 512]>)>,
    synths: std::collections::HashMap<u32, Synth>,
    /// The state block command 8 named; none until it runs.
    state: Option<u32>,
}

impl Streams {
    /// Command 8: the stream whose history the next frames use.
    pub(crate) fn set_state(&mut self, at: u32) {
        self.state = Some(at);
    }

    /// Command 7: the frame buffer at `buf` to PCM, with the window from
    /// the microcode data at `data`.
    pub(crate) fn frame(&mut self, ram: &mut Memory, data: u32, buf: u32) {
        let Some(state) = self.state else {
            eprintln!("audio: stream frame at {buf:#010x} before any stream state, skipped");
            return;
        };
        if !matches!(&self.window, Some((at, _)) if *at == data) {
            self.window = Some((data, window(ram, data)));
        }
        let d = &self.window.as_ref().unwrap().1;
        if ram.read_u32(state + MARK_AT) != MARK {
            self.synths.insert(state, Synth::new());
            ram.write_u32(state + MARK_AT, MARK);
        }
        let synth = self.synths.entry(state).or_insert_with(Synth::new);
        let whole = f32::from(ram.read_u16(buf) as i16);
        let gain = whole + f32::from(ram.read_u16(buf + 2)) / 65536.0;
        let mut pcm = [0i16; 576];
        for (t, out) in pcm.chunks_mut(32).enumerate() {
            let mut s = [0.0f32; 32];
            for (k, s) in s.iter_mut().enumerate() {
                *s = f32::from(ram.read_u16(buf + 8 + 2 * (32 * t + k) as u32) as i16);
            }
            synth.slot(d, &s, gain, out);
        }
        for (i, v) in pcm.iter().enumerate() {
            ram.write_u16(buf + 2 * i as u32, *v as u16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window with every value given: rows 0 to 16 put where the
    /// microcode data keeps them, each twice.
    fn ram_with_rows(data: u32, row: impl Fn(usize, usize) -> i16) -> Memory {
        let mut ram = Memory::new(1 << 20);
        for j in 0..=16u32 {
            for k in 0..16u32 {
                let v = row(j as usize, k as usize) as u16;
                let at = data + WINDOW_ROWS + j * ROW_STRIDE + 2 * k;
                ram.write_u16(at, v);
                ram.write_u16(at + 32, v);
            }
        }
        ram
    }

    #[test]
    fn the_window_comes_back_from_its_rows_by_symmetry() {
        // A made-up prototype, symmetric about 256 and largest there.
        let h = |i: usize| (32767.0 * (1.0 - (i as f32 - 256.0).abs() / 256.0)) as i16;
        let sign = |i: usize| if (i / 64) % 2 == 1 { -1 } else { 1 };
        let stored = |i: usize| h(i) * sign(i);
        let ram = ram_with_rows(0x8000_1000, |j, k| stored(j + 32 * k));
        let d = window(&ram, 0x8000_1000);
        for (i, d) in d.iter().enumerate() {
            let want = f32::from(stored(i)) / 32768.0;
            assert!((d - want).abs() < 1e-6, "D[{i}] = {d}, want {want}");
        }
    }

    #[test]
    #[should_panic(expected = "no synthesis window")]
    fn data_without_the_window_is_refused() {
        window(&Memory::new(1 << 20), 0x8000_1000);
    }

    #[test]
    fn one_subband_sample_comes_out_through_one_window_value() {
        // Only D[5] (and its mirror D[507], and the peak the check needs)
        // set: the first slot's output is the matrixed sample times D[5].
        let ram = ram_with_rows(0x8000_1000, |j, k| match (j, k) {
            (0, 8) => i16::MAX,
            (5, 0) => 16384,
            _ => 0,
        });
        let d = window(&ram, 0x8000_1000);
        let mut synth = Synth::new();
        let mut s = [0.0; 32];
        s[0] = 1000.0;
        let mut out = [0i16; 32];
        synth.slot(&d, &s, 2.0, &mut out);
        let want = (cosines()[5][0] * 1000.0 * d[5] * 2.0).round() as i16;
        let mut expect = [0i16; 32];
        expect[5] = want;
        assert_eq!(out, expect);
        assert!(want != 0);
    }

    #[test]
    fn a_frame_before_any_stream_state_leaves_memory_alone() {
        const DATA: u32 = 0x8000_1000;
        const BUF: u32 = 0x8005_0000;
        // Where an unset state of 0 would put its mark.
        const LOW: u32 = 0;
        let mut ram = ram_with_rows(DATA, |_, _| 8000);
        ram.write_u32(BUF, 1 << 16);
        let before = (ram.read_u32(LOW + MARK_AT), ram.read_u32(BUF));
        Streams::default().frame(&mut ram, DATA, BUF);
        assert_eq!((ram.read_u32(LOW + MARK_AT), ram.read_u32(BUF)), before);
    }

    #[test]
    fn a_stream_keeps_its_history_until_the_game_clears_its_block() {
        const DATA: u32 = 0x8000_1000;
        const STATE: u32 = 0x8004_0000;
        const BUF: u32 = 0x8005_0000;
        let mut ram = ram_with_rows(DATA, |j, k| match (j, k) {
            (0, 0) => 0,
            (0, 8) => i16::MAX,
            _ => 8000,
        });
        let mut streams = Streams::default();
        streams.set_state(STATE);
        let frame = |ram: &mut Memory, streams: &mut Streams, fill: i16| {
            for i in 0..580u32 {
                ram.write_u16(BUF + 2 * i, fill as u16);
            }
            ram.write_u32(BUF, 1 << 16); // gain 1.0
            streams.frame(ram, DATA, BUF);
            (0..576u32)
                .map(|i| ram.read_u16(BUF + 2 * i) as i16)
                .collect::<Vec<_>>()
        };
        frame(&mut ram, &mut streams, 100);
        let after_sound = frame(&mut ram, &mut streams, 0);
        assert!(after_sound.iter().any(|&v| v != 0), "history carries over");
        // The game starts the stream again: its block is cleared.
        for i in 0..0x440 / 4 {
            ram.write_u32(STATE + 4 * i, 0);
        }
        let fresh = frame(&mut ram, &mut streams, 0);
        assert!(
            fresh.iter().all(|&v| v == 0),
            "a cleared block starts silent"
        );
    }
}
