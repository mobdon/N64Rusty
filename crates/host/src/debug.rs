//! Debug port: a test drives the game one frame at a time over TCP.
//!
//! Listens on 127.0.0.1 only. The run waits for one client, then waits for a
//! command before every frame. One command per line, one reply line each:
//!
//! - `step [n]`: run n frames (default 1); replies `ok <frame>` once they ran.
//! - `pad <port> <buttons> <x> <y>`: hold this input on a port (same fields
//!   as an input script) from the next frame on, over any script or keys.
//! - `release <port>`: stop holding.
//! - `frame`: replies `ok <frame>`, the number of frames run so far.
//! - `quit`: ends the run. So does closing the connection.
//!
//! Anything else replies `err <reason>`.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use crate::pad::{parse_buttons, Pad, Pads};

pub struct DebugPort {
    listener: TcpListener,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    steps_left: u64,
    owe_reply: bool,
    held: [Option<Pad>; 4],
}

impl DebugPort {
    pub fn bind(port: u16) -> io::Result<DebugPort> {
        Ok(DebugPort {
            listener: TcpListener::bind(("127.0.0.1", port))?,
            conn: None,
            steps_left: 0,
            owe_reply: false,
            held: [None; 4],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Called before frame number `frame` (frames run so far). Waits until
    /// the client lets it run; `false` means stop.
    pub fn before_frame(&mut self, frame: u64) -> io::Result<bool> {
        if self.steps_left > 0 {
            self.steps_left -= 1;
            return Ok(true);
        }
        let (reader, writer) = match &mut self.conn {
            Some(c) => c,
            None => {
                let (s, _) = self.listener.accept()?;
                s.set_nodelay(true)?;
                self.conn.insert((BufReader::new(s.try_clone()?), s))
            }
        };
        if std::mem::take(&mut self.owe_reply) {
            writeln!(writer, "ok {frame}")?;
        }
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(false);
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let reply = match f[..] {
                ["step"] | ["step", _] => match f.get(1).map_or(Ok(1), |n| n.parse::<u64>()) {
                    Ok(0) => format!("ok {frame}"),
                    Ok(n) => {
                        self.steps_left = n - 1;
                        self.owe_reply = true;
                        return Ok(true);
                    }
                    Err(_) => "err bad count".into(),
                },
                ["pad", port, buttons, x, y] => match (
                    port_index(port),
                    parse_buttons(buttons),
                    x.parse(),
                    y.parse(),
                ) {
                    (Some(p), Ok(buttons), Ok(x), Ok(y)) => {
                        self.held[p] = Some(Pad { buttons, x, y });
                        "ok".into()
                    }
                    _ => "err usage: pad <port 1-4> <buttons> <x> <y>".into(),
                },
                ["release", port] => match port_index(port) {
                    Some(p) => {
                        self.held[p] = None;
                        "ok".into()
                    }
                    None => "err port must be 1 to 4".into(),
                },
                ["frame"] => format!("ok {frame}"),
                ["quit"] => {
                    writeln!(writer, "ok {frame}")?;
                    return Ok(false);
                }
                _ => format!("err unknown command `{}`", line.trim()),
            };
            writeln!(writer, "{reply}")?;
        }
    }

    /// Replace the ports the client is holding.
    pub fn apply(&self, pads: &mut Pads) {
        for (pad, held) in pads.iter_mut().zip(self.held) {
            if let Some(h) = held {
                *pad = h;
            }
        }
    }

    /// The run ended: answer a `step` that was still running.
    pub fn finish(&mut self, frame: u64) -> io::Result<()> {
        if let (true, Some((_, w))) = (self.owe_reply, &mut self.conn) {
            writeln!(w, "ok {frame}")?;
        }
        Ok(())
    }
}

fn port_index(s: &str) -> Option<usize> {
    s.parse::<usize>()
        .ok()
        .filter(|p| (1..=4).contains(p))
        .map(|p| p - 1)
}
