//! Input scripts: what each port does, anchored on frame numbers.
//!
//! One line per change, `<frame> <port> <buttons> <x> <y>`, for example
//! `120 1 A+START 0 80`. Ports are 1 to 4; buttons are names joined by `+`,
//! or `-` for none. A port keeps its state until its next line. `#` starts a
//! comment. Frames must not go backwards.

use crate::pad::{format_buttons, parse_buttons, Pad, Pads};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub frame: u64,
    /// 0-based.
    pub port: usize,
    pub pad: Pad,
}

pub fn parse(text: &str) -> Result<Vec<Event>, String> {
    let mut events: Vec<Event> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let err = |m: &str| format!("line {}: {m}: `{line}`", i + 1);
        let f: Vec<&str> = line.split_whitespace().collect();
        let [frame, port, buttons, x, y] = f[..] else {
            return Err(err("expected `<frame> <port> <buttons> <x> <y>`"));
        };
        let frame: u64 = frame.parse().map_err(|_| err("bad frame"))?;
        let port = match port.parse::<usize>() {
            Ok(p @ 1..=4) => p - 1,
            _ => return Err(err("port must be 1 to 4")),
        };
        let pad = Pad {
            buttons: parse_buttons(buttons).map_err(|e| err(&e))?,
            x: x.parse().map_err(|_| err("bad stick x"))?,
            y: y.parse().map_err(|_| err("bad stick y"))?,
        };
        if events.last().is_some_and(|e| e.frame > frame) {
            return Err(err("frames go backwards"));
        }
        events.push(Event { frame, port, pad });
    }
    Ok(events)
}

/// Plays a script back one frame at a time.
#[derive(Clone, Debug)]
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

    /// State for `frame`. Call with frames in increasing order.
    pub fn pads(&mut self, frame: u64) -> Pads {
        while let Some(e) = self.events.get(self.next).filter(|e| e.frame <= frame) {
            self.pads[e.port] = e.pad;
            self.next += 1;
        }
        self.pads
    }
}

/// Writes a script line for every port that changes, so a recording replays
/// exactly.
#[derive(Clone, Debug)]
pub struct Recorder {
    last: Pads,
    text: String,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            last: Pads::default(),
            text: "# frame port buttons x y\n".into(),
        }
    }
}

impl Recorder {
    pub fn record(&mut self, frame: u64, pads: &Pads) {
        for (port, (pad, last)) in pads.iter().zip(&mut self.last).enumerate() {
            if pad != last {
                let b = format_buttons(pad.buttons);
                self.text += &format!("{frame} {} {b} {} {}\n", port + 1, pad.x, pad.y);
                *last = *pad;
            }
        }
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
    fn parses_and_holds_state_until_the_next_line() {
        let ev = parse("# hello\n0 1 A 0 0\n\n10 2 - -80 80 # walk\n10 1 - 0 0\n").unwrap();
        let mut p = Player::new(ev);
        assert_eq!(p.pads(0)[0].buttons, button::A);
        assert_eq!(p.pads(9)[0].buttons, button::A);
        let at10 = p.pads(10);
        assert_eq!(at10[0], Pad::default());
        assert_eq!(
            at10[1],
            Pad {
                buttons: 0,
                x: -80,
                y: 80
            }
        );
    }

    #[test]
    fn rejects_bad_lines() {
        for bad in [
            "1 1 A 0",
            "1 0 A 0 0",
            "1 5 A 0 0",
            "1 1 Q 0 0",
            "1 1 A 200 0",
            "5 1 A 0 0\n4 1 - 0 0",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn recording_replays_exactly() {
        let mut frames = Vec::new();
        let mut rec = Recorder::default();
        for f in 0..300u64 {
            let mut pads = Pads::default();
            pads[(f % 4) as usize] = Pad {
                buttons: (f * 0x1357) as u16 & 0xFF3F,
                x: (f % 7) as i8 - 3,
                y: -((f % 5) as i8),
            };
            if f % 3 == 0 {
                pads = frames.last().copied().unwrap_or_default();
            }
            rec.record(f, &pads);
            frames.push(pads);
        }
        let mut p = Player::new(parse(rec.text()).unwrap());
        for (f, want) in frames.iter().enumerate() {
            assert_eq!(&p.pads(f as u64), want, "frame {f}");
        }
    }
}
