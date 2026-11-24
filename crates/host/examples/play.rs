//! Opens the game window with no game in it and shows each port's input in
//! the title bar, to try out key bindings and gamepads by hand.
//!
//! cargo run -p n64rusty-host --example play [run.toml]

use n64rusty_host::pad::format_buttons;
use n64rusty_host::window::run_window;
use n64rusty_host::RunConfig;

fn main() {
    let cfg = match std::env::args_os().nth(1) {
        Some(p) => RunConfig::load(p.as_ref()),
        None => Ok(RunConfig::default()),
    }
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let mut last = String::new();
    let result = run_window(&cfg, |frame, pads, window| {
        let p = &pads[..cfg.controllers as usize];
        let title: Vec<String> = p
            .iter()
            .enumerate()
            .map(|(i, p)| format!("P{} {} {},{}", i + 1, format_buttons(p.buttons), p.x, p.y))
            .collect();
        let title = title.join("  |  ");
        if title != last {
            window.set_title(&title);
            last = title;
        }
        let _ = frame;
    });
    match result {
        Ok(frames) => println!("{frames} frames"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
    }
}
