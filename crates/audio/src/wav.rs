//! Minimal 16-bit stereo PCM WAV writer.

use std::io::{self, Seek, SeekFrom, Write};

/// Writes 16-bit stereo PCM to a RIFF WAV stream. Sizes in the header are
/// filled in by [`Wav::finish`].
pub struct Wav<W: Write + Seek> {
    w: W,
    bytes: u32,
}

impl<W: Write + Seek> Wav<W> {
    /// Writes the header for `rate` Hz.
    pub fn new(mut w: W, rate: u32) -> io::Result<Self> {
        w.write_all(b"RIFF\0\0\0\0WAVEfmt ")?;
        for v in [16u32, 0x0002_0001, rate, rate * 4, 0x0010_0004] {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(b"data\0\0\0\0")?;
        Ok(Self { w, bytes: 0 })
    }

    /// Appends interleaved left/right samples.
    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let buf: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.w.write_all(&buf)?;
        self.bytes = self.bytes.saturating_add(buf.len() as u32);
        Ok(())
    }

    /// Fills in the sizes and returns the stream.
    pub fn finish(mut self) -> io::Result<W> {
        self.w.seek(SeekFrom::Start(4))?;
        self.w
            .write_all(&(36u32.saturating_add(self.bytes)).to_le_bytes())?;
        self.w.seek(SeekFrom::Start(40))?;
        self.w.write_all(&self.bytes.to_le_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()?;
        Ok(self.w)
    }
}
