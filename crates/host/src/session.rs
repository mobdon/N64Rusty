//! A run, frame by frame: where each frame's input comes from, in order of
//! priority: the debug port, then an input script, then live input (keys and
//! gamepads, window runs only). Recording sees the final input.

use std::net::SocketAddr;

use crate::config::RunConfig;
use crate::debug::DebugPort;
use crate::pad::Pads;
use crate::script::{self, Player, Recorder};

pub struct Session {
    frame: u64,
    limit: Option<u64>,
    controllers: usize,
    player: Option<Player>,
    recorder: Option<Recorder>,
    debug: Option<DebugPort>,
    record_to: Option<std::path::PathBuf>,
}

impl Session {
    pub fn new(cfg: &RunConfig) -> Result<Session, String> {
        let player = match &cfg.replay {
            Some(p) => {
                let text =
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                Some(Player::new(
                    script::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?,
                ))
            }
            None => None,
        };
        let debug = match cfg.debug_port {
            Some(port) => {
                Some(DebugPort::bind(port).map_err(|e| format!("debug port {port}: {e}"))?)
            }
            None => None,
        };
        Ok(Session {
            frame: 0,
            limit: cfg.frames,
            controllers: cfg.controllers as usize,
            player,
            recorder: cfg.record.as_ref().map(|_| Recorder::default()),
            debug,
            record_to: cfg.record.clone(),
        })
    }

    /// Where the debug port listens, if there is one.
    pub fn debug_addr(&self) -> Option<SocketAddr> {
        self.debug.as_ref().and_then(|d| d.local_addr().ok())
    }

    /// Frames run so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Input for the next frame, or `None` when the run is over. `live` is
    /// what the keys and gamepads say right now.
    pub fn next_frame(&mut self, live: Pads) -> Result<Option<Pads>, String> {
        if self.limit.is_some_and(|l| self.frame >= l) {
            return Ok(None);
        }
        if let Some(d) = &mut self.debug {
            if !d
                .before_frame(self.frame)
                .map_err(|e| format!("debug port: {e}"))?
            {
                return Ok(None);
            }
        }
        let mut pads = match &mut self.player {
            Some(p) => p.pads(self.frame),
            None => live,
        };
        if let Some(d) = &self.debug {
            d.apply(&mut pads);
        }
        for pad in &mut pads[self.controllers..] {
            *pad = Default::default();
        }
        if let Some(r) = &mut self.recorder {
            r.record(self.frame, &pads);
        }
        self.frame += 1;
        Ok(Some(pads))
    }

    /// End the run: save the recording and answer the debug client.
    pub fn finish(mut self) -> Result<u64, String> {
        if let Some(d) = &mut self.debug {
            d.finish(self.frame)
                .map_err(|e| format!("debug port: {e}"))?;
        }
        if let (Some(r), Some(path)) = (&self.recorder, &self.record_to) {
            std::fs::write(path, r.text()).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(self.frame)
    }
}

/// Run with no window and no live input. `game` runs once per frame with
/// that frame's number and input. Needs a frame limit or a debug port so it
/// cannot run forever. Returns the number of frames run.
pub fn run_headless(cfg: &RunConfig, game: impl FnMut(u64, &Pads)) -> Result<u64, String> {
    run_session(Session::new(cfg)?, game)
}

/// [`run_headless`] with a session made beforehand, for example to read its
/// debug address first.
pub fn run_session(mut s: Session, mut game: impl FnMut(u64, &Pads)) -> Result<u64, String> {
    if s.limit.is_none() && s.debug.is_none() {
        return Err("a headless run needs `frames` or `debug_port`".into());
    }
    while let Some(pads) = s.next_frame(Pads::default())? {
        game(s.frame() - 1, &pads);
    }
    s.finish()
}
