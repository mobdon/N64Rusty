//! The `n_audio` command set, with synthetic command lists only.

use n64rusty_audio::AudioRsp;
use n64rusty_runtime::Memory;

const LIST: u32 = 0x8010_0000;
const DATA: u32 = 0x8020_0000;
const STATE: u32 = 0x0030_0000;
const OUT: u32 = 0x0040_0000;

// Buffer offsets from the main buffer at DMEM 0x4F0.
const MAIN2: u32 = 0x170;
const DRY_LEFT: u32 = 0x9D0 - 0x4F0;
const DRY_RIGHT: u32 = 0xB40 - 0x4F0;
const WET_LEFT: u32 = 0xCB0 - 0x4F0;
const SUBFRAME: usize = 184;

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

fn load(off: u32, count: u32, from: u32) -> (u32, u32) {
    (4 << 24 | count << 12 | off, from & 0xFF_FFFF)
}
fn save(off: u32, count: u32, to: u32) -> (u32, u32) {
    (6 << 24 | count << 12 | off, to)
}
fn clear(off: u32, count: u32) -> (u32, u32) {
    (2 << 24 | off, count)
}
fn book(at: u32) -> (u32, u32) {
    (11 << 24 | 32, at & 0xFF_FFFF)
}

fn setup() -> (AudioRsp, Memory) {
    (AudioRsp::n_audio(), Memory::new(8 << 20))
}

#[test]
fn buffers_move_mix_and_interleave() {
    let (mut rsp, mut ram) = setup();
    put(&mut ram, DATA, &[100, -200, 30000, -30000, 5, 6, 7, 8]);
    put(&mut ram, DATA + 0x20, &[1, 2, 3, 4]);
    put(&mut ram, DATA + 0x40, &[-1, -2, -3, -4]);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(0, 0xB00),
            load(0, 16, DATA),
            (10 << 24, 0x200 << 16 | 14), // move 14 bytes, rounded up to 16
            (12 << 24 | 0x7FFF, 0x200 << 16), // mix offset 0x200 into 0
            save(0, 16, OUT),
            load(DRY_LEFT, 8, DATA + 0x20),
            load(DRY_RIGHT, 8, DATA + 0x40),
            (13 << 24, 0),
            save(0, 16, OUT + 0x100),
        ],
    );
    assert_eq!(
        get(&ram, OUT, 8),
        [199, -400, 32767, -32768, 9, 11, 13, 15],
        "doubled, saturating, all 8 samples moved"
    );
    assert_eq!(get(&ram, OUT + 0x100, 8), [1, -1, 2, -2, 3, -3, 4, -4]);
}

#[test]
fn adpcm_packs_offsets_and_count() {
    let (mut rsp, mut ram) = setup();
    let mut b = [0i16; 16];
    b[8..].fill(2048); // running total, as in the standard set's test
    put(&mut ram, DATA, &b);
    let frame = [0x10u8, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11];
    for (i, v) in frame.iter().enumerate() {
        ram.write_u8(DATA + 0x100 + i as u32, *v);
    }
    let decode = |flags: u32| {
        [
            book(DATA),
            load(4, 16, DATA + 0x100), // 9 bytes, DMA rounds to 16
            (1 << 24 | STATE, flags << 28 | 32 << 16 | 4 << 12 | 0x100),
            save(0x100, 64, OUT),
        ]
    };
    // The output starts with the 16 samples decoded before (none yet).
    run(&mut rsp, &mut ram, &decode(1));
    let want: Vec<i16> = (1..=16).map(|i| 2 * i).collect();
    assert_eq!(get(&ram, OUT, 16), [0; 16]);
    assert_eq!(get(&ram, OUT + 32, 16), want);
    run(&mut rsp, &mut ram, &decode(0));
    let want: Vec<i16> = (1..=32).map(|i| 2 * i).collect();
    assert_eq!(get(&ram, OUT, 32), want, "continues from the state");
}

#[test]
fn resample_fixed_subframe_to_second_buffer() {
    let (mut rsp, mut ram) = setup();
    let input: Vec<i16> = (0..32).map(|i| i * 100 - 1000).collect();
    put(&mut ram, DATA, &input);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(0, 0x400),
            load(8, 64, DATA),
            (5 << 24 | STATE, 1 << 30 | 0x8000 << 14 | 8 << 2 | 1),
            save(MAIN2, 0x170, OUT),
        ],
    );
    let out = get(&ram, OUT, SUBFRAME);
    assert_eq!(&out[..3], [0, 0, 0]);
    assert_eq!(&out[3..35], &input[..]);
    assert!(out[35..].iter().all(|&v| v == 0));
}

#[test]
fn envelope_ramps_linearly_into_dry_and_wet() {
    let (mut rsp, mut ram) = setup();
    put(&mut ram, DATA, &[16384; SUBFRAME]);
    let (dry, wet) = (0x7FFF_i64, 0x4000_i64);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(DRY_LEFT, 0x5C0),
            load(0, 0x170, DATA),
            (9 << 24 | 6 << 16, (dry as u32) << 16 | wet as u32), // left volume 0, dry, wet
            (9 << 24 | 4 << 16 | 0x4000, 0),                      // right target, rate 0
            (9 << 24 | 0x4000, 0x0800_0000),                      // left target, +256 per sample
            (3 << 24 | 1 << 16 | 0x4000, STATE),                  // right volume
            save(DRY_LEFT, 0x170, OUT),
            save(DRY_RIGHT, 0x170, OUT + 0x200),
            save(WET_LEFT, 0x170, OUT + 0x400),
        ],
    );
    let mix = |vol: i64, g: i64| (16384 * ((vol * g + 0x4000) >> 15)) >> 15;
    let dl = get(&ram, OUT, SUBFRAME);
    for (k, &v) in dl.iter().enumerate() {
        let vol = (256 * (k as i64 + 1)).min(0x4000);
        assert_eq!(v as i64, mix(vol, dry), "dry left at {k}");
    }
    assert!(get(&ram, OUT + 0x200, SUBFRAME)
        .iter()
        .all(|&v| v as i64 == mix(0x4000, dry)));
    assert_eq!(get(&ram, OUT + 0x400, 1)[0] as i64, mix(256, wet));

    // The next subframe continues at the target.
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(DRY_LEFT, 0x170),
            (3 << 24, STATE),
            save(DRY_LEFT, 2, OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 1)[0] as i64, mix(0x4000, dry));
}

#[test]
fn envelope_adds_with_saturation_at_both_sixteen_bit_limits() {
    let (mut rsp, mut ram) = setup();
    // Main buffer: loud positive, loud negative, then silence.
    let mut main = [0i16; SUBFRAME];
    main[..2].copy_from_slice(&[16384, -16384]);
    put(&mut ram, DATA, &main);
    let mut dry = [0i16; SUBFRAME];
    dry[..2].copy_from_slice(&[32700, -32700]);
    put(&mut ram, DATA + 0x200, &dry);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(DRY_LEFT, 0x5C0),
            load(0, 0x170, DATA),
            load(DRY_LEFT, 0x170, DATA + 0x200),
            load(DRY_RIGHT, 0x170, DATA + 0x200),
            (9 << 24 | 6 << 16 | 0x7FFF, 0x7FFF << 16), // left volume, full dry, no wet
            (9 << 24 | 4 << 16 | 0x7FFF, 0),           // right target, rate 0
            (9 << 24 | 0x7FFF, 0),                     // left target, rate 0
            (3 << 24 | 1 << 16 | 0x7FFF, STATE),       // right volume
            save(DRY_LEFT, 0x170, OUT),
            save(DRY_RIGHT, 0x170, OUT + 0x200),
        ],
    );
    // Each adds about +-16383 to +-32700: both limits clamp, and the silent
    // samples leave the buffer as it was.
    for at in [OUT, OUT + 0x200] {
        let out = get(&ram, at, SUBFRAME);
        assert_eq!(out[..2], [i16::MAX, i16::MIN]);
        assert!(out[2..].iter().all(|&v| v == 0));
    }
}

#[test]
fn pole_filter_runs_on_either_buffer_and_keeps_state() {
    let (mut rsp, mut ram) = setup();
    // Second row 1.0 (Q14) everywhere: with unity gain the filter is a
    // running total of its input.
    let mut b = [0i16; 16];
    b[8..].fill(0x4000);
    put(&mut ram, DATA, &b);
    put(&mut ram, DATA + 0x400, &[1; SUBFRAME]);
    let filter = |flags: u32| {
        [
            book(DATA),
            load(MAIN2, 0x170, DATA + 0x400),
            (14 << 24 | flags << 16 | 0x4000, 1 << 24 | STATE),
            save(MAIN2, 0x170, OUT),
        ]
    };
    run(&mut rsp, &mut ram, &filter(1));
    let want: Vec<i16> = (1..=SUBFRAME as i16).collect();
    assert_eq!(get(&ram, OUT, SUBFRAME), want);
    run(&mut rsp, &mut ram, &filter(0));
    assert_eq!(get(&ram, OUT, 2), [185, 186], "continues from the state");
}

#[test]
fn pole_filter_gain_from_a_real_command_shape() {
    // The first command a game sent: init, gain 0x1800 (0.375), second
    // buffer, state at a physical address.
    let (mut rsp, mut ram) = setup();
    put(&mut ram, DATA, &[0; 16]);
    put(&mut ram, DATA + 0x400, &[8000; SUBFRAME]);
    run(
        &mut rsp,
        &mut ram,
        &[
            book(DATA),
            load(MAIN2, 0x170, DATA + 0x400),
            (0x0E01_1800, 0x0108_51B0),
            save(MAIN2, 0x170, OUT),
        ],
    );
    assert!(get(&ram, OUT, SUBFRAME).iter().all(|&v| v == 3000));
    assert_eq!(get(&ram, 0x0008_51B0, 4), [3000; 4]);
}

#[test]
fn a_game_that_asks_turns_its_stream_frames_into_sound() {
    const UCODE: u32 = 0x8030_0000;
    const BUF: u32 = 0x8031_0000;
    let mut rsp = AudioRsp::n_audio_streams();
    let mut ram = Memory::new(8 << 20);
    // The microcode's synthesis window: rows of D[j + 32k] at 0x310, each
    // stored twice, 0x40 bytes apart. Only the peak and D[5] are set.
    let row = |j: u32, k: u32| UCODE + 0x310 + 0x40 * j + 2 * k;
    for copy in [0, 32] {
        ram.write_u16(row(0, 8) + copy, 0x7FFF);
        ram.write_u16(row(5, 0) + copy, 0x4000);
    }
    rsp.set_microcode_data(UCODE);
    // A frame: four header words (a gain of 1.5), then slot 0's subband 0
    // at 1000.
    ram.write_u32(BUF, 1 << 16 | 0x8000);
    ram.write_u16(BUF + 8, 1000);
    run(
        &mut rsp,
        &mut ram,
        &[(8 << 24, STATE), (7 << 24 | 0x1C, BUF & 0xFF_FFFF)],
    );
    // The first time slot (32 samples) sees only that one window value;
    // later slots see the sample again as it moves through the history.
    let pcm = get(&ram, BUF, 32);
    // cos(21 pi / 64) * 1000 * D[5] * gain, D[5] = 0x4000 in Q15.
    let want = ((21.0 * std::f64::consts::PI / 64.0).cos() * 1000.0 * 0.5 * 1.5).round() as i16;
    let mut expect = vec![0i16; 32];
    expect[5] = want;
    assert_eq!(pcm, expect);
}

#[test]
fn a_game_that_asks_gets_its_biquad_filter() {
    let mut rsp = AudioRsp::n_audio_streams();
    let mut ram = Memory::new(8 << 20);
    // b0 0.25, b1 0.5 (Q15); a1 0.5 (Q14) on y[n-1], a2 0.
    let mut coef = [0i16; 16];
    (coef[0], coef[1], coef[8]) = (8192, 16384, 8192);
    for (i, c) in coef.iter().enumerate() {
        ram.write_u16(DATA + 2 * i as u32, *c as u16);
    }
    put(&mut ram, DATA + 0x100, &[1000]);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(0, SUBFRAME as u32 * 2),
            load(0, 2, DATA + 0x100),
            book(DATA),
            (14 << 24 | 1 << 16, STATE), // initialise the history
            save(0, 16, OUT),
        ],
    );
    // x = 1000, 0, 0...: num = 250, 500, 250, 0...; y adds y[n-1] / 2,
    // each product rounded as the vector unit rounds it.
    assert_eq!(get(&ram, OUT, 6), [250, 626, 564, 282, 142, 72]);
}

#[test]
fn other_games_keep_the_pole_filter_with_the_same_coefficients() {
    let (mut rsp, mut ram) = setup();
    let mut coef = [0i16; 16];
    (coef[0], coef[1], coef[8]) = (8192, 16384, 8192);
    for (i, c) in coef.iter().enumerate() {
        ram.write_u16(DATA + 2 * i as u32, *c as u16);
    }
    put(&mut ram, DATA + 0x100, &[1000]);
    run(
        &mut rsp,
        &mut ram,
        &[
            clear(0, SUBFRAME as u32 * 2),
            load(0, 2, DATA + 0x100),
            book(DATA),
            (14 << 24 | 1 << 16, STATE), // gain 0: the pole filter mutes
            save(0, 16, OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 6), [0; 6]);
}

#[test]
fn unknown_command_is_skipped() {
    let (mut rsp, mut ram) = setup();
    run(&mut rsp, &mut ram, &[(7 << 24, 0), (7 << 24, 0)]);
}
