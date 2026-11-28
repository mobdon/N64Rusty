//! A guest loop that makes no calls cannot move virtual time; the run must
//! stop with a hang report instead of spinning forever. One run per
//! process, so this file holds one test.

use std::sync::Arc;

use n64rusty_runner::{os, save::Save, RAM_SIZE};
use n64rusty_runtime::{sx32, Ctx, Memory};

const ENTRY: u32 = 0x8000_1000;
const SPIN: u32 = 0x8000_1100;

fn spin(c: &mut Ctx) {
    // Polls a word nothing ever writes, as a game waiting on hardware would.
    while c.mem.read_u32(0x8000_2000) == 0 {
        std::hint::spin_loop();
    }
}

fn game(c: &mut Ctx) {
    c.call(SPIN);
}

#[test]
fn call_free_loop_ends_as_a_hang() {
    let mut ctx = Ctx::new(Memory::new(RAM_SIZE));
    ctx.register(ENTRY, game);
    ctx.register(SPIN, spin);
    ctx.r[29] = sx32(0x8000_0000 + RAM_SIZE as u32 - 0x10);
    let ctx: &'static mut Ctx = Box::leak(Box::new(ctx));
    let (outcome, _, frames) = os::run(
        ctx,
        ENTRY,
        Arc::from(Vec::new()),
        u64::MAX,
        Box::new(n64rusty_runner::devices::Headless),
        Save::in_memory(),
        os::Settings::default(),
    );
    let os::Outcome::Hang(report) = outcome else {
        panic!("expected a hang, got {outcome:?}");
    };
    assert_eq!(frames, 0);
    assert!(
        report.contains("0x80001100"),
        "names the looping call:\n{report}"
    );
}
