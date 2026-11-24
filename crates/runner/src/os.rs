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
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use n64rusty_runtime::{sx32, Ctx, GuestFn};

use crate::devices::{Devices, Pad};

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
}

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
    /// Video retraces so far.
    pub frames: u64,
    frame_limit: u64,
    done: Option<Outcome>,
    rom: Arc<[u8]>,
    framebuffer: u32,
    devices: Box<dyn Devices>,
    /// Cartridge DMA requests seen: (ROM offset, RAM address, length).
    pub dma_log: Vec<(u32, u32, u32)>,
    /// Report text for a panic, filled by the panicking thread.
    report: String,
}

struct Os {
    state: Mutex<State>,
    cv: Condvar,
    ctx: AtomicPtr<Ctx>,
}

static OS: OnceLock<Os> = OnceLock::new();

/// Text of the last panic, set by the panic hook.
pub static PANIC_MESSAGE: Mutex<String> = Mutex::new(String::new());

fn os() -> &'static Os {
    OS.get().expect("system library state not set up")
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
            .map(|t| format!("  thread {:#010x} pri {} {:?}", t.id, t.pri, t.state))
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
        first < count && count <= 0x1_0000,
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

/// Delivers retraces and timers that are due by the current clock.
fn deliver_due(c: &mut Ctx, st: &mut State) {
    while c.clock >= st.next_vi {
        st.next_vi += TICKS_PER_FRAME;
        st.frames += 1;
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
    // Status.IE clear (inside __osDisableInt .. __osRestoreInt): try again
    // on a later call.
    if c.cop0[12] & 1 == 0 {
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
        os().cv.notify_all();
        if mine == Some(TState::Dead) {
            // A finished thread never runs guest code again.
            park(st);
        }
        st = os()
            .cv
            .wait_while(st, |s| s.current != Some(me) || s.done.is_some())
            .unwrap_or_else(|e| e.into_inner());
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
    loop {
        st = os().cv.wait(st).unwrap_or_else(|e| e.into_inner());
    }
}

fn spawn_thread(id: u32, generation: u64) {
    std::thread::Builder::new()
        .name(format!("guest-{id:08x}"))
        .stack_size(256 << 20)
        .spawn(move || {
            GENERATION.with(|g| g.set(generation));
            let st = lock();
            let st = os()
                .cv
                .wait_while(st, |s| s.current != Some(id) || s.done.is_some())
                .unwrap_or_else(|e| e.into_inner());
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
            c.r[29] = sx32(sp);
            // The boot code runs as the boot ROM left it, with 64-bit FPU
            // registers (Status.FR set). Threads start from the context the
            // system library builds, whose Status has FR clear: doubles
            // live in even/odd register pairs.
            c.fr = id == 0;
            c.cop0[12] = if id == 0 { STATUS_BOOT } else { STATUS_THREAD };
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
) -> (Outcome, String, u64) {
    ctx.on_deadline = Some(interrupt);
    ctx.cop0[12] = STATUS_BOOT;
    ctx.deadline = TICKS_PER_FRAME;
    let ok = OS
        .set(Os {
            state: Mutex::new(State {
                threads: BTreeMap::new(),
                current: Some(0),
                seq: 0,
                events: HashMap::new(),
                vi: None,
                timers: BTreeMap::new(),
                next_vi: TICKS_PER_FRAME,
                frames: 0,
                frame_limit,
                done: None,
                rom,
                framebuffer: 0,
                devices,
                dma_log: Vec::new(),
                report: String::new(),
            }),
            cv: Condvar::new(),
            ctx: AtomicPtr::new(ctx as *mut Ctx),
        })
        .is_ok();
    assert!(ok, "run can only be called once per process");
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
        },
    );
    spawn_thread(0, 0);
    os().cv.notify_all();
    let st = lock();
    let mut st = os()
        .cv
        .wait_while(st, |s| s.done.is_none() && !boot_returned(s))
        .unwrap_or_else(|e| e.into_inner());
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
    let t = c.clock;
    ret64(c, t);
}

fn os_set_time(c: &mut Ctx) {
    c.clock = arg64(c, 0);
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
    let mut st = lock();
    if dir == 0 {
        cart_copy(c, &mut st, dev, ram, len);
    }
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
    if dir == 0 {
        cart_copy(c, &mut st, base.wrapping_add(dev), ram, len);
    }
    if mq != 0 {
        put_message(c, &mut st, mq, mb, false);
    }
    let _st = reschedule(c, st);
    ret(c, 0);
}

fn os_pi_raw_start_dma(c: &mut Ctx) {
    let (dir, dev, ram, len) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let mut st = lock();
    if dir == 0 {
        cart_copy(c, &mut st, dev, ram, len);
    }
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
        c.mem.write_u8(a + 2, 0);
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
    post_event(c, &mut st, event::SP);
    // Graphics tasks also finish on the RDP.
    if kind == 1 {
        post_event(c, &mut st, event::DP);
    }
    let _st = reschedule(c, st);
}

fn os_ai_set_next_buffer(c: &mut Ctx) {
    let (addr, len) = (arg(c, 0), arg(c, 1));
    lock().devices.audio(c, addr, len);
    ret(c, 0);
}

fn os_ai_set_frequency(c: &mut Ctx) {
    let f = arg(c, 0);
    lock().devices.audio_rate(f);
    ret(c, f);
}

fn ret_zero(c: &mut Ctx) {
    ret(c, 0);
}

fn nothing(_: &mut Ctx) {}

fn os_set_int_mask(c: &mut Ctx) {
    // Interrupts are not modelled; report all enabled.
    ret(c, 0x003F_FF01);
}

/// System library routines replaced by the host, by their usual names.
pub const HOOKS: &[(&str, GuestFn)] = &[
    ("osInitialize", nothing),
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
    ("osEPiRawStartDma", os_pi_raw_start_dma),
    ("osPiReadIo", os_pi_read_io),
    ("osPiGetStatus", ret_zero),
    ("osCartRomInit", os_cart_rom_init),
    ("osContInit", os_cont_init),
    ("osContStartReadData", os_cont_start),
    ("osContStartQuery", os_cont_start),
    ("osContGetReadData", os_cont_get_read_data),
    ("osContGetQuery", os_cont_get_query),
    ("osSpTaskLoad", nothing),
    ("osSpTaskStartGo", os_sp_task_start_go),
    ("osSpTaskYield", nothing),
    ("osSpTaskYielded", ret_zero),
    ("osDpSetStatus", nothing),
    ("osDpGetStatus", ret_zero),
    ("osSpGetStatus", ret_zero),
    ("osAiSetFrequency", os_ai_set_frequency),
    ("osAiSetNextBuffer", os_ai_set_next_buffer),
    ("osAiGetLength", ret_zero),
    ("osAiGetStatus", ret_zero),
    ("osInvalDCache", nothing),
    ("osInvalICache", nothing),
    ("osWritebackDCache", nothing),
    ("osWritebackDCacheAll", nothing),
    ("osMapTLB", nothing),
    ("osUnmapTLBAll", nothing),
];

/// Installs the replacements over the routines found in `addrs`
/// (role name to guest address). Returns the names installed.
pub fn install(ctx: &mut Ctx, addrs: &BTreeMap<String, u32>) -> Vec<String> {
    let mut done = Vec::new();
    for (name, f) in HOOKS {
        if let Some(&a) = addrs.get(*name) {
            ctx.set_override(a, *f);
            done.push(name.to_string());
        }
    }
    done
}
