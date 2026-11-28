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
//! With `refresh` in the window config above the game's rate (or 0, the
//! default, on a display faster than the game), the window is drawn that
//! often: each picture blends object placement from the
//! previous game frame to the newest one by how much of a game frame has
//! passed (see `n64rusty_gfx::interpolate`). The game still runs at its
//! own rate; what is shown trails it by one game frame.
//!
//! With `headless = true` in the run config nothing is opened: frames run
//! as fast as the game allows and input comes from the replay script, which
//! is how this path is tested.

use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use n64rusty_gfx::{Frame, Renderer, WindowTarget};
use n64rusty_host::{Guest, Pads, RunConfig};
use n64rusty_runtime::Ctx;
use winit::window::Window;

use crate::devices::{Devices, Pad};
use crate::media::{open_gpu, Media, SharedGpu};

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
        let samples = crate::media::samples(ctx, addr, len);
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
    /// The game side's device, which the window's renderer shares.
    gpu: SharedGpu,
    widescreen: bool,
    /// Lines the window's picture is drawn at, at most (scaled up to the
    /// window); `None` draws at the window's own resolution.
    max_height: Option<u32>,
    sharp: bool,
    /// One game frame, when pictures are blended between game frames.
    blend: Option<Duration>,
    /// The game frame before the newest one.
    prev: Option<Frame>,
    /// The newest game frame and when it arrived.
    cur: Option<(Frame, Instant)>,
    /// Game frames taken for blending so far: names the newest one.
    taken: u64,
    /// The last picture blended into the window; None when the window must
    /// be drawn again (new window, new picture settings).
    drawn: Option<Picture>,
}

/// One blended picture in the window: which game frame, how far it is
/// blended from the one before, at what window size. The same picture
/// twice in a row is the same pixels, so it is drawn only once.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Picture {
    frame: u64,
    t: f32,
    size: (u32, u32),
}

/// How far (0 to 1) a picture is from the game frame before the newest
/// one to the newest one, `elapsed` after the newest came, with game frames
/// `period` apart: 1 (all of the newest) with no frame before it to blend
/// from, or once a whole period has passed.
fn blend_t(has_prev: bool, elapsed: Duration, period: Duration) -> f32 {
    if !has_prev {
        return 1.0;
    }
    (elapsed.as_secs_f32() / period.as_secs_f32()).min(1.0)
}

/// Connects the two sides. `media` supplies graphics, audio and capture.
pub fn pair(mut media: Media) -> (Live, Screen) {
    let shown = Shown::default();
    media.show_frames(shown.clone());
    let gpu = media.gpu();
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
        gpu,
        widescreen: false,
        max_height: None,
        sharp: true,
        blend: None,
        prev: None,
        cur: None,
        taken: 0,
        drawn: None,
    };
    (live, screen)
}

impl Guest for Screen {
    fn frame(&mut self, _frame: u64, pads: &Pads) {
        // Blocks while the game is still busy with the last frame, so a
        // slow frame slows the game down instead of dropping input.
        if self.input.send(*pads).is_err() {
            // The game side is gone; the process is exiting.
            return;
        }
        if self.blend.is_none() {
            self.present();
        } else if self.has_new_frame() {
            // A new game frame shows at once, as before; other blended
            // pictures are drawn by `redraw`, at the window's rate.
            self.redraw();
        }
    }

    fn redraw(&mut self) {
        let Some((window, _, _)) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let Some(picture) = self.next_picture((size.width, size.height), Instant::now()) else {
            return;
        };
        let (Some((_, renderer, target)), Some((cur, _))) = (&mut self.window, &self.cur) else {
            return;
        };
        let (width, height) = picture.size;
        let r = match &self.prev {
            Some(prev) => renderer.present_between(prev, cur, picture.t, target, width, height),
            None => renderer.present(cur, target, width, height),
        };
        match r {
            // shortcut: a picture the surface skipped (minimised, outdated)
            // counts as drawn; a new size or game frame draws again.
            Ok(()) => self.drawn = Some(picture),
            Err(e) => eprintln!("warning: frame not shown: {e}"),
        }
        profile_window(renderer);
    }

    fn view(&mut self, w: &n64rusty_host::config::WindowConfig, blend: Option<f64>) {
        self.widescreen = w.widescreen;
        self.sharp = !w.console_filter;
        self.max_height = w.max_render_height;
        self.blend = blend.map(|rate| Duration::from_secs_f64(1.0 / rate));
        self.drawn = None;
        if let Some((_, r, _)) = &mut self.window {
            r.set_widescreen(self.widescreen);
            r.set_sharp_textures(self.sharp);
            r.set_max_height(self.max_height);
        }
    }

    fn attach_window(&mut self, window: Arc<Window>) {
        let target = open_gpu(&self.gpu).and_then(|g| {
            let mut r = Renderer::on(g);
            r.set_widescreen(self.widescreen);
            r.set_sharp_textures(self.sharp);
            r.set_max_height(self.max_height);
            let t = r.window_target(window.clone()).map_err(|e| e.to_string())?;
            Ok((r, t))
        });
        match target {
            Ok((r, t)) => self.window = Some((window, r, t)),
            Err(e) => eprintln!("warning: cannot draw into the window: {e}"),
        }
        self.drawn = None;
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
    /// Takes the newest finished game frame for blending, if there is one,
    /// and gives the picture to draw at `now` in a window of `size`: None
    /// when not blending, before the first game frame, or when it would be
    /// the picture drawn last (no new game frame, no change in the blend
    /// and the same size).
    fn next_picture(&mut self, size: (u32, u32), now: Instant) -> Option<Picture> {
        let period = self.blend?;
        let taken = self.shown.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(f) = taken {
            self.prev = self.cur.take().map(|(f, _)| f);
            self.cur = Some((f, now));
            self.taken += 1;
        }
        let (cur, at) = self.cur.as_ref()?;
        // Frames of different sizes are not blended (see
        // `n64rusty_gfx::interpolate`): every t is the same picture.
        let blends = self.prev.as_ref().is_some_and(|p| p.width == cur.width);
        let picture = Picture {
            frame: self.taken,
            t: blend_t(blends, now.saturating_duration_since(*at), period),
            size,
        };
        (self.drawn != Some(picture)).then_some(picture)
    }

    /// Whether a finished game frame waits to be taken.
    fn has_new_frame(&self) -> bool {
        self.shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

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
        profile_window(renderer);
    }
}

/// With `N64RUSTY_PROFILE=n`, a line on the window's drawing every n
/// pictures.
fn profile_window(renderer: &mut n64rusty_gfx::Renderer) {
    static EVERY: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let Some(every) = *EVERY.get_or_init(crate::media::profile_every::<u32>) else {
        return;
    };
    if renderer.stats().frames >= every {
        eprintln!(
            "window: {}",
            crate::media::render_line(&renderer.take_stats())
        );
    }
}

/// Runs the game in a window (or headless, as the config says). `run` runs
/// the game to its end with the given devices and returns the exit code;
/// it runs on its own thread, and the process exits when it returns or
/// when the window is closed. When the window's menu asks for another game
/// or screen (see [`n64rusty_host::one_window`]), the game is stopped
/// instead and this returns, so the program goes on in the same window.
pub fn play(
    cfg: &RunConfig,
    media: Media,
    run: impl FnOnce(Box<dyn Devices>) -> u8 + Send + 'static,
) -> ExitCode {
    // A crash closes the window and often the console with it: keep the
    // message in crash.txt in the current folder (where the program was
    // started from, not necessarily next to it) as well.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let trace = std::backtrace::Backtrace::force_capture();
        let text = format!("thread {thread}: {info}\n\n{trace}\n");
        if let Err(e) = std::fs::write("crash.txt", text) {
            eprintln!("warning: could not write crash.txt: {e}");
        }
    }));
    let (live, mut screen) = pair(media);
    screen.widescreen = cfg.window.widescreen;
    screen.sharp = !cfg.window.console_filter;
    screen.max_height = cfg.window.max_render_height;
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stopping.clone();
    let game = std::thread::Builder::new()
        .name("game".into())
        .spawn(move || {
            let code = run(Box::new(live));
            if stopped.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            // A crash in a program with a start screen (the player) goes
            // back to it in the same window instead of closing everything.
            let screens =
                n64rusty_host::menu::START_SCREEN.load(std::sync::atomic::Ordering::Relaxed);
            if code != 0 && screens {
                let m = crate::os::PANIC_MESSAGE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                n64rusty_host::one_window::game_over(if m.is_empty() {
                    "The game stopped responding.".into()
                } else {
                    m
                });
                return;
            }
            std::process::exit(code.into());
        })
        .expect("starting the game thread");
    match n64rusty_host::run_windowed(screen, cfg) {
        Ok(s) => {
            // Dropping the host side first lets a retrace waiting for the
            // next host frame return, so the game can see it is stopped.
            drop(s);
            if n64rusty_host::one_window::switching() {
                stopping.store(true, std::sync::atomic::Ordering::Release);
                crate::os::stop();
                let _ = game.join();
                crate::os::release();
                println!("game stopped");
            } else {
                println!("window closed");
            }
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

    #[test]
    fn blend_t_runs_from_the_frame_before_to_the_newest() {
        let ms = Duration::from_millis;
        assert_eq!(blend_t(true, ms(0), ms(20)), 0.0);
        assert_eq!(blend_t(true, ms(10), ms(20)), 0.5);
        assert_eq!(blend_t(true, ms(20), ms(20)), 1.0);
        assert_eq!(blend_t(true, ms(500), ms(20)), 1.0);
        // Nothing to blend from: all of the newest.
        assert_eq!(blend_t(false, ms(0), ms(20)), 1.0);
    }

    /// A screen blending game frames `period` apart, with no window.
    fn blending(period: Duration) -> Screen {
        let (_, mut screen) = pair(Media::new(None, 60, None, false, None));
        screen.blend = Some(period);
        screen
    }

    #[test]
    fn window_and_game_side_share_one_device() {
        let (live, screen) = pair(Media::new(None, 60, None, false, None));
        assert!(Arc::ptr_eq(&live.media.gpu(), &screen.gpu));
    }

    /// Takes the next picture as `redraw` does, marking it drawn.
    fn draw(screen: &mut Screen, size: (u32, u32), now: Instant) -> bool {
        let picture = screen.next_picture(size, now);
        if picture.is_some() {
            screen.drawn = picture;
        }
        picture.is_some()
    }

    #[test]
    fn a_picture_is_drawn_once_until_something_changes() {
        let ms = Duration::from_millis;
        let size = (640, 480);
        let t0 = Instant::now();
        let mut s = blending(ms(20));
        // No game frame yet: nothing to draw.
        assert!(!draw(&mut s, size, t0));
        // The first game frame has none before it: drawn once, as it is.
        *s.shown.lock().unwrap() = Some(Frame::default());
        assert!(draw(&mut s, size, t0));
        assert!(!draw(&mut s, size, t0 + ms(10)));
        // A new size draws it again; new picture settings too.
        assert!(draw(&mut s, (800, 480), t0 + ms(10)));
        assert!(!draw(&mut s, (800, 480), t0 + ms(10)));
        s.view(&Default::default(), Some(50.0));
        assert!(draw(&mut s, (800, 480), t0 + ms(10)));
        // The second frame blends from the first until a period has passed.
        s.blend = Some(ms(20));
        *s.shown.lock().unwrap() = Some(Frame::default());
        assert!(draw(&mut s, size, t0 + ms(20)));
        assert!(!draw(&mut s, size, t0 + ms(20)));
        assert!(draw(&mut s, size, t0 + ms(30)));
        assert!(draw(&mut s, size, t0 + ms(40)));
        assert!(!draw(&mut s, size, t0 + ms(50)));
        // Without blending `redraw` draws nothing (frames are drawn as
        // they come, by `frame`).
        s.blend = None;
        *s.shown.lock().unwrap() = Some(Frame::default());
        assert!(!draw(&mut s, size, t0 + ms(60)));
    }

    /// Measures the pictures drawn in one second: host frames at 50 Hz, a
    /// game making a frame every second host frame (25 a second), and the
    /// window drawn at 100 Hz. A host frame draws only when a game frame
    /// waits, as `frame` does, so each game frame shows at the host frame
    /// it comes in. Before, every window tick and every host frame drew a
    /// picture: 150.
    #[test]
    fn a_slow_game_draws_only_pictures_that_change() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let size = (640, 480);
        let mut s = blending(ms(20));
        let mut drawn = 0;
        for tick in 0..100u32 {
            let now = t0 + ms(u64::from(tick) * 10);
            if tick % 2 == 0 {
                // A host frame: the game's frame comes every second one.
                if tick % 4 == 0 {
                    *s.shown.lock().unwrap() = Some(Frame::default());
                }
                if s.has_new_frame() {
                    assert!(draw(&mut s, size, now));
                    assert_eq!(s.cur.as_ref().map(|(_, at)| *at), Some(now));
                    drawn += 1;
                }
            }
            // The window's tick, at the same moment: already drawn.
            drawn += u32::from(draw(&mut s, size, now));
        }
        // The first frame once; each later one at t = 0, 0.5 and 1, and
        // not again while the game makes the next.
        assert_eq!(drawn, 1 + 24 * 3);
    }

    #[test]
    fn frames_of_another_size_are_drawn_once() {
        let ms = Duration::from_millis;
        let size = (640, 480);
        let t0 = Instant::now();
        let mut s = blending(ms(20));
        *s.shown.lock().unwrap() = Some(Frame::default());
        assert!(draw(&mut s, size, t0));
        // A wider frame is not blended from the one before: every t would
        // be the same picture.
        *s.shown.lock().unwrap() = Some(Frame {
            width: 640,
            ..Frame::default()
        });
        assert!(draw(&mut s, size, t0 + ms(20)));
        assert_eq!(s.drawn.map(|p| p.t), Some(1.0));
        assert!(!draw(&mut s, size, t0 + ms(30)));
        assert!(!draw(&mut s, size, t0 + ms(40)));
    }
}
