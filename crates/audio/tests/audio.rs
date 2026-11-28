//! Synthetic command lists only; nothing here plays sound.

use n64rusty_audio::{AudioOut, AudioRsp, Wav};
use n64rusty_runtime::Memory;

const LIST: u32 = 0x8010_0000;
const DATA: u32 = 0x8020_0000;
const STATE: u32 = 0x8030_0000;
const OUT: u32 = 0x8040_0000;

fn c(op: u32, flags: u32, lo0: u32, w1: u32) -> (u32, u32) {
    (op << 24 | flags << 16 | lo0, w1)
}
fn setbuff(i: u32, o: u32, n: u32) -> (u32, u32) {
    c(8, 0, i, o << 16 | n)
}
fn loadbuff(a: u32) -> (u32, u32) {
    c(4, 0, 0, a)
}
fn savebuff(a: u32) -> (u32, u32) {
    c(6, 0, 0, a)
}

fn run(rsp: &mut AudioRsp, ram: &mut Memory, cmds: &[(u32, u32)]) {
    for (i, &(w0, w1)) in cmds.iter().enumerate() {
        ram.write_u32(LIST + 8 * i as u32, w0);
        ram.write_u32(LIST + 8 * i as u32 + 4, w1);
    }
    rsp.run(ram, LIST, 8 * cmds.len() as u32);
}

fn put(ram: &mut Memory, at: u32, s: &[i16]) {
    for (i, &v) in s.iter().enumerate() {
        ram.write_u16(at + 2 * i as u32, v as u16);
    }
}

fn get(ram: &Memory, at: u32, n: usize) -> Vec<i16> {
    (0..n)
        .map(|i| ram.read_u16(at + 2 * i as u32) as i16)
        .collect()
}

fn ram() -> Memory {
    Memory::new(8 << 20)
}

#[test]
fn adpcm_integrator_predictor_continues_across_tasks() {
    let mut ram = ram();
    // Predictor 0: second row all 1.0 (Q11), so each sample is the
    // previous output plus the sum of residuals so far: a running total.
    let mut book = [0i16; 16];
    book[8..].fill(2048);
    put(&mut ram, DATA, &book);
    // Two frames: header (shift 1, predictor 0), then 16 residuals of +1.
    let frame = [0x10u8, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11];
    for (i, b) in frame.iter().chain(&frame).enumerate() {
        ram.write_u8(DATA + 0x100 + i as u32, *b);
    }
    let mut rsp = AudioRsp::new();
    let decode = |flags| {
        vec![
            c(11, 0, 32, DATA),
            setbuff(0x000, 0, 18),
            loadbuff(DATA + 0x100),
            setbuff(0x000, 0x100, 32),
            c(1, flags, 0, STATE),
            setbuff(0, 0x100, 64),
            savebuff(OUT),
        ]
    };
    // The output starts with the 16 samples decoded before (none yet).
    run(&mut rsp, &mut ram, &decode(1));
    let want: Vec<i16> = (1..=16).map(|i| 2 * i).collect();
    assert_eq!(get(&ram, OUT, 16), [0; 16]);
    assert_eq!(get(&ram, OUT + 32, 16), want);
    assert_eq!(
        get(&ram, STATE, 16),
        want,
        "state holds the last 16 samples"
    );

    // Continue from the saved state: the second frame carries on from 32.
    let mut cont = decode(0);
    cont[2] = loadbuff(DATA + 0x109);
    run(&mut rsp, &mut ram, &cont);
    let want: Vec<i16> = (1..=32).map(|i| 2 * i).collect();
    assert_eq!(get(&ram, OUT, 32), want);
}

#[test]
fn adpcm_clamps_and_sign_extends() {
    let mut ram = ram();
    put(&mut ram, DATA, &[0i16; 16]);
    // Shift 12: residual 7 -> 28672, -8 -> -32768, -1 -> -4096.
    let frame = [0xC0u8, 0x78, 0xF0, 0, 0, 0, 0, 0, 0];
    for (i, b) in frame.iter().enumerate() {
        ram.write_u8(DATA + 0x100 + i as u32, *b);
    }
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            c(11, 0, 32, DATA),
            setbuff(0, 0, 9),
            loadbuff(DATA + 0x100),
            setbuff(0, 0x100, 32),
            c(1, 1, 0, STATE),
            setbuff(0, 0x120, 8),
            savebuff(OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 4), [28672, -32768, -4096, 0]);
}

#[test]
fn resample_unity_pitch_is_a_delay_and_splits_cleanly() {
    let input: Vec<i16> = (0..64).map(|i| (i * 300 - 9000) as i16).collect();
    let mut ram = ram();
    put(&mut ram, DATA, &input);

    // One task of 64 samples.
    let mut rsp = AudioRsp::new();
    let task = |src: u32, flags: u32| {
        vec![
            setbuff(0x200, 0, 64),
            loadbuff(src),
            setbuff(0x200, 0x400, 64),
            c(5, flags, 0x8000, STATE),
            setbuff(0, 0x400, 64),
            savebuff(OUT),
        ]
    };
    let mut whole = task(DATA, 1);
    whole[0] = setbuff(0x200, 0, 128);
    whole[2] = setbuff(0x200, 0x400, 128);
    whole[4] = setbuff(0, 0x400, 128);
    run(&mut rsp, &mut ram, &whole);
    let one = get(&ram, OUT, 64);
    assert_eq!(&one[..3], [0, 0, 0]);
    assert_eq!(&one[3..], &input[..61]);

    // Same stream in two tasks of 32 gives the same output.
    run(&mut rsp, &mut ram, &task(DATA, 1));
    let a = get(&ram, OUT, 32);
    run(&mut rsp, &mut ram, &task(DATA + 64, 0));
    let b = get(&ram, OUT, 32);
    assert_eq!([a, b].concat(), one);
}

#[test]
fn resample_half_pitch_interpolates_between_samples() {
    let mut ram = ram();
    put(&mut ram, DATA, &[1000; 16]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            setbuff(0x200, 0, 32),
            loadbuff(DATA),
            setbuff(0x200, 0x400, 32),
            c(5, 1, 0x4000, STATE),
            setbuff(0, 0x400, 32),
            savebuff(OUT),
        ],
    );
    // 16 outputs consume 8 inputs; a constant input stays constant once
    // the zero history has passed.
    let out = get(&ram, OUT, 16);
    assert!(out[8..].iter().all(|&v| v == 1000), "{out:?}");
    assert!(out[..8].windows(2).all(|w| w[0] <= w[1] + 100), "{out:?}");
}

#[test]
fn envmixer_ramps_volume_and_feeds_aux() {
    let mut ram = ram();
    put(&mut ram, DATA, &[16384; 32]);
    let mut rsp = AudioRsp::new();
    let (inp, dl, dr, wl, wr) = (0x000, 0x100, 0x200, 0x300, 0x400);
    run(
        &mut rsp,
        &mut ram,
        &[
            setbuff(inp, 0, 64),
            loadbuff(DATA),
            c(2, 0, dl, 0x400),
            setbuff(inp, dl, 64),
            c(8, 8, dr, wl << 16 | wr),
            c(9, 4 | 2, 0x7FFF, 0),    // left volume
            c(9, 4, 0x4000, 0),        // right volume
            c(9, 2, 0x1000, 0x8000),   // left: halve every 8 down to 0x1000
            c(9, 0, 0x4000, 0x1_0000), // right: hold
            c(9, 8, 0x7FFF, 0x4000),   // dry, wet
            c(3, 1 | 8, 0, STATE),
            setbuff(0, dl, 0x500),
            savebuff(OUT),
        ],
    );
    let out = get(&ram, OUT, 0x280);
    let at = |base: u32, i: usize| out[(base - dl) as usize / 2 + i] as i64;
    let mix = |x: i64, vol: i64, gain: i64| (((x * vol) >> 15) * gain) >> 15;
    // Left volume halves every 8 samples and stops at its target.
    assert_eq!(at(dl, 0), mix(16384, 0x7FFF, 0x7FFF));
    assert_eq!(at(dl, 8), mix(16384, 0x3FFF, 0x7FFF));
    assert_eq!(at(dl, 16), mix(16384, 0x1FFF, 0x7FFF));
    assert_eq!(at(dl, 24), mix(16384, 0x1000, 0x7FFF));
    assert_eq!(at(dl, 31), mix(16384, 0x1000, 0x7FFF));
    // Right holds at half volume; wet is half of dry.
    assert!((0..32).all(|i| at(dr, i) == 8191));
    assert_eq!(at(wr, 0), 4096);
    assert_eq!(at(wl, 0), 8191);
    // The state carries the ramp into the next task.
    run(
        &mut rsp,
        &mut ram,
        &[
            setbuff(inp, dl, 16),
            c(2, 0, dl, 0x40),
            c(3, 0, 0, STATE),
            setbuff(0, dl, 2),
            savebuff(OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 1), [2047]);
}

#[test]
fn mix_interleave_save_and_segments() {
    let mut ram = ram();
    put(&mut ram, DATA, &[30000, -30000, 100, -100]);
    put(&mut ram, DATA + 8, &[10000, -10000, 7, -7]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            c(7, 0, 0, 3 << 24 | (DATA & 0xFF_FFFF)), // segment 3 = DATA
            setbuff(0x100, 0, 8),
            loadbuff(3 << 24),
            setbuff(0x200, 0, 8),
            loadbuff(3 << 24 | 8),
            c(10, 0, 0x200, 0x300 << 16 | 8), // copy to 0x300
            setbuff(0, 0, 8),
            c(12, 0, 0x7FFF, 0x300 << 16 | 0x100), // mix 0x300 into 0x100
            setbuff(0, 0x400, 8),
            c(13, 0, 0, 0x100 << 16 | 0x200),
            setbuff(0, 0x400, 16),
            savebuff(OUT),
        ],
    );
    assert_eq!(
        get(&ram, OUT, 8),
        [32767, 10000, -32768, -10000, 106, 7, -107, -7],
        "saturating mix, then left/right interleave"
    );
}

#[test]
fn unknown_command_is_skipped() {
    let (mut rsp, mut ram) = (AudioRsp::new(), ram());
    run(&mut rsp, &mut ram, &[c(16, 0, 0, 0), c(16, 0, 0, 0)]);
}

#[test]
fn wav_header_and_data() {
    let mut w = Wav::new(std::io::Cursor::new(Vec::new()), 32000).unwrap();
    w.write(&[1, -2, 3, -4]).unwrap();
    let b = w.finish().unwrap().into_inner();
    assert_eq!(b.len(), 44 + 8);
    assert_eq!(&b[..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 8);
    assert_eq!(&b[8..16], b"WAVEfmt ");
    assert_eq!(u16::from_le_bytes([b[22], b[23]]), 2, "stereo");
    assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 32000);
    assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 8);
    assert_eq!(&b[44..], [1, 0, 0xFE, 0xFF, 3, 0, 0xFC, 0xFF]);
}

#[test]
fn audio_out_records_wav_and_mute_leaves_recording() {
    let mut ram = ram();
    put(&mut ram, OUT, &[0x1234, -1, 5, 6]);
    let path = std::env::temp_dir().join(format!("n64rusty-audio-test-{}.wav", std::process::id()));
    let mut ai = AudioOut::new();
    ai.record_wav(&path);
    ai.set_frequency(22050);
    ai.set_muted(true);
    assert!(ai.muted());
    ai.play(&ram, OUT, 8).unwrap();
    ai.play(&ram, OUT, 4).unwrap();
    ai.finish().unwrap();
    let b = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 22050);
    assert_eq!(
        &b[44..],
        [0x34, 0x12, 0xFF, 0xFF, 5, 0, 6, 0, 0x34, 0x12, 0xFF, 0xFF]
    );
}

#[test]
fn audio_out_needs_a_frequency_and_plays_whole_frames() {
    let mut ram = ram();
    put(&mut ram, OUT, &[1, 2, 3, 4]);
    let path =
        std::env::temp_dir().join(format!("n64rusty-audio-frames-{}.wav", std::process::id()));
    let mut ai = AudioOut::new();
    ai.record_wav(&path);
    assert!(ai.play(&ram, OUT, 8).is_err(), "no frequency set yet");
    ai.set_frequency(32000);
    ai.play(&ram, OUT, 6).unwrap();
    ai.finish().unwrap();
    let b = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(&b[44..], [1, 0, 2, 0], "the half frame is dropped");
}

#[test]
fn third_set_loads_mixes_interleaves_and_saves() {
    // LOADBUFF, ENVSETUP1/2, ENVMIXER, INTERLEAVE and SAVEBUFF of the
    // command set with opcodes past 0x0F.
    let mut ram = Memory::new(0x10000);
    let (list, src, dst) = (0x8000_0000u32, 0x8000_1000u32, 0x8000_2000u32);
    for i in 0..16 {
        ram.write_u16(src + 2 * i, 0x4000);
    }
    let cmds: [(u32, u32); 7] = [
        (0x1402_0000, src & 0xFF_FFFF), // 32 bytes to DMEM 0
        (0x1200_0000, 0),               // wet 0, no steps
        (0x1600_0000, 0xFFFF_8000),     // left ~1.0, right 0.5
        (0x1300_1000, 0x1018_2028),     // 16 samples from 0 to dry L 0x100, R 0x180
        (0x0D02_0400, 0x0100_0180),     // interleave 32 bytes a side to 0x400
        (0x1504_0400, dst & 0xFF_FFFF), // save 64 bytes
        (0x0000_0000, 0),
    ];
    for (k, (w0, w1)) in cmds.iter().enumerate() {
        ram.write_u32(list + 8 * k as u32, *w0);
        ram.write_u32(list + 8 * k as u32 + 4, *w1);
    }
    let mut rsp = AudioRsp::new();
    rsp.run(&mut ram, list, 8 * cmds.len() as u32);
    assert_eq!(ram.read_u16(dst) as i16, 0x3FFF); // left: 0x4000 * 0xFFFF >> 16
    assert_eq!(ram.read_u16(dst + 2) as i16, 0x2000); // right: half
}

// Third command set (opcodes past 0x0F, physical addresses). Expected values
// follow the algorithm of that microcode as the mupen64plus RSP HLE models it
// (alist_nead.c and alist.c), cited per test where it matters.

const A_INIT: u32 = 0x01;
const A_LOOP: u32 = 0x02;
const A_SMALL: u32 = 0x04;
/// One ADPCM frame decodes to 16 samples: 32 bytes of output.
const FRAME_BYTES: u32 = 32;
/// Loop start state a game copies from its sample bank.
const LOOP_STATE: u32 = DATA + 0x40;
const FRAME_DATA: u32 = DATA + 0x100;

/// LOADBUFF and SAVEBUFF of the third set carry their own DMEM address and
/// byte count. Every third-set list below uses them, and their opcodes
/// (0x14, 0x15) are what tells the interpreter the list is of that set.
fn t_load(dmem: u32, bytes: u32, from: u32) -> (u32, u32) {
    (0x14 << 24 | bytes << 12 | dmem, from & 0xFF_FFFF)
}
fn t_save(dmem: u32, bytes: u32, to: u32) -> (u32, u32) {
    (0x15 << 24 | bytes << 12 | dmem, to & 0xFF_FFFF)
}
fn t_clear(dmem: u32, bytes: u32) -> (u32, u32) {
    c(2, 0, dmem, bytes)
}

fn put_bytes(ram: &mut Memory, at: u32, b: &[u8]) {
    for (i, &v) in b.iter().enumerate() {
        ram.write_u8(at + i as u32, v);
    }
}

fn get_bytes(ram: &Memory, at: u32, n: usize) -> Vec<u8> {
    (0..n).map(|i| ram.read_u8(at + i as u32)).collect()
}

/// Codebook whose predictor 0 has a second row of 1.0 (Q11): each output
/// is the last output plus the sum of residuals so far.
fn integrator_book(ram: &mut Memory) -> (u32, u32) {
    let mut book = [0i16; 16];
    book[8..].fill(2048);
    put(ram, DATA, &book);
    c(11, 0, 32, DATA)
}

/// A 4-bit frame with scale 0, predictor 0 and every residual +1.
const ONES_FRAME: [u8; 9] = [0x00, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11];

#[test]
fn third_set_adpcm_loop_takes_history_from_the_loop_state_and_saves_to_its_own() {
    let mut ram = ram();
    let book = integrator_book(&mut ram);
    let loop_history: Vec<i16> = (1..=16).map(|i| i * 100).collect();
    put(&mut ram, LOOP_STATE, &loop_history);
    put(&mut ram, STATE, &[7; 16]);
    put_bytes(&mut ram, FRAME_DATA, &ONES_FRAME);
    let decode = |flags: u32| {
        vec![
            book,
            c(15, 0, 0, LOOP_STATE),
            t_load(0, 16, FRAME_DATA),
            setbuff(0, 0x100, FRAME_BYTES),
            c(1, flags, 0, STATE),
            t_save(0x100, 2 * FRAME_BYTES, OUT),
        ]
    };
    let mut rsp = AudioRsp::new();
    run(&mut rsp, &mut ram, &decode(A_LOOP));
    // alist_adpcm: with the loop flag the last frame comes from the loop
    // address, and the decoded frame is stored to the state address.
    assert_eq!(
        get(&ram, OUT, 16),
        loop_history,
        "history is the loop start"
    );
    let after_loop: Vec<i16> = (1..=16).map(|i| 1600 + i).collect();
    assert_eq!(get(&ram, OUT + 32, 16), after_loop, "predicts from 1600");
    assert_eq!(get(&ram, STATE, 16), after_loop, "state gets the new frame");
    assert_eq!(get(&ram, LOOP_STATE, 16), loop_history, "loop start kept");

    // Without the loop flag the next frame carries on from the state.
    run(&mut rsp, &mut ram, &decode(0));
    assert_eq!(get(&ram, OUT, 16), after_loop);
    let next: Vec<i16> = (17..=32).map(|i| 1600 + i).collect();
    assert_eq!(get(&ram, OUT + 32, 16), next);
}

#[test]
fn third_set_adpcm_small_flag_decodes_five_byte_two_bit_frames() {
    let mut ram = ram();
    // All-zero predictor: each output is its residual.
    put(&mut ram, DATA, &[0; 16]);
    // adpcm_predict_frame_2bits: four residuals a byte, high bits first,
    // signed (01 = 1, 10 = -2, 11 = -1), each shifted left by the scale.
    let frames = [
        0x20, 0x6C, 0x1B, 0xFF, 0x40, // scale 2
        0x00, 0x55, 0xAA, 0x00, 0xC0, // scale 0
    ];
    put_bytes(&mut ram, FRAME_DATA, &frames);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            c(11, 0, 32, DATA),
            t_load(0, 16, FRAME_DATA),
            setbuff(0, 0x100, 2 * FRAME_BYTES),
            c(1, A_INIT | A_SMALL, 0, STATE),
            t_save(0x100, 3 * FRAME_BYTES, OUT),
        ],
    );
    let first: Vec<i16> = [1i16, -2, -1, 0, 0, 1, -2, -1, -1, -1, -1, -1, 1, 0, 0, 0]
        .iter()
        .map(|r| r << 2)
        .collect();
    let second: [i16; 16] = [1, 1, 1, 1, -2, -2, -2, -2, 0, 0, 0, 0, -1, 0, 0, 0];
    assert_eq!(get(&ram, OUT, 16), [0; 16], "init: no history");
    assert_eq!(get(&ram, OUT + 32, 16), first);
    assert_eq!(get(&ram, OUT + 64, 16), second, "second header at byte 5");
}

#[test]
fn third_set_add_mixer_saturates_over_its_sixteen_byte_units() {
    let mut ram = ram();
    put(&mut ram, DATA, &[30000, -30000, 1, 2, 3, 4, 5, 6, 99]);
    put(
        &mut ram,
        DATA + 0x20,
        &[10000, -10000, 10, 10, 10, 10, 10, 10, 50],
    );
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(0, 18, DATA),
            t_load(0x100, 18, DATA + 0x20),
            // Flags give the count in 16-byte units: 1 = 8 samples.
            c(4, 1, 0, 0x100), // input DMEM 0 (high half), output 0x100
            t_save(0x100, 18, OUT),
        ],
    );
    assert_eq!(
        get(&ram, OUT, 9),
        [32767, -32768, 11, 12, 13, 14, 15, 16, 50],
        "saturating add of 8 samples; the 9th is past the count"
    );
}

#[test]
fn third_set_zero_order_hold_resample_repeats_the_nearest_earlier_sample() {
    let mut ram = ram();
    let input: Vec<i16> = (1..=16).map(|i| i * 10).collect();
    put(&mut ram, DATA, &input);
    let resample = |pitch: u32, accu: u32| {
        vec![
            t_load(0, 32, DATA),
            setbuff(0, 0x200, 16),
            c(6, 0, pitch, accu),
            t_save(0x200, 16, OUT),
        ]
    };
    let mut rsp = AudioRsp::new();
    // alist_resample_zoh: output the sample at the position, then step by
    // pitch * 2 (Q16). Half pitch from a half-sample start:
    run(&mut rsp, &mut ram, &resample(0x4000, 0x8000));
    assert_eq!(get(&ram, OUT, 8), [10, 20, 20, 30, 30, 40, 40, 50]);
    // One and a half samples a step from the start: 0, 1.5, 3, 4.5, ...
    run(&mut rsp, &mut ram, &resample(0xC000, 0));
    assert_eq!(get(&ram, OUT, 8), [10, 20, 40, 50, 70, 80, 100, 110]);
}

/// DUPLICATE copies one 128-byte block.
const BLOCK: u32 = 128;

#[test]
fn third_set_duplicate_repeats_one_128_byte_block_count_times() {
    let mut ram = ram();
    let block: Vec<i16> = (0..64).map(|i| i * 3 + 1).collect();
    put(&mut ram, DATA, &block);
    put(&mut ram, DATA + 0x100, &[-5]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(0, BLOCK, DATA),
            t_load(0x200 + 3 * BLOCK, 2, DATA + 0x100), // sentinel past the copies
            c(9, 3, 0, 0x200 << 16),
            t_save(0x200, 3 * BLOCK + 2, OUT),
        ],
    );
    let out = get(&ram, OUT, 3 * 64 + 1);
    for k in 0..3 {
        assert_eq!(&out[64 * k..64 * (k + 1)], &block[..], "copy {k}");
    }
    assert_eq!(out[192], -5, "nothing written past three blocks");
}

#[test]
#[ignore = "bug: DUPLICATE re-reads its source each copy, so a destination overlapping the source repeats overwritten data"]
fn third_set_duplicate_onto_its_own_source_repeats_the_original_block() {
    let mut ram = ram();
    let block: Vec<i16> = (0..64).map(|i| i + 1).collect();
    put(&mut ram, DATA, &block);
    let mut rsp = AudioRsp::new();
    // alist_repeat64 reads the 64 samples once (the microcode holds them in
    // vector registers), then stores them count times.
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(0x100, BLOCK, DATA),
            c(9, 2, 0x100, 0x140 << 16),
            t_save(0x140, 2 * BLOCK, OUT),
        ],
    );
    let out = get(&ram, OUT, 128);
    assert_eq!(&out[..64], &block[..]);
    assert_eq!(&out[64..], &block[..], "second copy is the original too");
}

#[test]
fn third_set_dmem_move_rounds_its_count_up_to_four_bytes() {
    let mut ram = ram();
    let bytes: Vec<u8> = (1..=16).collect();
    put_bytes(&mut ram, DATA, &bytes);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(0, 16, DATA),
            t_clear(0x100, 16),
            c(10, 0, 0, 0x100 << 16 | 6), // 6 bytes: (6 + 3) & !3 = 8
            t_save(0x100, 16, OUT),
        ],
    );
    assert_eq!(
        get_bytes(&ram, OUT, 16),
        [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn third_set_gain_is_signed_q4_4_in_place_and_saturates() {
    // alist_multQ44: sample * gain >> 4 (arithmetic), clamped to 16 bits,
    // over a byte count.
    const TWO: u32 = 0x20;
    const MINUS_ONE: u32 = 0xF0;
    const HALF: u32 = 0x08;
    let input = [1000, -1000, -3, 30000, -32768, 7, 555];
    let mut ram = ram();
    put(&mut ram, DATA, &input);
    let gain = |rsp: &mut AudioRsp, ram: &mut Memory, g: u32| {
        run(
            rsp,
            ram,
            &[
                t_load(0x100, 14, DATA),
                c(14, g, 12, 0x100 << 16), // 12 bytes: the 7th sample is past it
                t_save(0x100, 14, OUT),
            ],
        );
        get(ram, OUT, input.len())
    };
    let mut rsp = AudioRsp::new();
    assert_eq!(
        gain(&mut rsp, &mut ram, TWO),
        [2000, -2000, -6, 32767, -32768, 14, 555]
    );
    assert_eq!(
        gain(&mut rsp, &mut ram, MINUS_ONE),
        [-1000, 1000, 3, -30000, 32767, -7, 555]
    );
    assert_eq!(
        gain(&mut rsp, &mut ram, HALF),
        [500, -500, -2, 15000, -16384, 3, 555],
        "the shift rounds towards minus infinity"
    );
}

#[test]
fn third_set_interl_copies_every_other_sample() {
    let mut ram = ram();
    put(&mut ram, DATA, &[1, 2, 3, 4, 5, 6, 7, 8]);
    put(&mut ram, DATA + 0x20, &[-9; 5]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(0, 16, DATA),
            t_load(0x100, 10, DATA + 0x20),
            c(0x11, 0, 4, 0x100), // 4 samples from DMEM 0 (high half) to 0x100
            t_save(0x100, 10, OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 5), [1, 3, 5, 7, -9]);
}

/// 16 signed 8-bit samples covering both limits and a sign change.
const S8_BYTES: [u8; 16] = [
    0x01, 0x7F, 0x80, 0xFF, 0x00, 0x40, 0xC0, 0x10, 0x20, 0x30, 0xF0, 0xE0, 0xD0, 0x02, 0x03, 0x04,
];

fn s8_widened() -> Vec<i16> {
    S8_BYTES.iter().map(|&b| i16::from(b as i8) << 8).collect()
}

fn s8_decode(flags: u32) -> Vec<(u32, u32)> {
    vec![
        c(15, 0, 0, LOOP_STATE),
        t_load(0, 16, FRAME_DATA),
        setbuff(0, 0x100, FRAME_BYTES),
        c(0x17, flags, 0, STATE),
        t_save(0x100, 2 * FRAME_BYTES, OUT),
    ]
}

#[test]
fn third_set_s8_decode_widens_bytes_after_the_history_and_saves_state() {
    let mut ram = ram();
    put_bytes(&mut ram, FRAME_DATA, &S8_BYTES);
    put(&mut ram, STATE, &[7; 16]);
    let mut rsp = AudioRsp::new();
    run(&mut rsp, &mut ram, &s8_decode(A_INIT));
    assert_eq!(get(&ram, OUT, 16), [0; 16], "init: zero history");
    assert_eq!(get(&ram, OUT + 32, 16), s8_widened());
    assert_eq!(get(&ram, STATE, 16), s8_widened(), "state is the last 16");

    // Without init or loop the history is the saved state.
    run(&mut rsp, &mut ram, &s8_decode(0));
    assert_eq!(get(&ram, OUT, 16), s8_widened());
}

#[test]
fn third_set_s8_decode_loop_takes_history_from_the_loop_state() {
    let mut ram = ram();
    let loop_history: Vec<i16> = (1..=16).map(|i| -i * 100).collect();
    put(&mut ram, LOOP_STATE, &loop_history);
    put(&mut ram, STATE, &[7; 16]);
    put_bytes(&mut ram, FRAME_DATA, &S8_BYTES);
    let mut rsp = AudioRsp::new();
    run(&mut rsp, &mut ram, &s8_decode(A_LOOP));
    assert_eq!(get(&ram, OUT, 16), loop_history);
    assert_eq!(get(&ram, OUT + 32, 16), s8_widened());
    assert_eq!(get(&ram, STATE, 16), s8_widened());
    assert_eq!(get(&ram, LOOP_STATE, 16), loop_history, "loop start kept");
}

// Third-set envelope mixer buffers.
const ENV_IN: u32 = 0x000;
const ENV_DL: u32 = 0x100;
const ENV_DR: u32 = 0x180;
const ENV_WL: u32 = 0x200;
const ENV_WR: u32 = 0x280;
/// Envelope volumes and steps move every 8 samples.
const ENV_BLOCK: usize = 8;

/// ENVSETUP1: wet volume (high byte), wet step, left and right steps.
fn env_setup1(wet: u32, wet_step: u32, left_step: u32, right_step: u32) -> (u32, u32) {
    c(0x12, wet, wet_step, left_step << 16 | right_step)
}
/// ENVSETUP2: left and right volumes (unsigned Q16).
fn env_setup2(left: u32, right: u32) -> (u32, u32) {
    c(0x16, 0, 0, left << 16 | right)
}
/// ENVMIXER over `n` samples from ENV_IN into the four buffers.
fn env_mix(n: u32, bits: u32) -> (u32, u32) {
    (
        0x13 << 24 | (ENV_IN >> 4) << 16 | n << 8 | bits,
        (ENV_DL >> 4) << 24 | (ENV_DR >> 4) << 16 | (ENV_WL >> 4) << 8 | ENV_WR >> 4,
    )
}
fn env_clear() -> (u32, u32) {
    t_clear(ENV_DL, ENV_WR + 0x80 - ENV_DL)
}
/// alist_envmix_nead: sample * volume >> 16 for dry, and that * wet >> 16.
fn env_scale(x: i32, vol: u32) -> i32 {
    (x * vol as i32) >> 16
}

#[test]
fn third_set_envmixer_volumes_step_every_eight_samples_and_carry_over() {
    const X: i32 = 0x4000;
    const STEP: u32 = 0x1000;
    const RIGHT: u32 = 0x8000;
    let mut ram = ram();
    put(&mut ram, DATA, &[X as i16; 24]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(ENV_IN, 48, DATA),
            env_clear(),
            env_setup1(0x10, STEP, STEP, 0), // wet starts at 0x1000
            env_setup2(STEP, RIGHT),
            env_mix(24, 0),
            t_save(ENV_DL, 0x200, OUT),
            // The next command in the same list starts from where the ramp
            // ended: 0x4000. ENVSETUP is not resent, and the input at
            // ENV_IN is still loaded.
            env_clear(),
            env_mix(8, 0),
            t_save(ENV_DL, 2, OUT + 0x200),
        ],
    );
    let out = get(&ram, OUT, 0x100);
    let at = |buf: u32, i: usize| i32::from(out[(buf - ENV_DL) as usize / 2 + i]);
    for i in 0..24 {
        let block = (i / ENV_BLOCK) as u32 + 1;
        let left = env_scale(X, STEP * block);
        assert_eq!(at(ENV_DL, i), left, "dry left at {i}");
        assert_eq!(at(ENV_DR, i), env_scale(X, RIGHT), "dry right at {i}");
        assert_eq!(
            at(ENV_WL, i),
            env_scale(left, STEP * block),
            "wet left at {i}"
        );
    }
    assert_eq!(
        get(&ram, OUT + 0x200, 1),
        [env_scale(X, 4 * STEP) as i16],
        "second ENVMIXER carries on at the ramp's end"
    );
}

#[test]
fn third_set_envmixer_volume_is_a_sixteen_bit_lane() {
    // The volumes are 16-bit lanes: 0xF000 + 0x2000 gives 0x1000. Unsigned
    // wrap and signed saturating VADD (-4096 + 8192) agree here, so this
    // checks the lane width, not which of the two the microcode uses.
    const X: i32 = 0x4000;
    let mut ram = ram();
    put(&mut ram, DATA, &[X as i16; 16]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(ENV_IN, 32, DATA),
            env_clear(),
            env_setup1(0, 0, 0x2000, 0),
            env_setup2(0xF000, 0),
            env_mix(16, 0),
            t_save(ENV_DL, 32, OUT),
        ],
    );
    let dl = get(&ram, OUT, 16);
    assert!(dl[..ENV_BLOCK]
        .iter()
        .all(|&v| i32::from(v) == env_scale(X, 0xF000)));
    assert!(dl[ENV_BLOCK..]
        .iter()
        .all(|&v| i32::from(v) == env_scale(X, 0x1000)));
}

#[test]
fn third_set_envmixer_output_saturates_at_both_sixteen_bit_limits() {
    const FULL: u32 = 0xFFFF;
    const NEAR_MAX: i16 = 32000;
    let mut ram = ram();
    // Sample 0 is loud and positive, sample 1 the most negative.
    put(&mut ram, DATA, &[0x4000, i16::MIN, 0, 0, 0, 0, 0, 0]);
    put(
        &mut ram,
        DATA + 0x20,
        &[NEAR_MAX, -NEAR_MAX, 0, 0, 0, 0, 0, 0],
    );
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            t_load(ENV_IN, 16, DATA),
            env_clear(),
            t_load(ENV_DR, 16, DATA + 0x20),
            t_load(ENV_DL, 16, DATA + 0x20),
            env_setup1(0, 0, 0, 0),
            env_setup2(FULL, FULL),
            env_mix(8, 0x2), // invert the left dry output
            t_save(ENV_DL, 16, OUT),
            t_save(ENV_DR, 16, OUT + 0x20),
        ],
    );
    // Right: 32000 + 0x3FFF and -32000 + -0x8000 both saturate.
    assert_eq!(get(&ram, OUT + 0x20, 2), [i16::MAX, i16::MIN]);
    // Left inverted (v ^ -1): 0x3FFF -> -0x4000, -0x8000 -> 0x7FFF.
    assert_eq!(get(&ram, OUT, 2), [NEAR_MAX - 0x4000, i16::MAX - NEAR_MAX]);
}

// Standard set envelope mixer: ramp end and saturation.

/// The left dry buffer after a standard ENVMIXER over `n` samples of `x`
/// at left volume `vol` ramping by `rate` to `target`, dry gain full.
fn standard_envmix(x: i16, n: u32, vol: i16, target: i16, rate: u32) -> Vec<i16> {
    let mut ram = ram();
    put(&mut ram, DATA, &vec![x; n as usize]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            setbuff(0, 0, 2 * n),
            loadbuff(DATA),
            c(2, 0, 0x400, 4 * n),
            setbuff(0, 0x400, 2 * n),
            c(8, 8, 0x400 + 2 * n, 0x600 << 16 | 0x680), // wet: scratch
            c(9, 4 | 2, vol as u16 as u32, 0),
            c(9, 2, target as u16 as u32, rate),
            c(9, 8, 0x7FFF, 0),
            c(3, 1, 0, STATE),
            setbuff(0, 0x400, 2 * n),
            savebuff(OUT),
        ],
    );
    get(&ram, OUT, n as usize)
}

#[test]
fn envmixer_ramp_up_stops_at_its_target_and_holds_there() {
    // Doubling every 8 samples: 0x1000, 0x2000, 0x4000, then the target.
    const TARGET: i16 = 0x7000;
    const DOUBLE: u32 = 0x2_0000;
    const HOLD: u32 = 0x1_0000;
    const X: i16 = 0x4000;
    let ramp = standard_envmix(X, 48, 0x1000, TARGET, DOUBLE);
    let at_target = standard_envmix(X, 48, TARGET, TARGET, HOLD);
    let blocks: Vec<i16> = ramp.iter().step_by(8).copied().collect();
    assert!(blocks[..4].windows(2).all(|w| w[0] < w[1]), "{blocks:?}");
    assert_eq!(&ramp[24..], &at_target[24..], "never past the target");
}

#[test]
fn envmixer_adds_with_saturation_at_both_sixteen_bit_limits() {
    let mut ram = ram();
    put(&mut ram, DATA, &[0x4000, -0x4000]);
    put(&mut ram, DATA + 0x10, &[32700, -32700]);
    let mut rsp = AudioRsp::new();
    run(
        &mut rsp,
        &mut ram,
        &[
            setbuff(0, 0, 4),
            loadbuff(DATA),
            setbuff(0x100, 0, 4),
            loadbuff(DATA + 0x10),
            setbuff(0x200, 0, 4),
            loadbuff(DATA + 0x10),
            setbuff(0, 0x100, 4),
            c(8, 8, 0x200, 0x600 << 16 | 0x680), // wet: scratch
            c(9, 4 | 2, 0x7FFF, 0),
            c(9, 4, 0x7FFF, 0),
            c(9, 2, 0x7FFF, 0x1_0000),
            c(9, 0, 0x7FFF, 0x1_0000),
            c(9, 8, 0x7FFF, 0),
            c(3, 1, 0, STATE),
            setbuff(0, 0x100, 4),
            savebuff(OUT),
            setbuff(0, 0x200, 4),
            savebuff(OUT + 4),
        ],
    );
    assert_eq!(get(&ram, OUT, 4), [i16::MAX, i16::MIN, i16::MAX, i16::MIN]);
}
