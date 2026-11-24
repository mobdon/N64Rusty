//! A small text debug port so a test or tool can drive a run one frame at
//! a time over TCP (loopback only; see [`crate::config`]).
//!
//! One command per line, one reply line each:
//!
//! | Command | Reply | Effect |
//! |---|---|---|
//! | `step [n]` | `ok <frame>` | run `n` frames (default 1), reply when done |
//! | `frame` | `ok <frame>` | number of frames run so far |
//! | `pad <port> <buttons> <x> <y>` | `ok` | hold this state on a port, over any other input |
//! | `release <port>` | `ok` | give the port back to its usual input |
//! | `run` | `ok` | run freely (paced, in a window) |
//! | `quit` | `ok <frame>` | end the run |
//!
//! Anything else replies `err <reason>`. A run with a debug port starts
//! paused, waiting for the first `step` or `run`.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

/// A command line and where its reply goes.
pub type Request = (String, Sender<String>);

pub struct DebugPort {
    addr: SocketAddr,
    pub(crate) requests: Receiver<Request>,
}

impl DebugPort {
    /// Listens on `addr`; port 0 picks a free port (see [`DebugPort::addr`]).
    pub fn bind(addr: SocketAddr) -> io::Result<DebugPort> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let (tx, requests) = channel();
        // shortcut: the accept thread outlives the run until the process exits; fine for one run per process.
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                thread::spawn(move || serve(stream, tx));
            }
        });
        Ok(DebugPort { addr, requests })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

fn serve(stream: TcpStream, tx: Sender<Request>) -> io::Result<()> {
    let mut out = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let (reply_tx, reply_rx) = channel();
        if tx.send((line?, reply_tx)).is_err() {
            break;
        }
        match reply_rx.recv() {
            Ok(reply) => writeln!(out, "{reply}")?,
            Err(_) => break, // the run ended
        }
    }
    Ok(())
}
