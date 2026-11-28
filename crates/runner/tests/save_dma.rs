//! Save memory through the replaced PI routine: a tiny host-written "game"
//! reads a save block the file already holds, changes it and writes it to
//! another block. One run per process, so this file holds one test.

use std::collections::BTreeMap;
use std::sync::Arc;

use n64rusty_runner::{os, save::Save, RAM_SIZE};
use n64rusty_runtime::{sx32, Ctx, Memory};

const ENTRY: u32 = 0x8000_1000;
const EPI_START_DMA: u32 = 0x8000_1100;
const HANDLE: u32 = 0x8000_2000;
const MB: u32 = 0x8000_2100;
const BUF: u32 = 0x8000_3000;

/// `osEPiStartDma(handle, mb, dir)` for 8 bytes at save offset `dev`.
fn epi(c: &mut Ctx, dir: u32, dev: u32) {
    c.mem.write_u32(MB + 4, 0); // no message queue
    c.mem.write_u32(MB + 8, BUF);
    c.mem.write_u32(MB + 0x0C, dev);
    c.mem.write_u32(MB + 0x10, 8);
    c.r[4] = sx32(HANDLE);
    c.r[5] = sx32(MB);
    c.r[6] = u64::from(dir);
    c.call(EPI_START_DMA);
}

fn game(c: &mut Ctx) {
    // An SRAM handle, as games build one: base address in KSEG1.
    c.mem.write_u32(HANDLE + 0x0C, 0xA800_0000);
    epi(c, 0, 0x10);
    for k in 0..8 {
        let b = c.mem.read_u8(BUF + k);
        c.mem.write_u8(BUF + k, b + 1);
    }
    // Some games also put the full physical address in the request: the
    // library ORs it with the base, so it is the same block.
    epi(c, 1, 0x0800_0020);
}

#[test]
fn save_block_read_changed_and_written_back() {
    let dir = std::env::temp_dir().join(format!("n64rusty-save-dma-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("save.sra");
    let mut before = vec![0u8; 0x8000];
    before[0x10..0x18].copy_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);
    std::fs::write(&file, &before).unwrap();

    let mut ctx = Ctx::new(Memory::new(RAM_SIZE));
    ctx.register(ENTRY, game);
    ctx.r[29] = sx32(0x8000_0000 + RAM_SIZE as u32 - 0x10);
    let hooks = BTreeMap::from([("osEPiStartDma".to_string(), EPI_START_DMA)]);
    let mut settings = os::Settings::default();
    assert_eq!(
        os::install(&mut ctx, &hooks, &mut settings),
        ["osEPiStartDma"]
    );
    let ctx: &'static mut Ctx = Box::leak(Box::new(ctx));
    let (outcome, report, _) = os::run(
        ctx,
        ENTRY,
        Arc::from(Vec::new()),
        60,
        Box::new(n64rusty_runner::devices::Headless),
        Save::open(&file).unwrap(),
        settings,
    );
    // With no other thread, the boot thread ending reads as a deadlock.
    assert!(
        !matches!(outcome, os::Outcome::Panic(_)),
        "{outcome:?}\n{report}"
    );

    let after = std::fs::read(&file).unwrap();
    assert_eq!(&after[0x20..0x28], &[11, 21, 31, 41, 51, 61, 71, 81]);
    assert_eq!(
        &after[..0x20],
        &before[..0x20],
        "only the written block changes"
    );
    assert_eq!(&after[0x28..], &before[0x28..]);
}
