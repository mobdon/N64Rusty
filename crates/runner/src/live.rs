//! Live play: the game in a window, with sound and controllers.
//!
//! The window, frame pacing, keyboard and gamepad input, record and replay
//! all come from `n64rusty-host`. The game runs on its own threads as in a
//! headless run; the two sides meet at the video retrace:
//!
//! - each host frame (paced at the game's own rate, never faster) hands one
//!   set of controller states to the game, and the game's next retrace
//!   waits for it, so the game can never run ahead of the host;
//! - each finished frame the game shows is rendered at the window's size,
//!   keeping the game's 4:3 shape with black bars;
//! - each audio buffer goes to the speakers on a thread of its own.
//!
//! With `headless = true` in the run config nothing is opened: frames run
//! as fast as the game allows and input comes from the replay script, which
//! is how this path is tested.

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use n64rusty_gfx::{Frame, Image, Renderer};
use n64rusty_host::{Guest, Pads, RunConfig};
use n64rusty_runtime::Ctx;
use winit::window::Window;

use crate::devices::{Devices, Pad};
use crate::media::Media;

/// The last finished frame, waiting to be drawn.
type Shown = Arc<Mutex<Option<Frame>>>;
/// Audio buffers for the speakers: rate and interleaved left/right samples.
type Sound = (u32, Vec<i16>);
/// The window and the pixel buffer drawn into it.
type Surface = softbuffer::Surface<Arc<Window>, Arc<Window>>;

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
    window: Option<(Arc<Window>, Surface)>,
    renderer: Option<Renderer>,
    renderer_failed: bool,
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
        renderer: None,
        renderer_failed: false,
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
        self.present();
    }

    fn attach_window(&mut self, window: Arc<Window>) {
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()));
        match surface {
            Ok(s) => self.window = Some((window, s)),
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
    /// size.
    fn present(&mut self) {
        let Some((window, surface)) = &mut self.window else {
            return;
        };
        let Some(frame) = self.shown.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };
        if self.renderer.is_none() && !self.renderer_failed {
            match Renderer::new() {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    eprintln!("warning: no renderer, nothing is drawn: {e}");
                    self.renderer_failed = true;
                }
            }
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let (x, y, fw, fh) = fit(w.get(), h.get(), frame.width.max(1), frame.height());
        // shortcut: the frame is read back and copied into the window on
        // the CPU; render straight into a window surface if this is slow.
        let image = match renderer.render(&frame, fw, fh) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("warning: frame not drawn: {e}");
                return;
            }
        };
        let result = surface.resize(w, h).and_then(|()| {
            let mut buf = surface.buffer_mut()?;
            letterbox(&image, x, y, w.get(), &mut buf);
            buf.present()
        });
        if let Err(e) = result {
            eprintln!("warning: frame not shown: {e}");
        }
    }
}

/// The largest rectangle with the frame's shape that fits the window,
/// centred: `(x, y, width, height)`.
pub fn fit(win_w: u32, win_h: u32, frame_w: u32, frame_h: u32) -> (u32, u32, u32, u32) {
    let (fw, fh) = (frame_w as u64, frame_h as u64);
    let (w, h) = if win_w as u64 * fh <= win_h as u64 * fw {
        (win_w as u64, win_w as u64 * fh / fw)
    } else {
        (win_h as u64 * fw / fh, win_h as u64)
    };
    let (w, h) = (w.max(1) as u32, h.max(1) as u32);
    ((win_w - w.min(win_w)) / 2, (win_h - h.min(win_h)) / 2, w, h)
}

/// Copies `image` into a window buffer `width` pixels wide at (x, y), black
/// elsewhere. Window pixels are `0x00RRGGBB`.
pub fn letterbox(image: &Image, x: u32, y: u32, width: u32, buf: &mut [u32]) {
    buf.fill(0);
    let height = buf.len() as u32 / width.max(1);
    for row in 0..image.height.min(height.saturating_sub(y)) {
        let cols = image.width.min(width.saturating_sub(x)) as usize;
        let src = &image.rgba[(row * image.width * 4) as usize..][..cols * 4];
        let at = ((y + row) * width + x) as usize;
        for (d, s) in buf[at..at + cols].iter_mut().zip(src.as_chunks::<4>().0) {
            *d = (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32;
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
    let (live, screen) = pair(media);
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

    #[test]
    fn fit_keeps_the_shape_and_centres() {
        assert_eq!(fit(640, 480, 320, 240), (0, 0, 640, 480));
        assert_eq!(fit(1920, 1080, 320, 240), (240, 0, 1440, 1080));
        assert_eq!(fit(800, 1000, 320, 240), (0, 200, 800, 600));
        assert_eq!(fit(1, 1, 320, 240), (0, 0, 1, 1));
    }

    #[test]
    fn letterbox_places_the_image_with_black_bars() {
        let image = Image {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
        };
        let mut buf = vec![7; 4 * 3];
        letterbox(&image, 1, 1, 4, &mut buf);
        assert_eq!(buf[..4], [0; 4]);
        assert_eq!(buf[4..8], [0, 0xFF0000, 0x0000FF, 0]);
        assert_eq!(buf[8..], [0; 4]);
    }

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
