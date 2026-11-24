//! Scripted input: record and replay, anchored on frame numbers.
//!
//! A script is plain text, one change per line:
//!
//! ```text
//! # frame port buttons x y
//! 0 1 - 0 0
//! 120 1 START 0 0
//! 122 1 - 0 0
//! 300 2 A+Z 80 -12
//! ```
//!
//! From `frame` on, controller `port` (1 to 4) holds that state until a
//! later line changes it. Frames never go backwards. Blank lines and lines
//! starting with `#` are ignored, so scripts can be written by hand.

use crate::pad::{parse_buttons, ButtonNames, Pad, Pads};

/// One state change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    pub frame: u64,
    /// Port index, 0 to 3 (written 1 to 4 in the text).
    pub port: usize,
    pub pad: Pad,
}

/// Parses a script, naming the line of the first error.
pub fn parse(text: &str) -> Result<Vec<Event>, String> {
    let mut events: Vec<Event> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let event = parse_line(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        if events.last().is_some_and(|last| event.frame < last.frame) {
            return Err(format!("line {}: frame goes backwards", n + 1));
        }
        events.push(event);
    }
    Ok(events)
}

/// Parses `port buttons x y` (the part after the frame), as the debug port
/// also uses it.
pub fn parse_pad(fields: &[&str]) -> Result<(usize, Pad), String> {
    let [port, buttons, x, y] = fields else {
        return Err("expected `port buttons x y`".into());
    };
    let port: usize = port.parse().map_err(|_| format!("bad port `{port}`"))?;
    if !(1..=4).contains(&port) {
        return Err(format!("port {port} is not 1 to 4"));
    }
    let axis = |s: &str| {
        s.parse::<i8>()
            .map_err(|_| format!("bad stick value `{s}`"))
    };
    let pad = Pad {
        buttons: parse_buttons(buttons)?,
        x: axis(x)?,
        y: axis(y)?,
    };
    Ok((port - 1, pad))
}

fn parse_line(line: &str) -> Result<Event, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let (frame, rest) = fields.split_first().ok_or("empty line")?;
    let frame = frame.parse().map_err(|_| format!("bad frame `{frame}`"))?;
    let (port, pad) = parse_pad(rest)?;
    Ok(Event { frame, port, pad })
}

/// Plays a script back. Frames must be asked for in increasing order.
pub struct Player {
    events: Vec<Event>,
    next: usize,
    pads: Pads,
}

impl Player {
    pub fn new(events: Vec<Event>) -> Self {
        Player {
            events,
            next: 0,
            pads: Pads::default(),
        }
    }

    /// Controller states for `frame`.
    pub fn pads_at(&mut self, frame: u64) -> Pads {
        while let Some(e) = self.events.get(self.next).filter(|e| e.frame <= frame) {
            self.pads[e.port] = e.pad;
            self.next += 1;
        }
        self.pads
    }
}

/// Records the controller states of each frame, writing only changes.
#[derive(Default)]
pub struct Recorder {
    last: Option<Pads>,
    text: String,
}

impl Recorder {
    pub fn record(&mut self, frame: u64, pads: &Pads) {
        if self.text.is_empty() {
            self.text
                .push_str("# n64rusty input script: frame port buttons x y\n");
        }
        for (port, pad) in pads.iter().enumerate() {
            // The first frame writes every port, so a replay starts from a known state.
            if self.last.map_or(true, |last| last[port] != *pad) {
                let buttons = ButtonNames(pad.buttons);
                let line = format!("{frame} {} {buttons} {} {}\n", port + 1, pad.x, pad.y);
                self.text.push_str(&line);
            }
        }
        self.last = Some(*pads);
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::button;

    #[test]
    fn holds_state_until_changed() {
        let script = "# test\n0 1 - 0 0\n\n120 1 START 0 0\n122 1 - 0 0\n300 2 A+Z 80 -12\n";
        let mut p = Player::new(parse(script).unwrap());
        assert_eq!(p.pads_at(0), Pads::default());
        assert_eq!(p.pads_at(121)[0].buttons, button::START);
        assert_eq!(p.pads_at(122)[0].buttons, 0);
        let pads = p.pads_at(10_000);
        assert_eq!(
            pads[1],
            Pad {
                buttons: button::A | button::Z,
                x: 80,
                y: -12
            }
        );
    }

    #[test]
    fn record_then_replay_is_identical() {
        let frames: Vec<Pads> = (0..500u64)
            .map(|f| {
                let mut pads = Pads::default();
                pads[0].buttons = if f % 37 < 5 { button::A } else { 0 };
                pads[0].x = ((f / 10) % 160) as i8 - 80;
                pads[3].y = if f > 250 { -80 } else { 0 };
                pads
            })
            .collect();
        let mut rec = Recorder::default();
        for (f, pads) in frames.iter().enumerate() {
            rec.record(f as u64, pads);
        }
        let mut player = Player::new(parse(rec.text()).unwrap());
        for (f, pads) in frames.iter().enumerate() {
            assert_eq!(player.pads_at(f as u64), *pads, "frame {f}");
        }
    }

    #[test]
    fn rejects_bad_scripts() {
        for (bad, why) in [
            ("5 1 - 0 0\n4 1 - 0 0", "backwards"),
            ("0 5 - 0 0", "not 1 to 4"),
            ("0 0 - 0 0", "not 1 to 4"),
            ("0 1 X 0 0", "unknown button"),
            ("0 1 - 200 0", "bad stick"),
            ("0 1 -", "expected"),
            ("x 1 - 0 0", "bad frame"),
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(why), "{bad:?} gave {err}");
        }
    }
}
