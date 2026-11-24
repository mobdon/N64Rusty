//! The audio interface: takes finished stereo buffers from guest RAM and
//! sends them to a WAV file and the speakers.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::PathBuf;

use n64rusty_runtime::Memory;

use crate::Wav;

/// Host stand-in for the audio interface (AI).
#[derive(Default)]
pub struct AudioOut {
    rate: u32,
    muted: bool,
    wav_path: Option<PathBuf>,
    wav: Option<Wav<BufWriter<File>>>,
    #[cfg(feature = "speaker")]
    speaker: Option<Speaker>,
}

impl AudioOut {
    /// Output to nothing until a WAV file or speaker is added.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records everything played to a WAV file, created at the first
    /// buffer with the frequency set then.
    ///
    /// shortcut: a WAV file has one rate, so a later frequency change is
    /// recorded at the first rate; start a new file per rate if games need it.
    pub fn record_wav(&mut self, path: impl Into<PathBuf>) {
        self.wav_path = Some(path.into());
    }

    /// Plays through the default output device.
    #[cfg(feature = "speaker")]
    pub fn open_speaker(&mut self) -> Result<(), String> {
        self.speaker = Some(Speaker::open()?);
        Ok(())
    }

    /// Sets the sample rate in Hz, as the game does when it sets the AI
    /// frequency.
    pub fn set_frequency(&mut self, hz: u32) {
        self.rate = hz;
    }

    /// The sample rate in Hz.
    pub fn frequency(&self) -> u32 {
        self.rate
    }

    /// Silences the speakers. WAV recording carries on.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        #[cfg(feature = "speaker")]
        if let (true, Some(s)) = (muted, &self.speaker) {
            s.clear();
        }
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Plays the `len`-byte buffer of big-endian interleaved left/right
    /// 16-bit samples at `addr` in guest RAM, as the game does when it
    /// queues the next AI buffer.
    pub fn play(&mut self, ram: &Memory, addr: u32, len: u32) -> io::Result<()> {
        let samples: Vec<i16> = (0..len / 2)
            .map(|i| ram.read_u16(addr + 2 * i) as i16)
            .collect();
        if let (None, Some(path)) = (&self.wav, &self.wav_path) {
            self.wav = Some(Wav::new(BufWriter::new(File::create(path)?), self.rate)?);
        }
        if let Some(wav) = &mut self.wav {
            wav.write(&samples)?;
        }
        #[cfg(feature = "speaker")]
        if let (false, Some(s)) = (self.muted, &self.speaker) {
            s.push(self.rate, &samples);
        }
        Ok(())
    }

    /// Completes the WAV file, if one is being written.
    pub fn finish(&mut self) -> io::Result<()> {
        self.wav_path = None;
        match self.wav.take() {
            Some(wav) => wav.finish().map(drop),
            None => Ok(()),
        }
    }
}

#[cfg(feature = "speaker")]
pub use speaker::Speaker;

#[cfg(feature = "speaker")]
mod speaker {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

    #[derive(Default)]
    struct Queue {
        frames: VecDeque<[f32; 2]>,
        rate: u32,
        pos: f64,
    }

    impl Queue {
        /// Next frame at the device rate, linearly interpolated from the
        /// game's rate. Silence when the game has not supplied enough.
        fn next(&mut self, dev_rate: u32) -> [f32; 2] {
            if self.frames.len() < 2 || self.rate == 0 {
                return [0.0; 2];
            }
            let t = self.pos as f32;
            let (a, b) = (self.frames[0], self.frames[1]);
            self.pos += self.rate as f64 / dev_rate as f64;
            while self.pos >= 1.0 && self.frames.len() > 1 {
                self.pos -= 1.0;
                self.frames.pop_front();
            }
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
        }
    }

    /// Speaker output through the default output device.
    ///
    /// Not `Send` on every platform, so keep it on the thread that made it.
    pub struct Speaker {
        queue: Arc<Mutex<Queue>>,
        _stream: Stream,
    }

    impl Speaker {
        /// Opens the default output device and starts playing silence.
        pub fn open() -> Result<Self, String> {
            let dev = cpal::default_host()
                .default_output_device()
                .ok_or("no audio output device")?;
            let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
            let queue = Arc::new(Mutex::new(Queue::default()));
            let q = queue.clone();
            let sc = cfg.config();
            let stream = match cfg.sample_format() {
                SampleFormat::F32 => build::<f32>(&dev, &sc, q),
                SampleFormat::I16 => build::<i16>(&dev, &sc, q),
                SampleFormat::U16 => build::<u16>(&dev, &sc, q),
                f => return Err(format!("unsupported output sample format {f}")),
            }
            .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            Ok(Self {
                queue,
                _stream: stream,
            })
        }

        /// Queues interleaved left/right samples at `rate` Hz. Keeps at most
        /// half a second queued so output never drifts behind the game.
        pub fn push(&self, rate: u32, samples: &[i16]) {
            let mut q = self.queue.lock().unwrap();
            q.rate = rate;
            let f = |s: i16| s as f32 / 32768.0;
            q.frames
                .extend(samples.chunks_exact(2).map(|c| [f(c[0]), f(c[1])]));
            let max = (rate / 2).max(1) as usize;
            while q.frames.len() > max {
                q.frames.pop_front();
            }
        }

        /// Drops everything queued.
        pub fn clear(&self) {
            self.queue.lock().unwrap().frames.clear();
        }
    }

    fn build<T: SizedSample + FromSample<f32>>(
        dev: &cpal::Device,
        cfg: &StreamConfig,
        q: Arc<Mutex<Queue>>,
    ) -> Result<Stream, cpal::BuildStreamError> {
        let (ch, rate) = (cfg.channels as usize, cfg.sample_rate.0);
        dev.build_output_stream(
            cfg,
            move |data: &mut [T], _| {
                let mut q = q.lock().unwrap();
                for frame in data.chunks_mut(ch) {
                    let [l, r] = q.next(rate);
                    for (i, s) in frame.iter_mut().enumerate() {
                        let v = match (ch, i) {
                            (1, _) => (l + r) / 2.0,
                            (_, 0) => l,
                            (_, 1) => r,
                            _ => 0.0,
                        };
                        *s = T::from_sample(v);
                    }
                }
            },
            |e| eprintln!("audio output error: {e}"),
            None,
        )
    }
}
