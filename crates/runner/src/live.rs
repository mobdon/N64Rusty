//! Live play: the game in a window, with sound and controllers.
//!
//! The window, frame pacing, keyboard and gamepad input, record and replay
//! all come from `n64rusty-host`. The game runs on its own threads as in a
//! headless run; the two sides meet at the video retrace:
//!
//! - each host frame (paced at the game's own rate, never faster) hands one
//!   set of controller states to the game, and the game's next retrace
//!   waits for it, so the game can never run ahead of the host;
//! - each finished frame the game shows is rendered on the GPU straight
//!   into the window at its size, keeping the game's 4:3 shape with black
//!   bars;
//! - each audio buffer goes to the speakers on a thread of its own.
//!
//! With `refresh` in the window config above the game's rate, the window is
//! drawn that often: each picture blends object placement from the
//! previous game frame to the newest one by how much of a game frame has
//! passed (see `n64rusty_gfx::interpolate`). The game still runs at its
//! own rate; what is shown trails it by one game frame.
//!
//! With `headless = true` in the run config nothing is opened: frames run
//! as fast as the game allows and input comes from the replay script, which
//! is how this path is tested.

use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use n64rusty_gfx::{Frame, Renderer, WindowTarget};
use n64rusty_host::{Guest, Pads, RunConfig};
use n64rusty_runtime::Ctx;
use winit::window::Window;

use crate::devices::{Devices, Pad};
use crate::media::Media;

/// The last finished frame, waiting to be drawn.
type Shown = Arc<Mutex<Option<Frame>>>;
/// Audio buffers for the speakers: rate and interleaved left/right samples.
type Sound = (u32, Vec<i16>);

/// The game side: the default devices, with controller input and pacing
/// from the host frame loop.
pub struct Live {
    media: Media,
    input: Receiver<Pads>,
    pads: Pads,
    /// Ports that have had input. Port 1 is always plugged in; another
    /// port is plugged in from the first frame its controller is used.
    seen: [bool; 4],
    sound: SyncSender<Sound>,
    rate: u32,
}

impl Devices for Live {
    fn rsp_task(&mut self, ctx: &mut Ctx, task: u32) {
        self.media.rsp_task(ctx, task);
    }

    fn audio(&mut self, ctx: &Ctx, addr: u32, len: u32) {
        self.media.audio(ctx, addr, len);
        if self.rate == 0 {
            return;
        }
        let samples = (0..(len & !3) / 2)
            .map(|i| ctx.mem.read_u16(addr + 2 * i) as i16)
            .collect();
        // Full when the speakers are not open (or stalled): drop the buffer.
        let _ = self.sound.try_send((self.rate, samples));
    }

    fn audio_rate(&mut self, hz: u32) {
        self.rate = hz;
        self.media.audio_rate(hz);
    }

    fn retrace(&mut self, frame: u64, framebuffer: u32) {
        self.media.retrace(frame, framebuffer);
        // Wait for the host's next frame. An error means the host is gone
        // and the process is about to exit. Retraces count from 1 and host
        // frames from 0: skip host frame 0 so retrace n gets host frame n,
        // as `--input` scripts and headless runs number them.
        if frame == 1 {
            let _ = self.input.recv();
        }
        if let Ok(p) = self.input.recv() {
            self.pads = p;
            for (seen, pad) in self.seen.iter_mut().zip(&p) {
                *seen |= *pad != Default::default();
            }
        }
    }

    fn swap_buffer(&mut self, ctx: &Ctx, frame: u64, framebuffer: u32) {
        self.media.swap_buffer(ctx, frame, framebuffer);
    }

    fn summary(&mut self) -> String {
        self.media.summary()
    }

    fn pads(&mut self) -> [Pad; 4] {
        std::array::from_fn(|i| Pad {
            connected: i == 0 || self.seen[i],
            buttons: self.pads[i].buttons,
            stick_x: self.pads[i].x,
            stick_y: self.pads[i].y,
        })
    }
}

/// The host side: hands each frame's input to the game and draws the
/// frames it shows.
pub struct Screen {
    input: SyncSender<Pads>,
    shown: Shown,
    sound: Option<Receiver<Sound>>,
    window: Option<(Arc<Window>, Renderer, WindowTarget)>,
    widescreen: bool,
    /// One game frame, when pictures are blended between game frames.
    blend: Option<Duration>,
    /// The game frame before the newest one.
    prev: Option<Frame>,
    /// The newest game frame and when it arrived.
    cur: Option<(Frame, Instant)>,
}

/// Connects the two sides. `media` supplies graphics, audio and capture.
pub fn pair(mut media: Media) -> (Live, Screen) {
    let shown = Shown::default();
    media.show_frames(shown.clone());
    let (input_tx, input_rx) = mpsc::sync_channel(1);
    // About two seconds of buffers; the speakers keep at most half a second.
    let (sound_tx, sound_rx) = mpsc::sync_channel(120);
    let live = Live {
        media,
        input: input_rx,
        pads: Pads::default(),
        seen: [false; 4],
        sound: sound_tx,
        rate: 0,
    };
    let screen = Screen {
        input: input_tx,
        shown,
        sound: Some(sound_rx),
        window: None,
        widescreen: false,
        blend: None,
        prev: None,
        cur: None,
    };
    (live, screen)
}

impl Guest for Screen {
    fn frame(&mut self, _frame: u64, pads: &Pads) {
        // Blocks while the game is still busy with the last frame, so a
        // slow frame slows the game down instead of dropping input.
        let mut pads = *pads;
        loop {
            match self.input.try_send(pads) {
                Ok(()) => break,
                Err(TrySendError::Full(p)) => {
                    pads = p;
                    std::thread::sleep(Duration::from_micros(500));
                }
                // The game side is gone; the process is exiting.
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if self.blend.is_some() {
            self.redraw();
        } else {
            self.present();
        }
    }

    fn redraw(&mut self) {
        let Some(period) = self.blend else { return };
        let taken = self.shown.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(f) = taken {
            self.prev = self.cur.take().map(|(f, _)| f);
            self.cur = Some((f, Instant::now()));
        }
        let (Some((window, renderer, target)), Some((cur, at))) = (&mut self.window, &self.cur)
        else {
            return;
        };
        let size = window.inner_size();
        let t = (at.elapsed().as_secs_f32() / period.as_secs_f32()).min(1.0);
        let r = match &self.prev {
            Some(prev) => renderer.present_between(prev, cur, t, target, size.width, size.height),
            None => renderer.present(cur, target, size.width, size.height),
        };
        if let Err(e) = r {
            eprintln!("warning: frame not shown: {e}");
        }
    }

    fn attach_window(&mut self, window: Arc<Window>) {
        let target = Renderer::new().and_then(|mut r| {
            r.set_widescreen(self.widescreen);
            let t = r.window_target(window.clone())?;
            Ok((r, t))
        });
        match target {
            Ok((r, t)) => self.window = Some((window, r, t)),
            Err(e) => eprintln!("warning: cannot draw into the window: {e}"),
        }
        if let Some(rx) = self.sound.take() {
            std::thread::spawn(move || speakers(rx));
        }
    }
}

/// Plays audio buffers until the game side is gone.
fn speakers(rx: Receiver<Sound>) {
    // The speaker stream is not `Send` everywhere: it lives on this thread.
    let speaker = match n64rusty_audio::Speaker::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: no sound: {e}");
            return;
        }
    };
    for (rate, samples) in rx {
        speaker.push(rate, &samples);
    }
}

impl Screen {
    /// Draws the newest finished frame, if there is one, at the window's
    /// size, straight on the GPU.
    fn present(&mut self) {
        let Some((window, renderer, target)) = &mut self.window else {
            return;
        };
        let Some(frame) = self.shown.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let size = window.inner_size();
        if let Err(e) = renderer.present(&frame, target, size.width, size.height) {
            eprintln!("warning: frame not shown: {e}");
        }
    }
}

/// Runs the game in a window (or headless, as the config says). `run` runs
/// the game to its end with the given devices and returns the exit code;
/// it runs on its own thread, and the process exits when it returns or
/// when the window is closed.
pub fn play(
    cfg: &RunConfig,
    media: Media,
    run: impl FnOnce(Box<dyn Devices>) -> u8 + Send + 'static,
) -> ExitCode {
    // A crash closes the window and often the console with it: keep the
    // message in crash.txt in the per-game folder as well.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let trace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write("crash.txt", format!("thread {thread}: {info}\n\n{trace}\n"));
    }));
    let (live, mut screen) = pair(media);
    screen.widescreen = cfg.window.widescreen;
    screen.blend = cfg
        .window
        .refresh
        .filter(|&r| r > cfg.frame_rate)
        .map(|_| Duration::from_secs_f64(1.0 / cfg.frame_rate));
    std::thread::Builder::new()
        .name("game".into())
        .spawn(move || std::process::exit(run(Box::new(live)).into()))
        .expect("starting the game thread");
    match n64rusty_host::run_windowed(screen, cfg) {
        Ok(s) => {
            println!("window closed");
            drop(s);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n64rusty_host::pad::button;

    /// A stand-in for the game: retraces as fast as it can and reads the
    /// controllers after each, as a game's controller thread does.
    fn fake_game(mut live: Live, retraces: u64) -> Vec<[Pad; 4]> {
        let ctx = Ctx::new(n64rusty_runtime::Memory::new(0x1000));
        let mut seen = Vec::new();
        for n in 1..=retraces {
            live.retrace(n, 0);
            seen.push(live.pads());
            live.swap_buffer(&ctx, n, 0);
        }
        seen
    }

    #[test]
    fn headless_host_paces_retraces_and_replays_input() {
        let dir = std::env::temp_dir().join(format!("n64rusty-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("take.txt"),
            "0 1 - 0 0\n3 1 START 0 0\n4 1 A 80 0\n5 1 - 0 0\n5 2 B 0 -40\n",
        )
        .unwrap();
        let cfg =
            RunConfig::parse("headless = true\nframes = 8\nreplay = \"take.txt\"\n", &dir).unwrap();
        let (live, screen) = pair(Media::new(None, 60, None, false, None));
        let shown = screen.shown.clone();
        let game = std::thread::spawn(move || fake_game(live, 7));
        let screen = n64rusty_host::run_windowed(screen, &cfg).unwrap();
        let seen = game.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // Retrace n waits for host frame n and gets that frame's input
        // (seen[0] is retrace 1).
        assert_eq!(seen.len(), 7);
        let buttons: Vec<u16> = seen.iter().map(|p| p[0].buttons).collect();
        assert_eq!(buttons, [0, 0, button::START, button::A, 0, 0, 0]);
        assert_eq!(seen[3][0].stick_x, 80);
        // Port 2 plugs in the first time it is used.
        assert!(seen.iter().all(|p| p[0].connected));
        assert!(!seen[3][1].connected);
        assert!(seen[4][1].connected && seen[6][1].connected);
        assert_eq!((seen[4][1].buttons, seen[4][1].stick_y), (button::B, -40));
        assert!(!seen[6][2].connected);
        // Finished frames reach the screen; with no window none is drawn.
        assert!(shown.lock().unwrap().is_some());
        drop(screen);
    }
}
