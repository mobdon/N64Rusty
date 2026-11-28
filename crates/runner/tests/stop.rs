//! The player goes from one game to another in the same program: a run
//! stopped from outside ends, its guest thread never runs again, and a
//! second run in the same process plays normally.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use n64rusty_runner::{os, save::Save, RAM_SIZE};
use n64rusty_runtime::{sx32, Ctx, Memory};

const ENTRY: u32 = 0x8000_1000;
const NOOP: u32 = 0x8000_1100;

/// Calls the endless first game made.
static CALLS: AtomicU64 = AtomicU64::new(0);

fn noop(_: &mut Ctx) {}

fn endless(c: &mut Ctx) {
    loop {
        CALLS.fetch_add(1, Ordering::SeqCst);
        c.call(NOOP);
    }
}

fn game(f: fn(&mut Ctx)) -> &'static mut Ctx {
    let mut ctx = Ctx::new(Memory::new(RAM_SIZE));
    ctx.register(ENTRY, f);
    ctx.register(NOOP, noop);
    ctx.r[29] = sx32(0x8000_0000 + RAM_SIZE as u32 - 0x10);
    Box::leak(Box::new(ctx))
}

fn run(ctx: &'static mut Ctx, frames: u64) -> (os::Outcome, u64) {
    let (outcome, _, frames) = os::run(
        ctx,
        ENTRY,
        Arc::from(Vec::new()),
        frames,
        Box::new(n64rusty_runner::devices::Headless),
        Save::in_memory(),
        os::Settings::default(),
    );
    (outcome, frames)
}

#[test]
fn a_stopped_run_ends_and_the_next_one_plays() {
    let first = std::thread::spawn(|| run(game(endless), u64::MAX));
    while CALLS.load(Ordering::SeqCst) < 1000 {
        std::thread::sleep(Duration::from_millis(1));
    }
    os::stop();
    let (outcome, _) = first.join().unwrap();
    assert!(matches!(outcome, os::Outcome::Stopped), "{outcome:?}");
    os::release();
    let after = CALLS.load(Ordering::SeqCst);

    let (outcome, frames) = run(game(endless), 5);
    assert!(matches!(outcome, os::Outcome::Frames(5)), "{outcome:?}");
    assert_eq!(frames, 5);
    // The second run's calls count too; the first game's thread made none
    // after it stopped, so the count grew by the second run's calls only.
    let second = CALLS.load(Ordering::SeqCst) - after;
    assert!(second > 0);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(CALLS.load(Ordering::SeqCst) - after, second);
}
