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
#[should_panic(expected = "audio command 0x10 not implemented")]
fn unknown_command_names_itself() {
    let mut ram = ram();
    run(&mut AudioRsp::new(), &mut ram, &[c(16, 0, 0, 0)]);
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

#[test]
fn third_set_filter_is_an_fir_with_carried_state() {
    // FILTER setup (16 bytes, table at 0x1000), then FILTER on DMEM 0 with
    // its state at 0x2000. Both tables hold c0 = 0.5, c1 = 0.25; the state's
    // last sample is 0x100, so out[0] = 0.5 * in[0] + 0.25 * 0x100.
    let mut ram = Memory::new(0x10000);
    let (list, table, state) = (0x8000_0000u32, 0x8000_1000u32, 0x8000_2000u32);
    for t in [table, state + 0x10] {
        ram.write_u16(t, 0x4000);
        ram.write_u16(t + 2, 0x2000);
    }
    ram.write_u16(state + 14, 0x100);
    for i in 0..8 {
        ram.write_u16(0x8000_3000 + 2 * i, 0x1000);
    }
    let cmds: [(u32, u32); 4] = [
        (0x1410_0000, 0x3000), // 16 bytes to DMEM 0
        (0x0702_0010, table & 0xFF_FFFF),
        (0x0700_0000, state & 0xFF_FFFF),
        (0x1510_0000, 0x4000), // save them back
    ];
    for (k, (w0, w1)) in cmds.iter().enumerate() {
        ram.write_u32(list + 8 * k as u32, *w0);
        ram.write_u32(list + 8 * k as u32 + 4, *w1);
    }
    AudioRsp::new().run(&mut ram, list, 8 * cmds.len() as u32);
    let out = 0x8000_4000;
    assert_eq!(ram.read_u16(out) as i16, 0x800 + 0x40);
    assert_eq!(ram.read_u16(out + 2) as i16, 0x800 + 0x400);
    // The state now holds the last input samples.
    assert_eq!(ram.read_u16(state + 14), 0x1000);
}
