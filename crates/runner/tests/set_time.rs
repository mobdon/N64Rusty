//! `osSetTime` sets only what `osGetTime` reads: a game that sets the time
//! back must still get its retraces on time. One run per process, so this
//! file holds one test.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use n64rusty_runner::{os, save::Save, RAM_SIZE};
use n64rusty_runtime::{sx32, Ctx, Memory};

const ENTRY: u32 = 0x8000_1000;
const NOOP: u32 = 0x8000_1100;
const SET_TIME: u32 = 0x8000_1200;
const GET_TIME: u32 = 0x8000_1300;

/// Calls made after the time was set, and what `osGetTime` read then.
static CALLS_AFTER: AtomicU64 = AtomicU64::new(0);
static TIME_AFTER: AtomicU64 = AtomicU64::new(u64::MAX);

fn noop(_: &mut Ctx) {}

/// Calls in one frame of guest time (each call ticks the clock by 64).
const CALLS_PER_FRAME: u64 = os::TICKS_PER_FRAME / 64;

fn game(c: &mut Ctx) {
    for _ in 0..10 * CALLS_PER_FRAME {
        c.call(NOOP);
    }
    c.r[4] = 0;
    c.r[5] = 0;
    c.call(SET_TIME);
    c.call(GET_TIME);
    let t = (u64::from(c.r[2] as u32) << 32) | u64::from(c.r[3] as u32);
    TIME_AFTER.store(t, Ordering::SeqCst);
    loop {
        CALLS_AFTER.fetch_add(1, Ordering::SeqCst);
        c.call(NOOP);
    }
}

#[test]
fn setting_the_time_back_keeps_retraces_coming() {
    let mut ctx = Ctx::new(Memory::new(RAM_SIZE));
    ctx.register(ENTRY, game);
    ctx.register(NOOP, noop);
    ctx.r[29] = sx32(0x8000_0000 + RAM_SIZE as u32 - 0x10);
    let hooks = BTreeMap::from([
        ("osSetTime".to_string(), SET_TIME),
        ("osGetTime".to_string(), GET_TIME),
    ]);
    os::install(&mut ctx, &hooks);
    let ctx: &'static mut Ctx = Box::leak(Box::new(ctx));
    let (outcome, report, frames) = os::run(
        ctx,
        ENTRY,
        Arc::from(Vec::new()),
        13,
        Box::new(n64rusty_runner::devices::Headless),
        Save::in_memory(),
    );
    assert_eq!(frames, 13, "{outcome:?}\n{report}");
    assert!(TIME_AFTER.load(Ordering::SeqCst) < os::TICKS_PER_FRAME);
    // Rewinding the clock itself would need ten more frames of calls.
    assert!(CALLS_AFTER.load(Ordering::SeqCst) < 4 * CALLS_PER_FRAME);
}
