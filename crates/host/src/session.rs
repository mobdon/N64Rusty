//! The frame loop shared by headless and windowed runs: picks each frame's
//! input (debug port, then replay script, then live devices), records it,
//! and hands it to the game.

use std::fs::File;
use std::io::{self, Write};
use std::mem;
use std::net::SocketAddr;
use std::sync::mpsc::{Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::window::Window;

use crate::config::RunConfig;
use crate::debug::DebugPort;
use crate::pad::{Pad, Pads};
use crate::script::{self, Player, Recorder};

/// The game side of a run.
pub trait Guest {
    /// Runs one video frame of the game with these controller states.
    fn frame(&mut self, frame: u64, pads: &Pads);

    /// Called once the window exists, so a renderer can draw into it, for
    /// example with `wgpu::Instance::create_surface(window)`. Never called
    /// in a headless run.
    fn attach_window(&mut self, _window: Arc<Window>) {}

    /// Draw the window again between game frames, at the `refresh` rate of
    /// the window config. Never called otherwise.
    fn redraw(&mut self) {}

    /// The window settings changed (or the window opened): picture shape,
    /// texture look, and `Some(game frame rate)` when pictures should blend
    /// between game frames because the window draws faster than that.
    fn view(&mut self, _window: &crate::config::WindowConfig, _blend: Option<f64>) {}
}

/// What [`Session::tick`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    Ran,
    /// Paused by the debug port.
    Waiting,
    /// The run is over: frame limit reached or `quit`.
    Done,
}

enum Mode {
    Running,
    Paused,
    Stepping(u64, Sender<String>),
}

pub struct Session<G> {
    pub guest: G,
    frame: u64,
    limit: Option<u64>,
    done: bool,
    mode: Mode,
    replay: Option<Player>,
    record: Option<(Recorder, File, usize)>,
    record_error: Option<io::Error>,
    debug: Option<DebugPort>,
    held: [Option<Pad>; 4],
}

impl<G: Guest> Session<G> {
    pub fn new(guest: G, cfg: &RunConfig) -> Result<Session<G>, String> {
        let replay = match &cfg.replay {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let events =
                    script::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                Some(Player::new(events))
            }
            None => None,
        };
        let record = match &cfg.record {
            Some(path) => {
                let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
                Some((Recorder::default(), file, 0))
            }
            None => None,
        };
        let debug = match cfg.debug_port {
            Some(addr) => {
                Some(DebugPort::bind(addr).map_err(|e| format!("debug port {addr}: {e}"))?)
            }
            None => None,
        };
        Ok(Session {
            guest,
            frame: 0,
            limit: cfg.frames,
            done: false,
            mode: if debug.is_some() {
                Mode::Paused
            } else {
                Mode::Running
            },
            replay,
            record,
            record_error: None,
            debug,
            held: [None; 4],
        })
    }

    /// The debug port's address, if this run has one.
    pub fn debug_addr(&self) -> Option<SocketAddr> {
        self.debug.as_ref().map(DebugPort::addr)
    }

    /// Frames run so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Runs one frame if one may run. `live` is the host devices' state;
    /// `block` lets a paused run wait for the debug port instead of
    /// returning [`Tick::Waiting`].
    pub fn tick(&mut self, live: &Pads, block: bool) -> Tick {
        while let Some((line, reply)) = self.next_command(block) {
            self.command(&line, reply);
        }
        if self.done || self.limit.is_some_and(|l| self.frame >= l) {
            self.end_step();
            return Tick::Done;
        }
        if matches!(self.mode, Mode::Paused) {
            return Tick::Waiting;
        }
        let pads = self.input(live);
        self.record(&pads);
        self.guest.frame(self.frame, &pads);
        self.frame += 1;
        if let Mode::Stepping(left, _) = &mut self.mode {
            *left -= 1;
            if *left == 0 {
                self.end_step();
            }
        }
        Tick::Ran
    }

    /// Ends the run and returns the game, reporting any failure to write
    /// the recording.
    pub fn finish(mut self) -> io::Result<G> {
        self.end_step();
        match self.record_error.take() {
            Some(e) => Err(e),
            None => Ok(self.guest),
        }
    }

    fn input(&mut self, live: &Pads) -> Pads {
        let mut pads = match &mut self.replay {
            Some(player) => player.pads_at(self.frame),
            None => *live,
        };
        for (pad, held) in pads.iter_mut().zip(self.held) {
            if let Some(held) = held {
                *pad = held;
            }
        }
        pads
    }

    // Written as it goes, so the input that led to a crash is on disk.
    fn record(&mut self, pads: &Pads) {
        let Some((rec, file, written)) = &mut self.record else {
            return;
        };
        rec.record(self.frame, pads);
        let new = &rec.text()[*written..];
        if !new.is_empty() && self.record_error.is_none() {
            if let Err(e) = file.write_all(new.as_bytes()) {
                self.record_error = Some(e);
            }
        }
        *written = rec.text().len();
    }

    fn next_command(&mut self, block: bool) -> Option<(String, Sender<String>)> {
        let port = self.debug.as_ref()?;
        let paused = matches!(self.mode, Mode::Paused);
        let (request, gone) = if block && paused && !self.done {
            match port.requests.recv() {
                Ok(r) => (Some(r), false),
                Err(_) => (None, true),
            }
        } else {
            match port.requests.try_recv() {
                Ok(r) => (Some(r), false),
                Err(e) => (None, e == TryRecvError::Disconnected),
            }
        };
        // Nothing can ever unpause the run now: end it rather than wait forever.
        if gone && paused {
            self.done = true;
        }
        request
    }

    fn end_step(&mut self) {
        if let Mode::Stepping(_, reply) = mem::replace(&mut self.mode, Mode::Paused) {
            let _ = reply.send(format!("ok {}", self.frame));
        }
    }

    fn command(&mut self, line: &str, reply: Sender<String>) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let answer = match fields.as_slice() {
            ["step"] => return self.step(1, reply),
            ["step", n] => match n.parse() {
                Ok(n) if n > 0 => return self.step(n, reply),
                _ => format!("err bad frame count `{n}`"),
            },
            ["frame"] => format!("ok {}", self.frame),
            ["pad", rest @ ..] => match script::parse_pad(rest) {
                Ok((port, pad)) => {
                    self.held[port] = Some(pad);
                    "ok".into()
                }
                Err(e) => format!("err {e}"),
            },
            ["release", port] => match port.parse::<usize>() {
                Ok(p @ 1..=4) => {
                    self.held[p - 1] = None;
                    "ok".into()
                }
                _ => format!("err port {port} is not 1 to 4"),
            },
            ["run"] => {
                self.end_step();
                self.mode = Mode::Running;
                "ok".into()
            }
            ["quit"] => {
                self.done = true;
                format!("ok {}", self.frame)
            }
            _ => format!("err unknown command `{line}`"),
        };
        let _ = reply.send(answer);
    }

    fn step(&mut self, n: u64, reply: Sender<String>) {
        self.end_step();
        self.mode = Mode::Stepping(n, reply);
    }
}

/// Runs without a window and unpaced: input comes only from the replay
/// script and the debug port. Returns when the frame limit is reached or the
/// debug port sends `quit`; with neither, it runs until the game stops it.
pub fn run_headless<G: Guest>(guest: G, cfg: &RunConfig) -> Result<G, String> {
    let mut session = Session::new(guest, cfg)?;
    if let Some(addr) = session.debug_addr() {
        eprintln!("debug port listening on {addr}");
    }
    while session.tick(&Pads::default(), true) != Tick::Done {}
    session
        .finish()
        .map_err(|e| format!("writing the recording: {e}"))
}

/// Deadline-based frame pacing: frames start one period apart with no
/// drift. If the host falls a whole frame behind, the schedule restarts
/// from now, so the game slows down rather than ever running fast.
pub struct Pacer {
    period: Duration,
    next: Instant,
}

impl Pacer {
    pub fn new(rate: f64, now: Instant) -> Pacer {
        Pacer {
            period: Duration::from_secs_f64(1.0 / rate),
            next: now,
        }
    }

    /// When the next frame is due.
    pub fn next(&self) -> Instant {
        self.next
    }

    /// True when a frame is due at `now`; the deadline then moves on.
    pub fn due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = if now - self.next >= self.period {
            now + self.period
        } else {
            self.next + self.period
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_keeps_time_and_never_rushes() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut p = Pacer::new(100.0, t0); // 10 ms frames
        assert!(p.due(t0));
        assert!(!p.due(t0 + ms(9)));
        assert!(p.due(t0 + ms(13))); // a little late: schedule unchanged
        assert_eq!(p.next(), t0 + ms(20));
        assert!(p.due(t0 + ms(20)));
        assert!(p.due(t0 + ms(95))); // stalled: restart, no burst of catch-up frames
        assert!(!p.due(t0 + ms(100)));
        assert_eq!(p.next(), t0 + ms(105));
    }

    struct Count(u64);
    impl Guest for Count {
        fn frame(&mut self, _: u64, _: &Pads) {
            self.0 += 1;
        }
    }

    #[test]
    fn paused_run_ends_when_the_debug_port_is_gone() {
        let mut s = Session::new(Count(0), &RunConfig::default()).unwrap();
        let (tx, requests) = std::sync::mpsc::channel();
        drop(tx);
        s.debug = Some(DebugPort {
            addr: "127.0.0.1:1".parse().unwrap(),
            requests,
        });
        s.mode = Mode::Paused;
        assert_eq!(s.tick(&Pads::default(), true), Tick::Done);
        assert_eq!(s.finish().unwrap().0, 0);

        // The same without blocking, as the window polls it.
        let mut s = Session::new(Count(0), &RunConfig::default()).unwrap();
        let (tx, requests) = std::sync::mpsc::channel();
        drop(tx);
        s.debug = Some(DebugPort {
            addr: "127.0.0.1:1".parse().unwrap(),
            requests,
        });
        s.mode = Mode::Paused;
        assert_eq!(s.tick(&Pads::default(), false), Tick::Done);
    }
}
