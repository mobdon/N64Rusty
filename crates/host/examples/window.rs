//! Opens a window and prints controller changes: a manual check of the
//! keyboard and gamepad layout. Not run by tests.
//!
//! cargo run -p n64rusty-host --example window [run.toml]

use n64rusty_host::pad::ButtonNames;
use n64rusty_host::{run_windowed, Guest, Pads, RunConfig};

struct Print(Pads);

impl Guest for Print {
    fn frame(&mut self, frame: u64, pads: &Pads) {
        for (n, (new, old)) in pads.iter().zip(self.0).enumerate() {
            if *new != old {
                println!(
                    "{frame} {} {} {} {}",
                    n + 1,
                    ButtonNames(new.buttons),
                    new.x,
                    new.y
                );
            }
        }
        self.0 = *pads;
    }
}

fn main() -> Result<(), String> {
    let cfg = match std::env::args().nth(1) {
        Some(path) => RunConfig::load(path.as_ref())?,
        None => RunConfig::default(),
    };
    run_windowed(Print(Pads::default()), &cfg).map(|_| ())
}
