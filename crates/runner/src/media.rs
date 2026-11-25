//! The default devices for runs: graphics tasks go through the display-list
//! interpreter (chosen finished frames are rendered off screen to PNG) and
//! audio tasks through the audio command interpreter (output optionally
//! recorded to a WAV file). Nothing opens a window or plays sound.

use std::fs::File;
use std::io::BufWriter;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use n64rusty_audio::{AudioRsp, Wav};
use n64rusty_gfx::{write_png, Frame, Interpreter, Readback, Renderer};
use n64rusty_host::script::Player;
use n64rusty_runtime::Ctx;

use crate::devices::{Devices, Pad};

/// `OSTask.t.type` of a graphics task.
const TASK_GFX: u32 = 1;
/// `OSTask.t.type` of an audio task.
const TASK_AUDIO: u32 = 2;
/// Picture decode (inverse DCT and colour conversion of JPEG blocks).
const TASK_JPEG: u32 = 4;
/// Offset of `OSTask.t.data_ptr`: the display list or audio command list.
const TASK_DATA_PTR: u32 = 0x30;
/// Offset of `OSTask.t.data_size`.
const TASK_DATA_SIZE: u32 = 0x34;

/// The default devices: graphics and audio interpreters, optional PNG
/// capture and WAV recording, one controller with nothing pressed.
pub struct Media {
    /// Scripted controller input, if any.
    input: Option<Player>,
    /// Ports the script uses (port 1 is always plugged in).
    ports: [bool; 4],
    retrace: u64,
    sound: AudioRsp,
    /// Where to record, and the recording once the first buffer arrives.
    /// (The audio crate's player can also drive speakers, which is not
    /// `Send` everywhere and never wanted here.)
    wav_path: Option<PathBuf>,
    wav: Option<Wav<BufWriter<File>>>,
    rate: u32,
    audio_tasks: u64,
    audio_buffers: u64,
    /// Set when the audio interpreter stopped on a command it lacks; audio
    /// tasks are skipped from then on so the game keeps running.
    audio_failed: Option<String>,
    gfx: Interpreter,
    capture: Option<PathBuf>,
    every: u64,
    /// Saved frames are this many times the game's resolution.
    scale: u32,
    /// Saved frames are 16:9 widescreen.
    wide: bool,
    /// Also save the picture half-way from the frame before each saved one,
    /// as a high refresh rate window would show it.
    between: bool,
    /// Per-game HUD areas for widescreen and the RAM word (address, value)
    /// that must match for them to apply (see `WindowConfig::hud`).
    hud: Vec<[f32; 5]>,
    hud_when: Option<[u32; 2]>,
    /// The frame before the next one to save, kept when `between` is set.
    prev: Option<Frame>,
    renderer: Option<Renderer>,
    renderer_failed: bool,
    tasks: u64,
    errors: u64,
    first_error: Option<String>,
    swaps: u64,
    saved: Vec<PathBuf>,
    last: Option<(u64, usize, usize)>,
    warnings: Vec<String>,
    /// Where each finished frame goes for a live window, if anywhere.
    shown: Option<Arc<Mutex<Option<Frame>>>>,
    /// With `N64RUSTY_PROFILE=n`, a profile line every n shown frames.
    profile: Option<Profile>,
}

/// Host time per part of a run, for the profile line.
struct Profile {
    every: u64,
    start: Instant,
    frames: u64,
    dl_us: u64,
    audio_us: u64,
    render_us: u64,
    png_us: u64,
}

impl Profile {
    fn from_env() -> Option<Profile> {
        let every = std::env::var("N64RUSTY_PROFILE")
            .ok()?
            .parse()
            .unwrap_or(120);
        Some(Profile {
            every: u64::max(every, 1),
            start: Instant::now(),
            frames: 0,
            dl_us: 0,
            audio_us: 0,
            render_us: 0,
            png_us: 0,
        })
    }
}

/// One line on where a renderer's time went, per picture.
pub(crate) fn render_line(s: &n64rusty_gfx::Stats) -> String {
    let n = u64::from(s.frames.max(1));
    let ms = |us: u64| us as f64 / n as f64 / 1000.0;
    format!(
        "render {:.2} ms (record {:.2}, of it textures {:.2}; submit+wait {:.2}), draws {}, textures uploaded {} ({} KB) per picture",
        ms(s.record_us + s.submit_us),
        ms(s.record_us),
        ms(s.upload_us),
        ms(s.submit_us),
        s.draws / n,
        s.textures_new / n,
        s.texture_bytes / n / 1024,
    )
}

impl Media {
    /// Saves every `every`-th finished frame to `capture` and records the
    /// sound to `wav`, when given. `n_audio` selects the audio command set
    /// of the alternative audio library. `input` is a controller script
    /// (`frame port buttons x y` lines; see `n64rusty_host::script`), read
    /// against retrace numbers.
    pub fn new(
        capture: Option<PathBuf>,
        every: u64,
        wav: Option<PathBuf>,
        n_audio: bool,
        input: Option<Vec<n64rusty_host::script::Event>>,
    ) -> Self {
        let mut ports = [true, false, false, false];
        for e in input.iter().flatten() {
            ports[e.port] = true;
        }
        Media {
            input: input.map(Player::new),
            ports,
            retrace: 0,
            sound: if n_audio {
                AudioRsp::n_audio()
            } else {
                AudioRsp::new()
            },
            wav_path: wav,
            wav: None,
            rate: 0,
            audio_tasks: 0,
            audio_buffers: 0,
            audio_failed: None,
            gfx: Interpreter::new(),
            capture,
            every: every.max(1),
            scale: 1,
            wide: false,
            between: false,
            hud: Vec::new(),
            hud_when: None,
            prev: None,
            renderer: None,
            renderer_failed: false,
            tasks: 0,
            errors: 0,
            first_error: None,
            swaps: 0,
            saved: Vec::new(),
            last: None,
            warnings: Vec::new(),
            shown: None,
            profile: Profile::from_env(),
        }
    }

    /// Saves frames at `scale` times the game's resolution (1 to 8).
    pub fn with_scale(mut self, scale: u32) -> Self {
        self.scale = scale.clamp(1, 8);
        self
    }

    /// Saves frames 16:9 wide, as the window's widescreen option draws them.
    pub fn with_widescreen(mut self, on: bool) -> Self {
        self.wide = on;
        self
    }

    /// Also saves `frame_<n>_half.png`: half-way from frame n-1 to frame n.
    pub fn with_between(mut self, on: bool) -> Self {
        self.between = on;
        self
    }

    /// HUD areas kept at the edges in widescreen, used while the RAM word
    /// in `when` (address, value) matches, or always without one.
    pub fn with_hud(mut self, hud: Vec<[f32; 5]>, when: Option<[u32; 2]>) -> Self {
        self.hud = hud;
        self.hud_when = when;
        self
    }

    /// Hands every finished frame to `slot`, replacing one not yet taken.
    pub fn show_frames(&mut self, slot: Arc<Mutex<Option<Frame>>>) {
        self.shown = Some(slot);
    }

    fn save(&mut self, frame: u64, f: &Frame) {
        let prev = self.prev.take();
        self.save_as(&format!("frame_{frame:05}.png"), f, None);
        if let Some(p) = prev {
            self.save_as(&format!("frame_{frame:05}_half.png"), f, Some(&p));
        }
    }

    /// Renders `f` (half-way from `prev`, if given) and saves it as `name`.
    fn save_as(&mut self, name: &str, f: &Frame, prev: Option<&Frame>) {
        let Some(dir) = self.capture.clone() else {
            return;
        };
        if self.renderer.is_none() && !self.renderer_failed {
            match Renderer::new() {
                Ok(mut r) => {
                    r.set_widescreen(self.wide);
                    self.renderer = Some(r);
                }
                Err(e) => {
                    eprintln!("warning: no off-screen renderer, frames are not saved: {e}");
                    self.renderer_failed = true;
                }
            }
        }
        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        let (mut w, h) = (f.width.max(1) * self.scale, f.height() * self.scale);
        if self.wide {
            w = (h * 16 / 9).max(w);
        }
        let t0 = Instant::now();
        let img = match prev {
            Some(p) => r.render_between(p, f, 0.5, w, h),
            None => r.render(f, w, h),
        };
        let t1 = Instant::now();
        let result = img.map_err(|e| e.to_string()).and_then(|img| {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(name);
            write_png(&path, &img).map_err(|e| e.to_string())?;
            Ok(path)
        });
        if let Some(p) = &mut self.profile {
            p.render_us += (t1 - t0).as_micros() as u64;
            p.png_us += t1.elapsed().as_micros() as u64;
        }
        match result {
            Ok(p) => self.saved.push(p),
            Err(e) => eprintln!("warning: {name} not saved: {e}"),
        }
    }

    /// Counts a shown frame and prints the profile line when one is due.
    fn profile_frame(&mut self) {
        let Some(p) = &mut self.profile else { return };
        p.frames += 1;
        if p.frames < p.every {
            return;
        }
        let n = p.frames as f64;
        let ms = |us: u64| us as f64 / n / 1000.0;
        let wall = p.start.elapsed().as_micros() as u64;
        let other = wall.saturating_sub(p.dl_us + p.audio_us + p.render_us + p.png_us);
        let mut line = format!(
            "profile at frame {}: {:.2} ms/frame = guest {:.2} + display lists {:.2} + audio {:.2} + capture render {:.2} + png {:.2}",
            self.swaps,
            ms(wall),
            ms(other),
            ms(p.dl_us),
            ms(p.audio_us),
            ms(p.render_us),
            ms(p.png_us),
        );
        if let Some(r) = self.renderer.as_mut() {
            line.push_str("; ");
            line.push_str(&render_line(&r.take_stats()));
        }
        eprintln!("{line}");
        *p = Profile {
            every: p.every,
            ..Profile::from_env().expect("profiling is on")
        };
    }
}

/// Draws colour images the game reads back, on the off-screen renderer
/// (made on first use), at the game's own resolution.
struct GpuReadback<'a> {
    renderer: &'a mut Option<Renderer>,
    failed: &'a mut bool,
    /// Widescreen setting of saved frames, restored after each read.
    wide: bool,
}

impl Readback for GpuReadback<'_> {
    fn read(&mut self, frame: &Frame, image: u32, width: u32, height: u32) -> Option<Vec<u8>> {
        if self.renderer.is_none() && !*self.failed {
            match Renderer::new() {
                Ok(r) => *self.renderer = Some(r),
                Err(e) => {
                    eprintln!(
                        "warning: no off-screen renderer, drawn images are not read back: {e}"
                    );
                    *self.failed = true;
                }
            }
        }
        let r = self.renderer.as_mut()?;
        let f = Frame {
            width,
            color_image: image,
            ..frame.clone()
        };
        r.set_widescreen(false);
        let img = r.render(&f, width, height);
        r.set_widescreen(self.wide);
        img.ok().map(|i| i.rgba)
    }
}

/// The message of a caught panic.
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

impl Devices for Media {
    fn rsp_task(&mut self, ctx: &mut Ctx, task: u32) {
        let kind = ctx.mem.read_u32(task);
        if kind == TASK_AUDIO {
            if self.audio_failed.is_some() {
                return;
            }
            self.audio_tasks += 1;
            let list = ctx.mem.read_u32(task + TASK_DATA_PTR);
            let len = ctx.mem.read_u32(task + TASK_DATA_SIZE);
            let (sound, mem) = (&mut self.sound, &mut ctx.mem);
            let t0 = Instant::now();
            if let Err(e) = panic::catch_unwind(AssertUnwindSafe(|| sound.run(mem, list, len))) {
                self.audio_failed = Some(panic_text(&*e));
            }
            if let Some(p) = &mut self.profile {
                p.audio_us += t0.elapsed().as_micros() as u64;
            }
            return;
        }
        if kind == TASK_JPEG {
            let data = ctx.mem.read_u32(task + TASK_DATA_PTR);
            if data != 0 {
                crate::jpeg::run(&mut ctx.mem, data);
            }
            return;
        }
        if kind != TASK_GFX {
            return;
        }
        self.tasks += 1;
        let dl = ctx.mem.read_u32(task + TASK_DATA_PTR);
        // A panic inside the interpreter counts as an error for that task;
        // the game keeps running.
        let (gfx, bytes) = (&mut self.gfx, ctx.mem.bytes_mut());
        let mut rb = GpuReadback {
            renderer: &mut self.renderer,
            failed: &mut self.renderer_failed,
            wide: self.wide,
        };
        let run = || gfx.run_readback(bytes, dl, &mut rb);
        let t0 = Instant::now();
        let error = match panic::catch_unwind(AssertUnwindSafe(run)) {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(p) => Some(format!("interpreter panicked: {}", panic_text(&*p))),
        };
        if let Some(p) = &mut self.profile {
            p.dl_us += t0.elapsed().as_micros() as u64;
        }
        if let Some(e) = error {
            self.errors += 1;
            self.first_error.get_or_insert(e);
        }
    }

    fn audio(&mut self, ctx: &Ctx, addr: u32, len: u32) {
        self.audio_buffers += 1;
        let Some(path) = &self.wav_path else {
            return;
        };
        // Whole left/right frames of big-endian 16-bit samples.
        let samples: Vec<i16> = (0..(len & !3) / 2)
            .map(|i| ctx.mem.read_u16(addr + 2 * i) as i16)
            .collect();
        let result = (|| {
            if self.wav.is_none() {
                self.wav = Some(Wav::new(BufWriter::new(File::create(path)?), self.rate)?);
            }
            self.wav.as_mut().map_or(Ok(()), |w| w.write(&samples))
        })();
        if let Err(e) = result {
            self.audio_failed
                .get_or_insert_with(|| format!("recording: {e}"));
            self.wav_path = None;
        }
    }

    fn audio_rate(&mut self, hz: u32) {
        self.rate = hz;
    }

    fn retrace(&mut self, frame: u64, _framebuffer: u32) {
        self.retrace = frame;
    }

    fn pads(&mut self) -> [Pad; 4] {
        let held = match self.input.as_mut() {
            Some(p) => p.pads_at(self.retrace),
            None => Default::default(),
        };
        std::array::from_fn(|i| Pad {
            connected: self.ports[i],
            buttons: held[i].buttons,
            stick_x: held[i].x,
            stick_y: held[i].y,
        })
    }

    fn swap_buffer(&mut self, ctx: &Ctx, frame: u64, _framebuffer: u32) {
        let mut f = self.gfx.take_frame();
        if self.hud_when.is_none_or(|[a, v]| ctx.mem.read_u32(a) == v) {
            f.hud = self.hud.clone();
        }
        self.swaps += 1;
        self.last = Some((frame, f.draws().count(), f.textures.len()));
        for w in &f.warnings {
            if self.warnings.len() < 20 && !self.warnings.contains(w) {
                self.warnings.push(w.clone());
            }
        }
        if self.swaps.is_multiple_of(self.every) {
            self.save(frame, &f);
        }
        if self.between && self.capture.is_some() && (self.swaps + 1).is_multiple_of(self.every) {
            self.prev = Some(f.clone());
        }
        if let Some(slot) = &self.shown {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        }
        self.profile_frame();
    }

    fn summary(&mut self) -> String {
        let mut s = format!(
            "graphics: {} tasks, {} frames shown, {} interpreter errors\n",
            self.tasks, self.swaps, self.errors
        );
        if let Some(e) = &self.first_error {
            s.push_str(&format!("  first error: {e}\n"));
        }
        if let Some((frame, draws, textures)) = self.last {
            s.push_str(&format!(
                "  last frame shown at retrace {frame}: {draws} draws, {textures} textures\n"
            ));
        }
        for w in &self.warnings {
            s.push_str(&format!("  skipped: {w}\n"));
        }
        s.push_str(&format!(
            "audio: {} tasks, {} buffers at {} Hz\n",
            self.audio_tasks, self.audio_buffers, self.rate
        ));
        if let Some(e) = &self.audio_failed {
            s.push_str(&format!("  audio stopped: {e}\n"));
        }
        if let Some(Err(e)) = self.wav.take().map(|w| w.finish()) {
            s.push_str(&format!("  recording not finished: {e}\n"));
        }
        for p in &self.saved {
            s.push_str(&format!("  saved {}\n", p.display()));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_drives_the_pads_by_retrace() {
        let events =
            n64rusty_host::script::parse("0 1 - 0 0\n10 1 START 0 0\n12 2 A 40 -20\n").unwrap();
        let mut m = Media::new(None, 60, None, false, Some(events));
        m.retrace(5, 0);
        let p = m.pads();
        assert!(p[0].connected && p[1].connected && !p[2].connected);
        assert_eq!(p[0].buttons, 0);
        m.retrace(12, 0);
        let p = m.pads();
        assert_eq!(p[0].buttons, n64rusty_host::pad::button::START);
        assert_eq!(
            (p[1].buttons, p[1].stick_x, p[1].stick_y),
            (n64rusty_host::pad::button::A, 40, -20)
        );
    }

    #[test]
    fn no_script_means_one_idle_controller() {
        let mut m = Media::new(None, 60, None, false, None);
        let p = m.pads();
        assert!(p[0].connected && !p[1].connected);
        assert_eq!(p[0].buttons, 0);
    }
}
