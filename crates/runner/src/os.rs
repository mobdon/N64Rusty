//! Replacements for the console's system library: threads, message queues,
//! events, timers, and the device managers (video, cartridge DMA, serial
//! controllers, RSP tasks, audio).
//!
//! Guest threads are host threads, but only one runs at a time: it holds
//! the baton (`State::current`). The shared [`Ctx`] is only touched by the
//! baton holder. A switch saves the CPU registers of the thread giving up
//! the baton and the next thread restores its own. Switches happen only at
//! system calls (blocking receives, sends that wake a higher-priority
//! thread, priority changes, thread starts), which is how the console's
//! priority scheduling behaves apart from interrupt preemption.
//!
//! Time is virtual (`Ctx::clock`). When every thread is waiting, time jumps
//! to the next event: a video retrace (60 per second) or a timer. A thread
//! at priority 0 (the idle thread) is never resumed; it only exists to be
//! preempted.

use std::collections::{BTreeMap, HashMap};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use n64rusty_runtime::{sx32, Ctx, Flash, FlashOp, GuestFn};

use crate::devices::{Devices, Pad};
use crate::save::Save;

/// COP0 Count ticks per second (half the 93.75 MHz CPU clock).
pub const COUNT_HZ: u64 = 46_875_000;
/// Ticks between NTSC video retraces.
pub const TICKS_PER_FRAME: u64 = COUNT_HZ / 60;

/// System event numbers used by `osSetEventMesg`.
pub mod event {
    pub const SP: u32 = 4;
    pub const SI: u32 = 5;
    pub const AI: u32 = 6;
    pub const VI: u32 = 7;
    pub const PI: u32 = 8;
    pub const DP: u32 = 9;
}

/// Guest address of the cartridge handle `osCartRomInit` returns. Low RAM
/// between the boot information and the game is free.
const CART_HANDLE: u32 = 0x8000_0340;

/// How a run ended.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Reached the frame limit.
    Frames(u64),
    /// A guest thread panicked.
    Panic(String),
    /// Every thread waits and no event can wake one.
    Deadlock(String),
    /// The boot code returned without starting a thread.
    Returned,
    /// No retrace for [`HANG_WALL`] of host time: a guest loop that makes
    /// no calls, so virtual time cannot move. Threads and recent calls.
    Hang(String),
    /// Ended from outside by [`stop`]: the player went to another game.
    Stopped,
}

/// Host time without a single retrace after which a run counts as hung.
/// A game frame takes milliseconds headless; any call advances the clock.
pub const HANG_WALL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    Recv(u32),
    Send(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TState {
    Stopped,
    Ready,
    Waiting(Wait),
    Dead,
}

struct Regs {
    r: [u64; 32],
    hi: u64,
    lo: u64,
    f: [u64; 32],
    fcr31: u32,
    fr: bool,
    status: u32,
}

impl Regs {
    fn save(c: &Ctx) -> Box<Self> {
        Box::new(Regs {
            r: c.r,
            hi: c.hi,
            lo: c.lo,
            f: c.f,
            fcr31: c.fcr31,
            fr: c.fr,
            status: c.cop0[12],
        })
    }

    fn load(&self, c: &mut Ctx) {
        c.r = self.r;
        c.hi = self.hi;
        c.lo = self.lo;
        c.f = self.f;
        c.fcr31 = self.fcr31;
        c.fr = self.fr;
        c.cop0[12] = self.status;
    }
}

struct Thread {
    id: u32,
    pri: i32,
    state: TState,
    entry: u32,
    arg: u32,
    sp: u32,
    spawned: bool,
    regs: Option<Box<Regs>>,
    /// Order of becoming ready, for round robin among equal priorities.
    seq: u64,
    /// Which osCreateThread made this thread. A game may create a new
    /// thread in the memory of one that is only stopped; the host thread
    /// left from the old one must then never run again.
    generation: u64,
    /// Woken only when this thread gets the CPU, so a switch wakes one
    /// host thread instead of all of them.
    cv: Arc<Condvar>,
}

thread_local! {
    /// The generation of the guest thread this host thread runs.
    static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Whether this host thread still runs the guest thread `id`.
fn still_mine(st: &State, id: u32) -> bool {
    st.threads
        .get(&id)
        .is_some_and(|t| t.generation == GENERATION.with(|g| g.get()))
}

struct Timer {
    at: u64,
    interval: u64,
    mq: u32,
    msg: u32,
}

/// Everything the system library replacements keep.
pub struct State {
    threads: BTreeMap<u32, Thread>,
    current: Option<u32>,
    seq: u64,
    events: HashMap<u32, (u32, u32)>,
    vi: Option<(u32, u32, u32)>,
    timers: BTreeMap<u32, Timer>,
    next_vi: u64,
    /// What `osSetTime` added to the clock for `osGetTime`. The clock
    /// itself (Count, retraces, timers) never goes back.
    time_offset: u64,
    /// Video retraces so far.
    pub frames: u64,
    frame_limit: u64,
    done: Option<Outcome>,
    rom: Arc<[u8]>,
    framebuffer: u32,
    devices: Box<dyn Devices>,
    save: Save,
    /// Cartridge DMA requests seen: (ROM offset, RAM address, length).
    pub dma_log: Vec<(u32, u32, u32)>,
    /// The serial interface's 64 bytes of PIF RAM.
    pif: [u8; 64],
    /// Report text for a panic, filled by the panicking thread.
    report: String,
    /// The page buffer of a flash save chip.
    flash_page: [u8; 128],
    /// The audio interface's buffers, played on the guest clock.
    ai: Ai,
    /// The baton holder saw the run end and will never run guest code again.
    halted: bool,
}

/// The audio interface's two-buffer queue, timed by the guest clock, so a
/// game that sizes its next buffer from what is left to play
/// (`osAiGetLength`) makes as much sound as the time that passed.
#[derive(Default)]
struct Ai {
    /// Samples per second (`osAiSetFrequency`).
    rate: u64,
    /// Queued buffers: when each ends (clock) and its length in bytes.
    queue: Vec<(u64, u32)>,
}

impl Ai {
    /// Queues `len` bytes at clock `now`, after what is already queued.
    fn push(&mut self, now: u64, len: u32) {
        self.retire(now);
        if self.rate == 0 {
            return;
        }
        let start = self.queue.last().map_or(now, |b| b.0.max(now));
        let ticks = u64::from(len / 4) * COUNT_HZ / self.rate;
        self.queue.push((start + ticks, len));
    }

    /// Bytes still to play of the buffer playing at `now`.
    fn length(&mut self, now: u64) -> u32 {
        self.retire(now);
        let Some(&(end, len)) = self.queue.first() else {
            return 0;
        };
        let left = (end - now) * self.rate / COUNT_HZ * 4;
        (left.min(u64::from(len)) as u32) & !7
    }

    /// Status register: bit 31 when both buffers are taken, bit 30 busy.
    fn status(&mut self, now: u64) -> u32 {
        self.retire(now);
        match self.queue.len() {
            0 => 0,
            1 => 1 << 30,
            _ => 3 << 30,
        }
    }

    fn retire(&mut self, now: u64) {
        self.queue.retain(|b| b.0 > now);
    }
}

struct Os {
    state: Mutex<State>,
    cv: Condvar,
    ctx: AtomicPtr<Ctx>,
}

thread_local! {
    /// The run this host thread belongs to. Each run has its own state, so
    /// threads of a stopped run, parked forever, never touch the next one.
    static MINE: std::cell::Cell<Option<&'static Os>> = const { std::cell::Cell::new(None) };
}

/// The newest run, for [`stop`] and [`release`] from outside its threads.
static LAST: AtomicPtr<Os> = AtomicPtr::new(std::ptr::null_mut());

fn last() -> Option<&'static Os> {
    // SAFETY: set only from a leaked box, never freed.
    unsafe { LAST.load(Ordering::Acquire).as_ref() }
}

/// Ends the newest run, as the player does to go to another game: the
/// thread on the CPU stops at its next system call or interrupt and no
/// guest thread runs again; [`run`] returns [`Outcome::Stopped`]. Returns
/// once that thread has stopped, or after two seconds of host time (a
/// guest loop that makes no calls cannot be stopped).
pub fn stop() {
    let Some(os) = last() else { return };
    let mut st = os.state.lock().unwrap_or_else(|e| e.into_inner());
    let running = st.done.is_none();
    if running {
        st.done = Some(Outcome::Stopped);
    }
    os.cv.notify_all();
    // A guest thread (a mod's hook) stopping its own run halts after it
    // returns: waiting here would only wait out the two seconds.
    if running && MINE.get().is_none() {
        let _ = os
            .cv
            .wait_timeout_while(st, std::time::Duration::from_secs(2), |s| !s.halted)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// Closes the newest run's devices (sound, window side) once it has ended:
/// its state is never freed, as parked threads keep using its lock.
pub fn release() {
    if let Some(os) = last() {
        let mut st = os.state.lock().unwrap_or_else(|e| e.into_inner());
        st.devices = Box::new(crate::devices::Headless);
        st.save = Save::in_memory();
    }
}

/// A memory write a run makes at each retrace from `from` to `to` (see
/// `--poke`): a debugging aid, for example to start a game somewhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Poke {
    pub from: u64,
    pub to: u64,
    pub addr: u32,
    pub value: u32,
    /// 1, 2 or 4.
    pub bytes: u8,
}

static POKES: Mutex<Vec<Poke>> = Mutex::new(Vec::new());

/// Where `--hash-out` writes one RAM hash per retrace.
static HASH_OUT: Mutex<Option<std::io::BufWriter<std::fs::File>>> = Mutex::new(None);

/// Makes the run write `frame hash` for RAM at every retrace, to compare
/// two runs (determinism, or translated code against the interpreter).
pub fn set_hash_out(file: std::fs::File) {
    *HASH_OUT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::io::BufWriter::new(file));
}

/// A fast hash of RAM, 8 bytes at a time.
pub fn ram_hash(ram: &[u8]) -> u64 {
    ram.chunks(8).fold(0xCBF2_9CE4_8422_2325, |h, w| {
        let mut b = [0; 8];
        b[..w.len()].copy_from_slice(w);
        (h ^ u64::from_le_bytes(b))
            .wrapping_mul(0x0100_0000_01B3)
            .rotate_left(29)
    })
}

fn write_hash(c: &Ctx, frame: u64) {
    use std::io::Write;
    if let Some(out) = HASH_OUT.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        // A short file would read as two runs that differ: stop instead.
        if let Err(e) =
            writeln!(out, "{frame} {:016x}", ram_hash(c.mem.bytes())).and_then(|()| out.flush())
        {
            panic!("--hash-out: {e}");
        }
    }
}

/// Sets the memory writes this run makes.
pub fn set_pokes(pokes: Vec<Poke>) {
    *POKES.lock().unwrap_or_else(|e| e.into_inner()) = pokes;
}

/// Parses a poke script: `frame[-last] addr value [bytes]` per line, frame
/// numbers in decimal (retraces), address and value in hex, bytes 1, 2 or 4
/// (default 4). `#` starts a comment.
pub fn parse_pokes(text: &str) -> Result<Vec<Poke>, String> {
    let hex = |t: &str| u32::from_str_radix(t.trim_start_matches("0x"), 16).ok();
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("line {}: expected `frame[-last] addr value [bytes]`", n + 1);
        let (Some(frames), Some(addr), Some(value)) = (f.first(), f.get(1), f.get(2)) else {
            return Err(bad());
        };
        let (from, to) = frames.split_once('-').unwrap_or((frames, frames));
        let bytes = f.get(3).map_or(Some(4), |b| b.parse().ok());
        match (from.parse(), to.parse(), hex(addr), hex(value), bytes) {
            (Ok(from), Ok(to), Some(addr), Some(value), Some(bytes @ (1 | 2 | 4))) => {
                out.push(Poke {
                    from,
                    to,
                    addr,
                    value,
                    bytes,
                })
            }
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

fn apply_pokes(c: &mut Ctx, frame: u64) {
    for p in POKES.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if (p.from..=p.to).contains(&frame) {
            match p.bytes {
                1 => c.mem.write_u8(p.addr, p.value as u8),
                2 => c.mem.write_u16(p.addr, p.value as u16),
                _ => c.mem.write_u32(p.addr, p.value),
            }
        }
    }
}

/// Text of the last panic, set by the panic hook.
pub static PANIC_MESSAGE: Mutex<String> = Mutex::new(String::new());

fn os() -> &'static Os {
    MINE.get().expect("system library state not set up")
}

fn lock() -> MutexGuard<'static, State> {
    os().state.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shared context. Only the baton holder may call this.
#[allow(clippy::mut_from_ref)]
fn shared_ctx() -> &'static mut Ctx {
    // SAFETY: the pointer is set once from a context that lives for the
    // whole run, and only the thread holding the baton dereferences it.
    unsafe { &mut *os().ctx.load(Ordering::Acquire) }
}

// ---------------------------------------------------------------------------
// Argument helpers (o32 calling convention)
// ---------------------------------------------------------------------------

fn arg(c: &Ctx, n: usize) -> u32 {
    if n < 4 {
        c.r[4 + n] as u32
    } else {
        c.mem.read_u32((c.r[29] as u32).wrapping_add(4 * n as u32))
    }
}

/// A 64-bit argument passed in the register pair starting at `n` (even).
fn arg64(c: &Ctx, n: usize) -> u64 {
    (u64::from(arg(c, n)) << 32) | u64::from(arg(c, n + 1))
}

fn ret(c: &mut Ctx, v: u32) {
    c.r[2] = sx32(v);
}

fn ret64(c: &mut Ctx, v: u64) {
    c.r[2] = sx32((v >> 32) as u32);
    c.r[3] = sx32(v as u32);
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

impl State {
    fn me(&self) -> u32 {
        self.current.expect("no current thread")
    }

    fn best_ready(&self) -> Option<u32> {
        self.threads
            .values()
            .filter(|t| t.state == TState::Ready && t.pri > 0)
            .max_by(|a, b| a.pri.cmp(&b.pri).then(b.seq.cmp(&a.seq)))
            .map(|t| t.id)
    }

    fn make_ready(&mut self, id: u32) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(t) = self.threads.get_mut(&id) {
            t.state = TState::Ready;
            t.seq = seq;
        }
    }

    fn finish(&mut self, o: Outcome) {
        if self.done.is_none() {
            self.done = Some(o);
        }
        os().cv.notify_all();
    }

    fn describe_threads(&self) -> String {
        self.threads
            .values()
            .map(|t| {
                // A switched-out thread is inside a system call: its return
                // address says which guest code made it.
                let from = t.regs.as_ref().map_or(String::new(), |r| {
                    format!(" (call at {:#010x})", (r.r[31] as u32).wrapping_sub(8))
                });
                format!("  thread {:#010x} pri {} {:?}{from}", t.id, t.pri, t.state)
            })
            .chain(self.timers.iter().map(|(k, t)| {
                format!(
                    "  timer {k:#010x} due at count {:#x} posts to {:#010x}",
                    t.at, t.mq
                )
            }))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Puts a message on a guest queue if there is room, waking the
/// highest-priority receiver. Returns false if the queue is full.
fn put_message(c: &mut Ctx, st: &mut State, mq: u32, msg: u32, jam: bool) -> bool {
    let valid = c.mem.read_u32(mq + 8);
    let first = c.mem.read_u32(mq + 12);
    let count = c.mem.read_u32(mq + 16);
    let buf = c.mem.read_u32(mq + 20);
    if count == 0 || valid >= count {
        return false;
    }
    assert!(
        first < count,
        "message queue {mq:#010x} is not set up: valid {valid}, first {first}, count {count}, buffer {buf:#010x}, called from {:#010x}",
        c.r[31] as u32
    );
    if jam {
        let f = (first + count - 1) % count;
        c.mem.write_u32(buf + 4 * f, msg);
        c.mem.write_u32(mq + 12, f);
    } else {
        let slot = (first + valid) % count;
        c.mem.write_u32(buf + 4 * slot, msg);
    }
    c.mem.write_u32(mq + 8, valid + 1);
    let waiter = st
        .threads
        .values()
        .filter(|t| t.state == TState::Waiting(Wait::Recv(mq)))
        .max_by_key(|t| t.pri)
        .map(|t| t.id);
    if let Some(w) = waiter {
        st.make_ready(w);
    }
    true
}

/// Takes the first message from a guest queue, waking a blocked sender.
fn take_message(c: &mut Ctx, st: &mut State, mq: u32) -> Option<u32> {
    let valid = c.mem.read_u32(mq + 8);
    if valid == 0 {
        return None;
    }
    let first = c.mem.read_u32(mq + 12);
    let count = c.mem.read_u32(mq + 16);
    let buf = c.mem.read_u32(mq + 20);
    let msg = c.mem.read_u32(buf + 4 * first);
    c.mem.write_u32(mq + 12, (first + 1) % count);
    c.mem.write_u32(mq + 8, valid - 1);
    let waiter = st
        .threads
        .values()
        .filter(|t| t.state == TState::Waiting(Wait::Send(mq)))
        .max_by_key(|t| t.pri)
        .map(|t| t.id);
    if let Some(w) = waiter {
        st.make_ready(w);
    }
    Some(msg)
}

/// Posts the message registered for a system event, if any.
fn post_event(c: &mut Ctx, st: &mut State, e: u32) {
    if let Some(&(mq, msg)) = st.events.get(&e) {
        put_message(c, st, mq, msg, false);
    }
}

/// Sets the audio interface's length and status registers for games that
/// read them straight (their own `osAiGetLength`), as of now.
fn ai_regs(c: &mut Ctx, st: &mut State) {
    let (len, status) = (st.ai.length(c.clock), st.ai.status(c.clock));
    if let Some(io) = c.mem.io_mut() {
        io.regs.insert(0x0450_0004, len);
        io.regs.insert(0x0450_000C, status);
    }
}

/// Delivers retraces and timers that are due by the current clock.
fn deliver_due(c: &mut Ctx, st: &mut State) {
    ai_regs(c, st);
    while c.clock >= st.next_vi {
        flash_ops(c, st);
        st.next_vi += TICKS_PER_FRAME;
        st.frames += 1;
        apply_pokes(c, st.frames);
        write_hash(c, st.frames);
        st.devices.retrace(st.frames, st.framebuffer);
        if let Some((mq, msg, every)) = st.vi {
            if every > 0 && st.frames.is_multiple_of(u64::from(every)) {
                put_message(c, st, mq, msg, false);
            }
        }
        if st.frames >= st.frame_limit {
            st.finish(Outcome::Frames(st.frames));
        }
    }
    let due: Vec<u32> = st
        .timers
        .iter()
        .filter(|(_, t)| c.clock >= t.at)
        .map(|(&k, _)| k)
        .collect();
    for k in due {
        let t = st.timers.remove(&k).unwrap();
        put_message(c, st, t.mq, t.msg, false);
        if t.interval > 0 {
            st.timers.insert(
                k,
                Timer {
                    at: t.at + t.interval,
                    ..t
                },
            );
        }
    }
}

/// COP0 Status for the boot code: CU1, CU0, FR, all interrupt lines, IE.
const STATUS_BOOT: u32 = 0x3400_FF01;
/// Status.FR: 64-bit floating-point registers.
const STATUS_FR: u32 = 0x0400_0000;
/// The Status word this game's library gives new threads (config name
/// `thread_status`), or 0 for the usual one.
static THREAD_STATUS: AtomicU32 = AtomicU32::new(0);
/// COP0 Status a new thread starts with: CU1, all interrupt lines, IE.
const STATUS_THREAD: u32 = 0x2000_FF01;

/// Sets the clock deadline at the next retrace or timer, so a thread that
/// never blocks is still interrupted.
fn arm(c: &mut Ctx, st: &State) {
    let next_timer = st.timers.values().map(|t| t.at).min();
    c.deadline = next_timer.map_or(st.next_vi, |t| t.min(st.next_vi));
}

/// Interrupt: the clock passed the deadline during a call. Delivers what is
/// due and switches if that woke a thread of higher priority.
fn interrupt(c: &mut Ctx) {
    // Status.IE clear (inside __osDisableInt .. __osRestoreInt), or both
    // lines that carry these events masked (the RCP's, IP2, and the
    // timer's, IP7: osSetIntMask): try again on a later call.
    // shortcut: either line lets every event through; tell retraces from
    // timers if a game masks only one of them.
    let masked = INT_MASK.load(Ordering::Relaxed) && c.cop0[12] & 0x8400 == 0;
    if c.cop0[12] & 1 == 0 || masked {
        c.deadline = c.clock;
        return;
    }
    let mut st = lock();
    if st.current.is_none() {
        return;
    }
    deliver_due(c, &mut st);
    let me = st.me();
    let mine = st.threads.get(&me).map_or(0, |t| t.pri);
    let higher = st
        .best_ready()
        .is_some_and(|b| b != me && st.threads[&b].pri > mine);
    if higher || st.done.is_some() {
        st = reschedule(c, st);
    }
    arm(c, &st);
}

/// Runs the best ready thread. Returns once the calling thread holds the
/// baton again (immediately if it is still the best). A thread that is
/// dead does not come back.
fn reschedule(c: &mut Ctx, mut st: MutexGuard<'static, State>) -> MutexGuard<'static, State> {
    let me = st.me();
    loop {
        if st.done.is_some() {
            st.halted = true;
            os().cv.notify_all();
            park(st);
        }
        deliver_due(c, &mut st);
        if st.done.is_some() {
            continue;
        }
        let Some(best) = st.best_ready() else {
            // Everyone waits: jump to the next event.
            let next_timer = st.timers.values().map(|t| t.at).min();
            let next = next_timer.map_or(st.next_vi, |t| t.min(st.next_vi));
            if st.vi.is_none() && next_timer.is_none() {
                let d = st.describe_threads();
                st.finish(Outcome::Deadlock(d));
                continue;
            }
            c.clock = c.clock.max(next);
            continue;
        };
        if best == me {
            arm(c, &st);
            return st;
        }
        // Hand over.
        let mine = st.threads.get(&me).map(|t| t.state);
        if let Some(t) = st.threads.get_mut(&me) {
            t.regs = Some(Regs::save(c));
        }
        st.current = Some(best);
        let spawn = !st.threads[&best].spawned;
        if spawn {
            st.threads.get_mut(&best).unwrap().spawned = true;
            let generation = st.threads[&best].generation;
            spawn_thread(best, generation);
        }
        st.threads[&best].cv.notify_one();
        // The main thread watches for the boot thread ending.
        os().cv.notify_all();
        if mine == Some(TState::Dead) {
            // A finished thread never runs guest code again.
            park(st);
        }
        st = wait_turn(st, me);
        if !still_mine(&st, me) {
            // Replaced by a new thread in the same memory: retire.
            park(st);
        }
        if let Some(regs) = st.threads.get_mut(&me).and_then(|t| t.regs.take()) {
            regs.load(c);
        }
        arm(c, &st);
        return st;
    }
}

/// Waits forever: the run is over and the main thread is reporting.
fn park(mut st: MutexGuard<'static, State>) -> ! {
    // A panic on another guest thread poisons the lock; keep waiting so
    // that panic is the one reported.
    let never = Condvar::new();
    loop {
        st = never.wait(st).unwrap_or_else(|e| e.into_inner());
    }
}

/// Waits until thread `id` has the CPU. A thread replaced by a new one in
/// the same memory is never woken here, and retires anyway.
fn wait_turn(st: MutexGuard<'static, State>, id: u32) -> MutexGuard<'static, State> {
    let Some(cv) = st.threads.get(&id).map(|t| t.cv.clone()) else {
        park(st)
    };
    cv.wait_while(st, |s| s.current != Some(id) || s.done.is_some())
        .unwrap_or_else(|e| e.into_inner())
}

fn spawn_thread(id: u32, generation: u64) {
    let run = os();
    std::thread::Builder::new()
        .name(format!("guest-{id:08x}"))
        .stack_size(256 << 20)
        .spawn(move || {
            MINE.set(Some(run));
            GENERATION.with(|g| g.set(generation));
            let st = lock();
            let st = wait_turn(st, id);
            if !still_mine(&st, id) {
                park(st);
            }
            let c = shared_ctx();
            let (entry, arg, sp) = {
                let t = &st.threads[&id];
                (t.entry, t.arg, t.sp)
            };
            c.r = [0; 32];
            c.r[4] = sx32(arg);
            // osCreateThread starts the stack 16 bytes below the top it is
            // given: the entry function keeps its arguments there.
            c.r[29] = sx32(sp.wrapping_sub(16));
            // The boot code runs as the boot ROM left it, with 64-bit FPU
            // registers (Status.FR set). Threads start from the context the
            // system library builds, whose Status has FR clear: doubles
            // live in even/odd register pairs.
            // A game whose library differs names its own status word.
            let status = match THREAD_STATUS.load(Ordering::Relaxed) {
                0 => STATUS_THREAD,
                s => s,
            };
            c.fr = id == 0 || status & STATUS_FR != 0;
            c.cop0[12] = if id == 0 { STATUS_BOOT } else { status };
            drop(st);
            let r = panic::catch_unwind(AssertUnwindSafe(|| c.call(entry)));
            let mut st = lock();
            match r {
                Err(_) => {
                    st.report = crate::crash_report(c);
                    let m = PANIC_MESSAGE.lock().unwrap().clone();
                    st.finish(Outcome::Panic(m));
                }
                Ok(()) => {
                    if let Some(t) = st.threads.get_mut(&id) {
                        t.state = TState::Dead;
                    }
                    let _st = reschedule(c, st);
                }
            }
        })
        .expect("spawning a guest thread");
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Runs `entry` as the boot thread until the frame limit, a panic or a
/// deadlock. `ctx` must stay alive and untouched until this returns.
pub fn run(
    ctx: &mut Ctx,
    entry: u32,
    rom: Arc<[u8]>,
    frame_limit: u64,
    devices: Box<dyn Devices>,
    save: Save,
) -> (Outcome, String, u64) {
    ctx.on_deadline = Some(interrupt);
    ctx.cop0[12] = STATUS_BOOT;
    ctx.deadline = TICKS_PER_FRAME;
    // Never freed: a stopped run's parked threads keep waiting on its lock.
    let this: &'static Os = Box::leak(Box::new(Os {
        state: Mutex::new(State {
            threads: BTreeMap::new(),
            current: Some(0),
            seq: 0,
            events: HashMap::new(),
            vi: None,
            timers: BTreeMap::new(),
            next_vi: TICKS_PER_FRAME,
            time_offset: 0,
            frames: 0,
            frame_limit,
            done: None,
            rom,
            framebuffer: 0,
            devices,
            save,
            dma_log: Vec::new(),
            pif: [0; 64],
            report: String::new(),
            flash_page: [0xFF; 128],
            ai: Ai::default(),
            halted: false,
        }),
        cv: Condvar::new(),
        ctx: AtomicPtr::new(ctx as *mut Ctx),
    }));
    MINE.set(Some(this));
    LAST.store(this as *const Os as *mut Os, Ordering::Release);
    // The boot code runs on its own host thread like every guest thread,
    // as thread 0 at the highest priority.
    lock().threads.insert(
        0,
        Thread {
            id: 0,
            pri: i32::MAX,
            state: TState::Ready,
            entry,
            arg: 0,
            sp: ctx.r[29] as u32,
            spawned: true,
            regs: None,
            seq: 0,
            generation: 0,
            cv: Arc::default(),
        },
    );
    spawn_thread(0, 0);
    let mut st = lock();
    let mut seen = u64::MAX;
    loop {
        let (g, wait) = os()
            .cv
            .wait_timeout_while(st, HANG_WALL, |s| s.done.is_none() && !boot_returned(s))
            .unwrap_or_else(|e| e.into_inner());
        st = g;
        if !wait.timed_out() {
            break;
        }
        if st.frames == seen {
            // SAFETY: none. The guest thread is still running and may write
            // registers while this reads them: a data race on plain
            // integers, accepted because the values only go into a report
            // printed just before the process exits. The call trace itself
            // is not written by a loop that makes no calls.
            // shortcut: racy read for diagnostics; make the trace atomic if this report is ever trusted for more.
            let c: &Ctx = unsafe { &*os().ctx.load(Ordering::Acquire) };
            let d = format!(
                "{}\nthe running thread is {:?}\n{}",
                st.describe_threads(),
                st.current,
                crate::crash_report(c)
            );
            st.done = Some(Outcome::Hang(d));
            break;
        }
        seen = st.frames;
    }
    let outcome = st.done.clone().unwrap_or(Outcome::Returned);
    let report = std::mem::take(&mut st.report);
    (outcome, report, st.frames)
}

fn boot_returned(s: &State) -> bool {
    s.threads.get(&0).is_some_and(|t| t.state == TState::Dead)
        && s.threads.values().all(|t| t.state == TState::Dead)
}

/// The DMA log and frame count after a run, for reports.
pub fn summary() -> (u64, Vec<(u32, u32, u32)>, String, String) {
    let mut st = lock();
    let devices = st.devices.summary();
    (
        st.frames,
        st.dma_log.clone(),
        st.describe_threads(),
        devices,
    )
}

// ---------------------------------------------------------------------------
// Replacements. Each is a `fn(&mut Ctx)` installed over the game's routine.
// ---------------------------------------------------------------------------

fn os_create_thread(c: &mut Ctx) {
    let (t, id, entry, a, sp, pri) = (
        arg(c, 0),
        arg(c, 1),
        arg(c, 2),
        arg(c, 3),
        arg(c, 4),
        arg(c, 5) as i32,
    );
    c.mem.write_u32(t + 4, pri as u32);
    c.mem.write_u32(t + 0x14, id);
    let mut st = lock();
    st.seq += 1;
    let generation = st.seq;
    st.threads.insert(
        t,
        Thread {
            id: t,
            pri,
            state: TState::Stopped,
            entry,
            arg: a,
            sp,
            spawned: false,
            regs: None,
            seq: 0,
            generation,
            cv: Arc::default(),
        },
    );
}

fn os_start_thread(c: &mut Ctx) {
    let t = arg(c, 0);
    let mut st = lock();
    if st
        .threads
        .get(&t)
        .is_some_and(|x| x.state == TState::Stopped)
    {
        st.make_ready(t);
    }
    // The boot thread gives way for good once a thread is started.
    if st.me() == 0 {
        st.threads.get_mut(&0).unwrap().state = TState::Dead;
    }
    let _st = reschedule(c, st);
}

fn os_stop_thread(c: &mut Ctx) {
    let mut st = lock();
    let t = match arg(c, 0) {
        0 => st.me(),
        t => t,
    };
    if let Some(x) = st.threads.get_mut(&t) {
        x.state = TState::Stopped;
    }
    let _st = reschedule(c, st);
}

fn os_destroy_thread(c: &mut Ctx) {
    let mut st = lock();
    let t = match arg(c, 0) {
        0 => st.me(),
        t => t,
    };
    if let Some(x) = st.threads.get_mut(&t) {
        x.state = TState::Dead;
    }
    let _st = reschedule(c, st);
}

fn os_set_thread_pri(c: &mut Ctx) {
    let mut st = lock();
    let t = match arg(c, 0) {
        0 => st.me(),
        t => t,
    };
    let pri = arg(c, 1) as i32;
    if let Some(x) = st.threads.get_mut(&t) {
        x.pri = pri;
    }
    if t != 0 {
        c.mem.write_u32(t + 4, pri as u32);
    }
    let _st = reschedule(c, st);
}

fn os_get_thread_pri(c: &mut Ctx) {
    let st = lock();
    let t = match arg(c, 0) {
        0 => st.me(),
        t => t,
    };
    let p = st.threads.get(&t).map_or(0, |x| x.pri);
    drop(st);
    ret(c, p as u32);
}

fn os_get_thread_id(c: &mut Ctx) {
    let st = lock();
    let t = match arg(c, 0) {
        0 => st.me(),
        t => t,
    };
    drop(st);
    let id = if t == 0 { 0 } else { c.mem.read_u32(t + 0x14) };
    ret(c, id);
}

fn os_yield_thread(c: &mut Ctx) {
    let mut st = lock();
    let me = st.me();
    st.make_ready(me);
    let _st = reschedule(c, st);
}

fn os_create_mesg_queue(c: &mut Ctx) {
    let (mq, buf, count) = (arg(c, 0), arg(c, 1), arg(c, 2));
    c.mem.write_u32(mq, 0);
    c.mem.write_u32(mq + 4, 0);
    c.mem.write_u32(mq + 8, 0);
    c.mem.write_u32(mq + 12, 0);
    c.mem.write_u32(mq + 16, count);
    c.mem.write_u32(mq + 20, buf);
}

fn send(c: &mut Ctx, jam: bool) {
    let (mq, msg, block) = (arg(c, 0), arg(c, 1), arg(c, 2) != 0);
    let mut st = lock();
    loop {
        if put_message(c, &mut st, mq, msg, jam) {
            st = reschedule(c, st);
            drop(st);
            ret(c, 0);
            return;
        }
        if !block {
            drop(st);
            ret(c, u32::MAX);
            return;
        }
        let me = st.me();
        st.threads.get_mut(&me).unwrap().state = TState::Waiting(Wait::Send(mq));
        st = reschedule(c, st);
    }
}

fn os_send_mesg(c: &mut Ctx) {
    send(c, false);
}

fn os_jam_mesg(c: &mut Ctx) {
    send(c, true);
}

fn os_recv_mesg(c: &mut Ctx) {
    let (mq, out, block) = (arg(c, 0), arg(c, 1), arg(c, 2) != 0);
    let mut st = lock();
    loop {
        deliver_due(c, &mut st);
        if let Some(m) = take_message(c, &mut st, mq) {
            if out != 0 {
                c.mem.write_u32(out, m);
            }
            st = reschedule(c, st);
            drop(st);
            ret(c, 0);
            return;
        }
        if !block {
            drop(st);
            ret(c, u32::MAX);
            return;
        }
        let me = st.me();
        if me == 0 {
            // The boot code waiting before any thread exists: let time pass.
            c.clock = c.clock.max(st.next_vi);
            continue;
        }
        st.threads.get_mut(&me).unwrap().state = TState::Waiting(Wait::Recv(mq));
        st = reschedule(c, st);
    }
}

fn os_set_event_mesg(c: &mut Ctx) {
    let (e, mq, msg) = (arg(c, 0), arg(c, 1), arg(c, 2));
    lock().events.insert(e, (mq, msg));
}

fn os_vi_set_event(c: &mut Ctx) {
    let (mq, msg, every) = (arg(c, 0), arg(c, 1), arg(c, 2));
    lock().vi = Some((mq, msg, every));
}

fn os_vi_swap_buffer(c: &mut Ctx) {
    let fb = arg(c, 0);
    let mut st = lock();
    st.framebuffer = fb;
    let frame = st.frames;
    st.devices.swap_buffer(c, frame, fb);
}

fn os_vi_get_framebuffer(c: &mut Ctx) {
    let fb = lock().framebuffer;
    ret(c, fb);
}

fn os_get_time(c: &mut Ctx) {
    let t = c.clock.wrapping_add(lock().time_offset);
    ret64(c, t);
}

/// Sets only what `osGetTime` reads, as the library does: rewinding the
/// clock itself would stop retraces and timers until it caught up again.
fn os_set_time(c: &mut Ctx) {
    let offset = arg64(c, 0).wrapping_sub(c.clock);
    lock().time_offset = offset;
}

fn os_get_count(c: &mut Ctx) {
    let t = c.clock as u32;
    ret(c, t);
}

fn os_set_timer(c: &mut Ctx) {
    let t = arg(c, 0);
    let countdown = arg64(c, 2);
    let interval = arg64(c, 4);
    let (mq, msg) = (arg(c, 6), arg(c, 7));
    let at = c.clock + if countdown > 0 { countdown } else { interval };
    lock().timers.insert(
        t,
        Timer {
            at,
            interval,
            mq,
            msg,
        },
    );
    ret(c, 0);
}

fn os_stop_timer(c: &mut Ctx) {
    let t = arg(c, 0);
    let had = lock().timers.remove(&t).is_some();
    ret(c, if had { 0 } else { u32::MAX });
}

/// One PI transfer of `len` bytes between RAM and device address `dev`:
/// `dir` 0 reads the device, 1 writes it. Save memory is read and written;
/// cartridge ROM is only read.
fn pi_dma(c: &mut Ctx, st: &mut State, dir: u32, dev: u32, ram: u32, len: u32) {
    let Some(off) = Save::sram_offset(dev & 0x1FFF_FFFF) else {
        if dir == 0 {
            cart_copy(c, st, dev, ram, len);
        }
        return;
    };
    flash_ops(c, st);
    let at = (ram & 0x1FFF_FFFF) as usize;
    let n = len as usize;
    if at + n > c.mem.bytes().len() {
        panic!("save memory DMA at {ram:#010x}+{len:#x} runs past RAM");
    }
    let flash = c
        .mem
        .io_mut()
        .map(|io| io.flash.mode)
        .filter(|_| flash_used(c));
    let bytes = c.mem.bytes_mut();
    match (flash, dir) {
        (None, 0) => st.save.read(off, &mut bytes[at..at + n]),
        (None, _) => st.save.write(off, &bytes[at..at + n]),
        // Flash reads its array at twice the bus offset.
        (Some(0xF0), 0) => st.save.read(off * 2, &mut bytes[at..at + n]),
        (Some(0xE1), 0) => {
            for k in 0..n {
                bytes[at + k] = Flash::ID[k % 8];
            }
        }
        (Some(0xB4), 1) => {
            let k = n.min(128);
            st.flash_page[..k].copy_from_slice(&bytes[at..at + k]);
        }
        _ => {}
    }
}

fn flash_used(c: &mut Ctx) -> bool {
    c.mem.io_mut().is_some_and(|io| io.flash.used)
}

/// Applies the erases and page writes a flash chip was told to do.
fn flash_ops(c: &mut Ctx, st: &mut State) {
    let Some(io) = c.mem.io_mut() else { return };
    for op in std::mem::take(&mut io.flash.ops) {
        match op {
            FlashOp::Erase { offset, len } => st.save.write(offset, &vec![0xFF; len]),
            FlashOp::Program { offset } => st.save.write(offset, &st.flash_page.clone()),
        }
    }
}

/// Copies `len` bytes of cartridge space at PI address `dev` into RAM.
fn cart_copy(c: &mut Ctx, st: &mut State, dev: u32, ram: u32, len: u32) {
    let off = (dev & 0x0FFF_FFFF) as usize;
    let at = (ram & 0x1FFF_FFFF) as usize;
    let n = len as usize;
    let bytes = c.mem.bytes_mut();
    if at + n > bytes.len() {
        panic!("cartridge DMA to {ram:#010x}+{len:#x} runs past RAM");
    }
    for k in 0..n {
        bytes[at + k] = st.rom.get(off + k).copied().unwrap_or(0);
    }
    st.dma_log.push((off as u32, ram, len));
    c.overlay_loaded(off as u32, ram, len);
}

fn os_pi_start_dma(c: &mut Ctx) {
    let (mb, dir, dev, ram, len, mq) = (
        arg(c, 0),
        arg(c, 2),
        arg(c, 3),
        arg(c, 4),
        arg(c, 5),
        arg(c, 6),
    );
    // The request is filled in as the system library does before handing
    // it on (type, priority, return queue, RAM and cartridge address,
    // size): games match the message they get back by these fields.
    const OS_MESG_TYPE_DMAREAD: u16 = 11;
    c.mem
        .write_u16(mb, OS_MESG_TYPE_DMAREAD + u16::from(dir != 0));
    c.mem.write_u8(mb + 2, arg(c, 1) as u8);
    c.mem.write_u32(mb + 4, mq);
    c.mem.write_u32(mb + 8, ram);
    c.mem.write_u32(mb + 0x0C, dev);
    c.mem.write_u32(mb + 0x10, len);
    let mut st = lock();
    pi_dma(c, &mut st, dir, dev, ram, len);
    if mq != 0 {
        put_message(c, &mut st, mq, mb, false);
    }
    let _st = reschedule(c, st);
    ret(c, 0);
}

fn os_epi_start_dma(c: &mut Ctx) {
    let (handle, mb, dir) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let base = c.mem.read_u32(handle + 0x0C);
    let mq = c.mem.read_u32(mb + 4);
    let ram = c.mem.read_u32(mb + 8);
    let dev = c.mem.read_u32(mb + 0x0C);
    let len = c.mem.read_u32(mb + 0x10);
    let mut st = lock();
    // The system library ORs the handle base with the device address: a
    // handle may give the full address in either.
    pi_dma(c, &mut st, dir, base | dev, ram, len);
    if mq != 0 {
        put_message(c, &mut st, mq, mb, false);
    }
    let _st = reschedule(c, st);
    ret(c, 0);
}

fn os_pi_raw_start_dma(c: &mut Ctx) {
    let (dir, dev, ram, len) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let mut st = lock();
    pi_dma(c, &mut st, dir, dev, ram, len);
    drop(st);
    ret(c, 0);
}

/// osEPiRawStartDma(handle, direction, devAddr, dramAddr, size): as
/// osPiRawStartDma, with the handle's base address ORed in (see
/// `os_epi_start_dma`).
fn os_epi_raw_start_dma(c: &mut Ctx) {
    let (handle, dir, dev, ram) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let len = arg(c, 4);
    let base = c.mem.read_u32(handle + 0x0C);
    let mut st = lock();
    pi_dma(c, &mut st, dir, base | dev, ram, len);
    drop(st);
    ret(c, 0);
}

fn os_pi_read_io(c: &mut Ctx) {
    let (dev, out) = (arg(c, 0), arg(c, 1));
    let off = (dev & 0x0FFF_FFFC) as usize;
    let st = lock();
    let w = st
        .rom
        .get(off..off + 4)
        .map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()));
    drop(st);
    c.mem.write_u32(out, w);
    ret(c, 0);
}

fn os_cart_rom_init(c: &mut Ctx) {
    // OSPiHandle: type 0 (cartridge), base address 0x1000_0000.
    for k in 0..0x74 / 4 {
        c.mem.write_u32(CART_HANDLE + 4 * k, 0);
    }
    c.mem.write_u32(CART_HANDLE + 0x0C, 0x1000_0000);
    ret(c, CART_HANDLE);
}

fn write_pads(c: &mut Ctx, at: u32, pads: &[Pad; 4]) {
    for (k, p) in pads.iter().enumerate() {
        let a = at + 6 * k as u32;
        c.mem.write_u16(a, p.buttons);
        c.mem.write_u8(a + 2, p.stick_x as u8);
        c.mem.write_u8(a + 3, p.stick_y as u8);
        c.mem.write_u8(a + 4, if p.connected { 0 } else { 8 });
    }
}

fn write_status(c: &mut Ctx, at: u32, pads: &[Pad; 4]) {
    for (k, p) in pads.iter().enumerate() {
        let a = at + 4 * k as u32;
        c.mem.write_u16(a, if p.connected { 0x0005 } else { 0 });
        // Pak status: a Rumble Pak when the game's config asks for one, as
        // the controller itself answers (see `run_pif`).
        c.mem.write_u8(
            a + 2,
            u8::from(p.connected && RUMBLE.load(Ordering::Relaxed)),
        );
        c.mem.write_u8(a + 3, if p.connected { 0 } else { 8 });
    }
}

fn os_cont_init(c: &mut Ctx) {
    let (bits, status) = (arg(c, 1), arg(c, 2));
    let pads = lock().devices.pads();
    let mask = pads
        .iter()
        .enumerate()
        .filter(|(_, p)| p.connected)
        .fold(0u8, |m, (k, _)| m | 1 << k);
    c.mem.write_u8(bits, mask);
    write_status(c, status, &pads);
    ret(c, 0);
}

/// Starts a controller transfer: completes at once and posts to `mq`.
fn os_cont_start(c: &mut Ctx) {
    let mq = arg(c, 0);
    let mut st = lock();
    let msg = st
        .events
        .get(&event::SI)
        .filter(|(q, _)| *q == mq)
        .map_or(0, |&(_, m)| m);
    put_message(c, &mut st, mq, msg, false);
    let _st = reschedule(c, st);
    ret(c, 0);
}

fn os_cont_get_read_data(c: &mut Ctx) {
    let out = arg(c, 0);
    let pads = lock().devices.pads();
    write_pads(c, out, &pads);
}

/// __osSiRawStartDma(direction, buffer): moves the 64-byte command block
/// between RAM and PIF RAM, answering the commands in it on the way in, and
/// raises the serial interrupt as each transfer would.
fn os_si_raw_start_dma(c: &mut Ctx) {
    let (dir, buf) = (arg(c, 0), arg(c, 1));
    let mut st = lock();
    if dir == 1 {
        for (k, b) in st.pif.iter_mut().enumerate() {
            *b = c.mem.read_u8(buf + k as u32);
        }
        let pads = st.devices.pads();
        let st = &mut *st;
        run_pif(&mut st.pif, &pads, &mut st.save);
    } else {
        for (k, &b) in st.pif.iter().enumerate() {
            c.mem.write_u8(buf + k as u32, b);
        }
    }
    post_event(c, &mut st, event::SI);
    let _st = reschedule(c, st);
    ret(c, 0);
}

/// Answers the controller commands in a PIF command block: per channel a
/// send length, a reply length, the command and room for the reply. No
/// pak is plugged in (unless the game's config asks for a Rumble Pak): pak
/// reads and writes answer with a bad data CRC, as an empty slot does, and
/// the status says no pak. Channel 4 is the cartridge
/// EEPROM, when the game has one (see `Save::set_eeprom`).
fn run_pif(pif: &mut [u8; 64], pads: &[Pad; 4], save: &mut Save) {
    const NO_REPLY: u8 = 0x80;
    let (mut i, mut ch) = (0, 0);
    while i < 63 {
        match pif[i] {
            0xFE => break,
            0xFF | 0xFD => i += 1,
            0 => {
                ch += 1;
                i += 1;
            }
            t => {
                let (tx, rx) = ((t & 0x3F) as usize, (pif[i + 1] & 0x3F) as usize);
                let (cmd, at) = (i + 2, i + 2 + tx);
                if at + rx > 63 {
                    break;
                }
                let pad = pads.get(ch).filter(|p| p.connected);
                let eeprom = save.eeprom().len();
                let reply: Option<Vec<u8>> = if ch == 4 && eeprom > 0 {
                    // Status (16 Kbit or 4 Kbit), read or write one 8-byte block.
                    let block = usize::from(pif[cmd + 1]) * 8;
                    match pif[cmd] {
                        0x00 | 0xFF => {
                            Some(vec![0x00, if eeprom > 512 { 0xC0 } else { 0x80 }, 0x00])
                        }
                        0x04 if block + 8 <= eeprom => {
                            Some(save.eeprom()[block..block + 8].to_vec())
                        }
                        0x05 if block + 8 <= eeprom && tx >= 10 => {
                            let data = pif[cmd + 2..cmd + 10].to_vec();
                            save.eeprom_write(block, &data);
                            Some(vec![0x00])
                        }
                        _ => None,
                    }
                } else {
                    let rumble = RUMBLE.load(Ordering::Relaxed);
                    pad.and_then(|p| match pif[cmd] {
                        0x00 | 0xFF => Some(vec![0x05, 0x00, u8::from(rumble)]),
                        0x01 => Some(vec![
                            (p.buttons >> 8) as u8,
                            p.buttons as u8,
                            p.stick_x as u8,
                            p.stick_y as u8,
                        ]),
                        // A Rumble Pak answers 0x80 from its ID area
                        // (0x8000) and takes motor writes at 0xC000.
                        // shortcut: the motor itself is not passed to the
                        // host's controller.
                        0x02 if rumble && tx >= 3 => {
                            let addr = (u16::from(pif[cmd + 1]) << 8) & 0xF000;
                            let data = vec![if addr == 0x8000 { 0x80 } else { 0 }; 32];
                            let crc = pak_crc(&data);
                            Some([data, vec![crc]].concat())
                        }
                        0x03 if rumble && tx >= 35 => Some(vec![pak_crc(&pif[cmd + 3..cmd + 35])]),
                        // No pak: the check byte is the complement of the
                        // data's.
                        0x02 => Some([vec![0; 32], vec![0xFF]].concat()),
                        0x03 => Some(vec![0xFF]),
                        _ => None,
                    })
                };
                match reply {
                    Some(r) => {
                        let n = r.len().min(rx);
                        pif[at..at + n].copy_from_slice(&r[..n]);
                    }
                    None => pif[i + 1] |= NO_REPLY,
                }
                i = at + rx;
                ch += 1;
            }
        }
    }
    pif[63] = 0;
}

/// The check byte a controller pak sends after 32 bytes of data (CRC-8,
/// polynomial 0x85, as the system library works it out).
fn pak_crc(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for i in 0..=data.len() {
        for bit in (0..8).rev() {
            let tap = if crc & 0x80 != 0 { 0x85 } else { 0 };
            crc <<= 1;
            if data.get(i).is_some_and(|b| b >> bit & 1 != 0) {
                crc |= 1;
            }
            crc ^= tap;
        }
    }
    crc
}

fn os_cont_get_query(c: &mut Ctx) {
    let out = arg(c, 0);
    let pads = lock().devices.pads();
    write_status(c, out, &pads);
}

fn os_sp_task_start_go(c: &mut Ctx) {
    let task = arg(c, 0);
    let kind = c.mem.read_u32(task);
    let mut st = lock();
    st.devices.rsp_task(c, task);
    // The task is done at once here, but it reports done a little later,
    // as on the console: games send requests about a running task (which
    // frame buffer it fills) after starting it.
    // shortcut: one fixed time for every task, not its real length.
    let at = c.clock + TASK_TICKS;
    if kind == 1 {
        post_event_at(c, &mut st, event::SP, at, 1);
        // Graphics tasks also finish on the RDP.
        post_event_at(c, &mut st, event::DP, at + 1, 2);
    } else {
        post_event_at(c, &mut st, event::SP, at, 3);
    }
    let _st = reschedule(c, st);
}

/// How long an RSP task takes to report done, in COUNT ticks (1 ms).
const TASK_TICKS: u64 = COUNT_HZ / 1000;

/// Posts the message registered for event `e` at clock `at`, as a timer
/// under `key`: a small number no guest timer has (theirs are addresses).
fn post_event_at(c: &mut Ctx, st: &mut State, e: u32, at: u64, key: u32) {
    if let Some(&(mq, msg)) = st.events.get(&e) {
        st.timers.insert(
            key,
            Timer {
                at,
                interval: 0,
                mq,
                msg,
            },
        );
        arm(c, st);
    }
}

fn os_ai_set_next_buffer(c: &mut Ctx) {
    let (addr, len) = (arg(c, 0), arg(c, 1));
    let mut st = lock();
    st.ai.push(c.clock, len);
    ai_regs(c, &mut st);
    st.devices.audio(c, addr, len);
    ret(c, 0);
}

fn os_ai_get_length(c: &mut Ctx) {
    let n = lock().ai.length(c.clock);
    ret(c, n);
}

fn os_ai_get_status(c: &mut Ctx) {
    let n = lock().ai.status(c.clock);
    ret(c, n);
}

fn os_ai_set_frequency(c: &mut Ctx) {
    let f = arg(c, 0);
    let mut st = lock();
    st.ai.rate = u64::from(f);
    st.devices.audio_rate(f);
    ret(c, f);
}

/// A debug print routine (`printf`-style: format string, then arguments
/// the usual way): the text goes to the console, as a development kit
/// would show it.
fn debug_print(c: &mut Ctx) {
    let text = format_guest(c, arg(c, 0), 1);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        eprintln!("game: {line}");
    }
}

/// The NUL-terminated string at `at` (at most 1 KiB).
fn guest_str(c: &Ctx, at: u32) -> String {
    (0..1024)
        .map(|i| c.mem.read_u8(at.wrapping_add(i)))
        .take_while(|&b| b != 0)
        .map(char::from)
        .collect()
}

/// Formats the guest `printf` format string at `fmt` with the arguments
/// from argument slot `first` on: %d %i %u %x %X %o %c %s %p %f %e %g,
/// with flags, width and precision as written.
fn format_guest(c: &Ctx, fmt: u32, first: usize) -> String {
    let f = guest_str(c, fmt);
    let mut out = String::new();
    let mut k = first;
    let mut chars = f.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            chars.next();
            spec.push(n);
            if n.is_ascii_alphabetic() && n != 'l' && n != 'h' || n == '%' {
                break;
            }
        }
        let conv = spec.pop().unwrap_or('%');
        let width: usize = spec
            .trim_start_matches(['-', '0', '+', ' ', '#'])
            .split('.')
            .next()
            .and_then(|w| w.trim_end_matches(['l', 'h']).parse().ok())
            .unwrap_or(0);
        let zero = spec.starts_with('0');
        let left = spec.starts_with('-');
        let prec: Option<usize> = spec
            .split_once('.')
            .and_then(|(_, p)| p.trim_end_matches(['l', 'h']).parse().ok());
        let mut next = || {
            k += 1;
            arg(c, k - 1)
        };
        let body = match conv {
            '%' => "%".to_string(),
            'd' | 'i' => (next() as i32).to_string(),
            'u' => next().to_string(),
            'x' => format!("{:x}", next()),
            'X' => format!("{:X}", next()),
            'o' => format!("{:o}", next()),
            'p' => format!("{:08x}", next()),
            'c' => char::from(next() as u8).to_string(),
            's' => {
                let t = guest_str(c, next());
                match prec {
                    Some(p) => t.chars().take(p).collect(),
                    None => t,
                }
            }
            'f' | 'e' | 'g' | 'F' | 'E' | 'G' => {
                // Doubles take an even pair of argument slots.
                k += k % 2;
                let hi = u64::from(arg(c, k));
                let lo = u64::from(arg(c, k + 1));
                k += 2;
                let v = f64::from_bits(hi << 32 | lo);
                c_float(v, conv, prec.unwrap_or(6))
            }
            other => format!("%{spec}{other}"),
        };
        let pad = width.saturating_sub(body.chars().count());
        if left {
            out.push_str(&body);
            out.extend(std::iter::repeat_n(' ', pad));
        } else if zero && conv != 's' && conv != 'c' {
            // Zeros go after the sign.
            let digits = body.strip_prefix('-').map_or(&body[..], |d| {
                out.push('-');
                d
            });
            out.extend(std::iter::repeat_n('0', pad));
            out.push_str(digits);
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(&body);
        }
    }
    out
}

/// `v` as C's `printf` writes it with conversion `conv` (f e g F E G) and
/// precision `prec`.
fn c_float(v: f64, conv: char, prec: usize) -> String {
    // C writes the exponent with a sign and at least two digits.
    let exp = |p: usize| {
        let t = format!("{v:.p$e}");
        let (m, e) = t.split_once('e').unwrap_or((&t, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    };
    let t = match conv.to_ascii_lowercase() {
        'e' => exp(prec),
        'g' => {
            let p = prec.max(1);
            let x = if v == 0.0 {
                0
            } else {
                format!("{v:.*e}", p - 1)
                    .split_once('e')
                    .and_then(|(_, e)| e.parse::<i32>().ok())
                    .unwrap_or(0)
            };
            // Trailing zeros (and a bare point) are dropped.
            let trim = |t: String| {
                if t.contains('.') {
                    t.trim_end_matches('0').trim_end_matches('.').to_string()
                } else {
                    t
                }
            };
            if x < -4 || x >= p as i32 {
                let t = exp(p - 1);
                let (m, e) = t.split_once('e').unwrap_or((&t, ""));
                format!("{}e{e}", trim(m.to_string()))
            } else {
                trim(format!("{v:.*}", (p as i32 - 1 - x).max(0) as usize))
            }
        }
        _ => format!("{v:.prec$}"),
    };
    if conv.is_ascii_uppercase() {
        t.to_uppercase()
    } else {
        t
    }
}

fn ret_zero(c: &mut Ctx) {
    ret(c, 0);
}

fn nothing(_: &mut Ctx) {}

/// New code is in place: functions of code the game wrote itself are
/// checked against RAM again at their next call.
fn os_inval_icache(c: &mut Ctx) {
    let (ram, len) = (arg(c, 0), arg(c, 1));
    c.code_written(ram, len);
}

/// The RCP interrupt lines (MI mask, bits 16 to 21 of an `OSIntMask`) as
/// the game last set them; the CPU lines live in each thread's Status.
static RCP_MASK: AtomicU32 = AtomicU32::new(0x3F);

/// Whether `os_set_int_mask` masks (a game asks with `int_mask` in its
/// config); otherwise it reports every line enabled and masks nothing.
static INT_MASK: AtomicBool = AtomicBool::new(false);

/// Whether each controller reports a Rumble Pak (a game asks with `rumble`
/// in its config; others see no pak, as before).
static RUMBLE: AtomicBool = AtomicBool::new(false);

/// osSetIntMask: the CPU interrupt lines go into the running thread's
/// Status (bits 8 to 15), as on the console, so a game that masks them
/// around a list walk is not interrupted there (see `interrupt`). Returns
/// the mask before.
fn os_set_int_mask(c: &mut Ctx) {
    if !INT_MASK.load(Ordering::Relaxed) {
        ret(c, 0x003F_FF01);
        return;
    }
    let new = arg(c, 0);
    let status = c.cop0[12];
    let old = 1 | (status & 0xFF00) | (RCP_MASK.load(Ordering::Relaxed) << 16);
    RCP_MASK.store((new >> 16) & 0x3F, Ordering::Relaxed);
    c.cop0[12] = (status & !0xFF00) | (new & 0xFF00);
    ret(c, old);
}

/// System library routines replaced by the host, by their usual names.
pub const HOOKS: &[(&str, GuestFn)] = &[
    ("osInitialize", nothing),
    // Debug text: the system library's own, or a game's (named
    // `debug_print` in the per-game config).
    ("osSyncPrintf", debug_print),
    ("debug_print", debug_print),
    ("osCreateThread", os_create_thread),
    ("osStartThread", os_start_thread),
    ("osStopThread", os_stop_thread),
    ("osDestroyThread", os_destroy_thread),
    ("osSetThreadPri", os_set_thread_pri),
    ("osGetThreadPri", os_get_thread_pri),
    ("osGetThreadId", os_get_thread_id),
    ("osYieldThread", os_yield_thread),
    ("osCreateMesgQueue", os_create_mesg_queue),
    ("osSendMesg", os_send_mesg),
    ("osJamMesg", os_jam_mesg),
    ("osRecvMesg", os_recv_mesg),
    ("osSetEventMesg", os_set_event_mesg),
    ("osGetTime", os_get_time),
    ("osSetTime", os_set_time),
    ("osGetCount", os_get_count),
    ("osSetTimer", os_set_timer),
    ("osStopTimer", os_stop_timer),
    ("osSetIntMask", os_set_int_mask),
    ("osCreateViManager", nothing),
    ("osViSetMode", nothing),
    ("osViSetEvent", os_vi_set_event),
    ("osViBlack", nothing),
    ("osViSetSpecialFeatures", nothing),
    ("osViSetXScale", nothing),
    ("osViSetYScale", nothing),
    ("osViRepeatLine", nothing),
    ("osViSwapBuffer", os_vi_swap_buffer),
    ("osViGetCurrentFramebuffer", os_vi_get_framebuffer),
    ("osViGetNextFramebuffer", os_vi_get_framebuffer),
    ("osCreatePiManager", nothing),
    ("osPiStartDma", os_pi_start_dma),
    ("osEPiStartDma", os_epi_start_dma),
    ("osPiRawStartDma", os_pi_raw_start_dma),
    ("osEPiRawStartDma", os_epi_raw_start_dma),
    ("osPiReadIo", os_pi_read_io),
    ("osPiGetStatus", ret_zero),
    ("osCartRomInit", os_cart_rom_init),
    ("osContInit", os_cont_init),
    ("osContStartReadData", os_cont_start),
    ("osContStartQuery", os_cont_start),
    ("osContGetReadData", os_cont_get_read_data),
    ("osContGetQuery", os_cont_get_query),
    ("__osSiRawStartDma", os_si_raw_start_dma),
    ("osSpTaskLoad", nothing),
    ("osSpTaskStartGo", os_sp_task_start_go),
    ("osSpTaskYield", nothing),
    ("osSpTaskYielded", ret_zero),
    ("osDpSetStatus", nothing),
    ("osDpGetStatus", ret_zero),
    ("osSpGetStatus", ret_zero),
    ("osAiSetFrequency", os_ai_set_frequency),
    ("osAiSetNextBuffer", os_ai_set_next_buffer),
    ("osAiGetLength", os_ai_get_length),
    ("osAiGetStatus", os_ai_get_status),
    ("osInvalDCache", nothing),
    ("osInvalICache", os_inval_icache),
    ("osWritebackDCache", nothing),
    ("osWritebackDCacheAll", nothing),
    ("osMapTLB", nothing),
    ("osUnmapTLBAll", nothing),
];

/// Config name of a game's own TLB refill routine: not replaced, but run
/// for addresses no TLB page maps (see `Ctx::tlb_miss`).
pub const TLB_REFILL: &str = "tlb_refill";

/// Installs the replacements over the routines found in `addrs`
/// (role name and guest address). A name may come more than once: a game can
/// link a library routine into two segments. Returns the names installed.
pub fn install<'a>(
    ctx: &mut Ctx,
    addrs: impl IntoIterator<Item = (&'a String, &'a u32)>,
) -> Vec<String> {
    let mut done = Vec::new();
    THREAD_STATUS.store(0, Ordering::Relaxed);
    INT_MASK.store(false, Ordering::Relaxed);
    RUMBLE.store(false, Ordering::Relaxed);
    RCP_MASK.store(0x3F, Ordering::Relaxed);
    for (name, &a) in addrs {
        if let Some((_, f)) = HOOKS.iter().find(|(h, _)| h == name) {
            ctx.set_override(a, *f);
            done.push(name.clone());
        } else if name == TLB_REFILL {
            ctx.tlb_refill = a;
            done.push(name.clone());
        } else if name == "thread_status" {
            THREAD_STATUS.store(a, Ordering::Relaxed);
            done.push(name.clone());
        } else if name == "int_mask" {
            INT_MASK.store(a != 0, Ordering::Relaxed);
            done.push(name.clone());
        } else if name == "rumble" {
            RUMBLE.store(a != 0, Ordering::Relaxed);
            done.push(name.clone());
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_buffers_play_out_on_the_guest_clock() {
        let mut ai = Ai {
            rate: 32000,
            ..Ai::default()
        };
        // Two frames of sound (533 samples, 4 bytes each) queued at once.
        ai.push(0, 2132);
        ai.push(0, 2132);
        assert_eq!(ai.status(0), 3 << 30);
        assert_eq!(ai.length(0), 2128);
        // Halfway through the first buffer.
        let half = 533 / 2 * COUNT_HZ / 32000;
        assert!(ai.length(half).abs_diff(1064) <= 8);
        // The first has played; the second is playing.
        let one = 533 * COUNT_HZ / 32000 + 1;
        assert_eq!(ai.status(one), 1 << 30);
        assert!(ai.length(one) >= 2120);
        // All played: empty and idle.
        assert_eq!((ai.length(3 * one), ai.status(3 * one)), (0, 0));
    }

    #[test]
    fn the_interrupt_mask_goes_into_status_and_holds_back_interrupts() {
        let mut c = Ctx::new(n64rusty_runtime::Memory::new(0x1000));
        c.cop0[12] = STATUS_THREAD;
        // Without the config name nothing is masked.
        c.r[4] = 1;
        os_set_int_mask(&mut c);
        assert_eq!(c.cop0[12], STATUS_THREAD);
        INT_MASK.store(true, Ordering::Relaxed);
        c.r[4] = 1; // OS_IM_NONE
        os_set_int_mask(&mut c);
        assert_eq!(c.r[2] as u32, 0x003F_FF01, "the mask before");
        assert_eq!(c.cop0[12] & 0xFF00, 0);
        // Masked: an interrupt due now waits for a later call.
        c.clock = 100;
        interrupt(&mut c);
        assert_eq!(c.deadline, 100);
        // Restoring the old mask brings the lines back.
        c.r[4] = 0x003F_FF01;
        os_set_int_mask(&mut c);
        assert_eq!(c.cop0[12], STATUS_THREAD);
        assert_eq!(c.r[2] as u32, 1);
        INT_MASK.store(false, Ordering::Relaxed);
    }

    #[test]
    fn channel_4_is_an_eeprom_only_when_the_game_has_one() {
        let pads = [Pad::default(); 4];
        // Four empty channels, then channel 4: status (1 out, 3 back).
        let status = |save: &mut Save| {
            let mut pif = [0u8; 64];
            pif[..10].copy_from_slice(&[0, 0, 0, 0, 1, 3, 0x00, 0xFF, 0xFF, 0xFF]);
            pif[10] = 0xFE;
            run_pif(&mut pif, &pads, save);
            pif
        };
        assert_eq!(status(&mut Save::in_memory())[5] & 0x80, 0x80, "no reply");
        let mut save = Save::in_memory();
        save.set_eeprom(0x800);
        assert_eq!(status(&mut save)[7..10], [0x00, 0xC0, 0x00]);
        // Write block 3 (10 out, 1 back), then read it (2 out, 8 back).
        let mut pif = [0u8; 64];
        pif[..4].copy_from_slice(&[0, 0, 0, 0]);
        pif[4..18].copy_from_slice(&[10, 1, 0x05, 3, 1, 2, 3, 4, 5, 6, 7, 8, 0xFF, 0xFE]);
        run_pif(&mut pif, &pads, &mut save);
        let mut pif = [0u8; 64];
        pif[..4].copy_from_slice(&[0, 0, 0, 0]);
        pif[4..8].copy_from_slice(&[2, 8, 0x04, 3]);
        pif[16] = 0xFE;
        run_pif(&mut pif, &pads, &mut save);
        assert_eq!(pif[8..16], [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn ram_hash_sees_one_changed_byte() {
        let mut ram = vec![0u8; 0x1001];
        let h = ram_hash(&ram);
        assert_eq!(h, ram_hash(&ram));
        ram[0x1000] = 1;
        assert_ne!(h, ram_hash(&ram), "a short last chunk counts");
    }

    #[test]
    fn poke_scripts_parse() {
        let p = parse_pokes("# warp\n100 0x8011AC80 bb\n200-210 80001000 1 1\n").unwrap();
        assert_eq!(
            p[0],
            Poke {
                from: 100,
                to: 100,
                addr: 0x8011_AC80,
                value: 0xBB,
                bytes: 4
            }
        );
        assert_eq!(
            p[1],
            Poke {
                from: 200,
                to: 210,
                addr: 0x8000_1000,
                value: 1,
                bytes: 1
            }
        );
        assert!(parse_pokes("100 8011AC80").is_err());
        assert!(parse_pokes("100 8011AC80 1 3").is_err());
    }

    #[test]
    fn formats_a_guest_debug_print() {
        let mut c = Ctx::new(n64rusty_runtime::Memory::new(0x1000));
        let put = |c: &mut Ctx, at: u32, s: &str| {
            for (i, b) in s.bytes().chain([0]).enumerate() {
                c.mem.write_u8(0x8000_0000 + at + i as u32, b);
            }
        };
        put(&mut c, 0x100, "%s at %08X: %d%% %-3c|%5.2f");
        put(&mut c, 0x200, "load");
        c.r[29] = 0x8000_0800;
        c.r[5] = 0x8000_0200; // %s
        c.r[6] = 0xAB; // %08X
        c.r[7] = (-5i32) as u32 as u64; // %d
        c.mem.write_u32(0x8000_0810, u32::from(b'z')); // %c, slot 4
        let bits = 2.5f64.to_bits(); // %f, slots 6 and 7
        c.mem.write_u32(0x8000_0818, (bits >> 32) as u32);
        c.mem.write_u32(0x8000_081C, bits as u32);
        assert_eq!(
            format_guest(&c, 0x8000_0100, 1),
            "load at 000000AB: -5% z  | 2.50"
        );
        put(&mut c, 0x100, "%05d|%.2s");
        c.r[5] = (-42i32) as u32 as u64;
        c.r[6] = 0x8000_0200;
        assert_eq!(format_guest(&c, 0x8000_0100, 1), "-0042|lo");
    }

    #[test]
    fn floats_print_as_c_does() {
        assert_eq!(c_float(1.5, 'e', 6), "1.500000e+00");
        assert_eq!(c_float(0.00012, 'E', 2), "1.20E-04");
        assert_eq!(c_float(2.5, 'g', 6), "2.5");
        assert_eq!(c_float(1234567.0, 'g', 6), "1.23457e+06");
        assert_eq!(c_float(0.0001, 'g', 6), "0.0001");
        assert_eq!(c_float(100.0, 'G', 6), "100");
        assert_eq!(c_float(-3.25, 'f', 1), "-3.2");
    }

    #[test]
    fn pak_check_byte() {
        assert_eq!(pak_crc(&[0; 32]), 0);
        // One set bit gives the polynomial shifted through the rest.
        let mut one = [0u8; 32];
        one[31] = 1;
        assert_eq!(pak_crc(&one), 0x85);
    }

    #[test]
    fn pif_answers_status_buttons_and_an_empty_pak() {
        let pad = Pad {
            connected: true,
            buttons: 0x1000,
            stick_x: 5,
            stick_y: -3,
        };
        let pads = [pad, Pad::default(), Pad::default(), Pad::default()];
        // Port 1: status (1 out, 3 back). Port 2: read buttons, unplugged.
        let mut pif = [0u8; 64];
        let block = [
            1, 3, 0x00, 0xFF, 0xFF, 0xFF, 1, 4, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE,
        ];
        pif[..block.len()].copy_from_slice(&block);
        run_pif(&mut pif, &pads, &mut Save::in_memory());
        assert_eq!(pif[3..6], [0x05, 0x00, 0x00]);
        assert_eq!(pif[7] & 0x80, 0x80);
        // Port 1: buttons.
        let mut pif = [0u8; 64];
        pif[..8].copy_from_slice(&[1, 4, 0x01, 0, 0, 0, 0, 0xFE]);
        run_pif(&mut pif, &pads, &mut Save::in_memory());
        assert_eq!(pif[3..7], [0x10, 0x00, 5, 0xFD]);
        // Port 1: pak read (3 out, 33 back) answers with a bad CRC.
        let mut pif = [0u8; 64];
        pif[..5].copy_from_slice(&[3, 33, 0x02, 0x00, 0x00]);
        pif[38] = 0xFE;
        run_pif(&mut pif, &pads, &mut Save::in_memory());
        assert_eq!(pif[37], 0xFF);
    }
}
