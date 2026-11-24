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
use n64rusty_runtime::Ctx;

use crate::devices::Devices;

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
    /// Saves every `every`-th finished frame to `capture`, and records the
    /// sound to `wav`, when given.
    pub fn new(capture: Option<PathBuf>, every: u64, wav: Option<PathBuf>) -> Self {
        Media {
            sound: AudioRsp::new(),
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
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                self.audio_failed = Some(msg);
            }
            return;
        }
        if kind != TASK_GFX {
            return;
        }
        self.tasks += 1;
        let dl = ctx.mem.read_u32(task + TASK_DATA_PTR);
        if let Err(e) = self.gfx.run(ctx.mem.bytes(), dl) {
            self.errors += 1;
            self.first_error.get_or_insert_with(|| e.to_string());
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
