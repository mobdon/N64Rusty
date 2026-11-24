//! The default devices for runs: graphics tasks go through the display-list
//! interpreter (chosen finished frames are rendered off screen to PNG) and
//! audio tasks through the audio command interpreter (output optionally
//! recorded to a WAV file). Nothing opens a window or plays sound.

use std::fs::File;
use std::io::BufWriter;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use n64rusty_audio::{AudioRsp, Wav};
use n64rusty_gfx::{write_png, Frame, Interpreter, Renderer};
use n64rusty_host::script::Player;
use n64rusty_runtime::Ctx;

use crate::devices::{Devices, Pad};

/// `OSTask.t.type` of a graphics task.
const TASK_GFX: u32 = 1;
/// `OSTask.t.type` of an audio task.
const TASK_AUDIO: u32 = 2;
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
    renderer: Option<Renderer>,
    renderer_failed: bool,
    tasks: u64,
    errors: u64,
    first_error: Option<String>,
    swaps: u64,
    saved: Vec<PathBuf>,
    last: Option<(u64, usize, usize)>,
    warnings: Vec<String>,
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
            renderer: None,
            renderer_failed: false,
            tasks: 0,
            errors: 0,
            first_error: None,
            swaps: 0,
            saved: Vec::new(),
            last: None,
            warnings: Vec::new(),
        }
    }

    fn save(&mut self, frame: u64, f: &Frame) {
        let Some(dir) = self.capture.clone() else {
            return;
        };
        if self.renderer.is_none() && !self.renderer_failed {
            match Renderer::new() {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    eprintln!("warning: no off-screen renderer, frames are not saved: {e}");
                    self.renderer_failed = true;
                }
            }
        }
        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        let (w, h) = (f.width.max(1), f.height());
        let result = r
            .render(f, w, h)
            .map_err(|e| e.to_string())
            .and_then(|img| {
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let path = dir.join(format!("frame_{frame:05}.png"));
                write_png(&path, &img).map_err(|e| e.to_string())?;
                Ok(path)
            });
        match result {
            Ok(p) => self.saved.push(p),
            Err(e) => eprintln!("warning: frame {frame} not saved: {e}"),
        }
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
            if let Err(e) = panic::catch_unwind(AssertUnwindSafe(|| sound.run(mem, list, len))) {
                self.audio_failed = Some(panic_text(&*e));
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
        let (gfx, bytes) = (&mut self.gfx, ctx.mem.bytes());
        let error = match panic::catch_unwind(AssertUnwindSafe(|| gfx.run(bytes, dl))) {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(p) => Some(format!("interpreter panicked: {}", panic_text(&*p))),
        };
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

    fn swap_buffer(&mut self, _ctx: &Ctx, frame: u64, _framebuffer: u32) {
        let f = self.gfx.take_frame();
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
