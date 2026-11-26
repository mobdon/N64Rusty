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
fn a_game_that_asks_skips_its_stream_commands() {
    let mut rsp = AudioRsp::n_audio_streams();
    let mut ram = Memory::new(8 << 20);
    put(&mut ram, DATA, &[1, 2, 3, 4]);
    run(
        &mut rsp,
        &mut ram,
        &[
            (8 << 24, STATE),
            (7 << 24 | 0x1C, DATA + 0x100),
            load(0, 8, DATA),
            save(0, 8, OUT),
        ],
    );
    assert_eq!(get(&ram, OUT, 4), [1, 2, 3, 4]);
}

#[test]
#[should_panic(expected = "n_audio command 0x7 not implemented")]
fn unknown_command_names_itself() {
    let (mut rsp, mut ram) = setup();
    run(&mut rsp, &mut ram, &[(7 << 24, 0)]);
}
