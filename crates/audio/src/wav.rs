//! Minimal 16-bit stereo PCM WAV writer.

use std::io::{self, Seek, SeekFrom, Write};

/// Writes 16-bit stereo PCM to a RIFF WAV stream. Sizes in the header are
/// filled in by [`Wav::finish`].
pub struct Wav<W: Write + Seek> {
    w: W,
    bytes: u32,
}

impl<W: Write + Seek> Wav<W> {
    /// Writes the header for `rate` Hz. Fails for a rate of 0, which games
    /// leave until they set the audio frequency, or one too large for the
    /// header's byte-rate field.
    pub fn new(mut w: W, rate: u32) -> io::Result<Self> {
        if rate == 0 || rate > u32::MAX / 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("WAV sample rate {rate} Hz is not usable"),
            ));
        }
        w.write_all(b"RIFF\0\0\0\0WAVEfmt ")?;
        for v in [16u32, 0x0002_0001, rate, rate * 4, 0x0010_0004] {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(b"data\0\0\0\0")?;
        Ok(Self { w, bytes: 0 })
    }

    /// Appends interleaved left/right samples. Fails once the data would
    /// pass the 4 GiB a WAV header can describe (about 6 hours at 48 kHz).
    /// An odd count would split a left/right pair, so it is refused.
    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        if samples.len() % 2 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAV data must be whole left/right pairs",
            ));
        }
        let buf: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let total = u32::try_from(buf.len())
            .ok()
            .and_then(|n| n.checked_add(self.bytes))
            .filter(|&t| t <= u32::MAX - 36)
            .ok_or_else(|| io::Error::other("WAV recording reached its 4 GiB limit"))?;
        self.w.write_all(&buf)?;
        self.bytes = total;
        Ok(())
    }

    /// Fills in the sizes and returns the stream.
    pub fn finish(mut self) -> io::Result<W> {
        self.w.seek(SeekFrom::Start(4))?;
        self.w.write_all(&(36 + self.bytes).to_le_bytes())?;
        self.w.seek(SeekFrom::Start(40))?;
        self.w.write_all(&self.bytes.to_le_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()?;
        Ok(self.w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_data_past_the_header_limit() {
        let mut w = Wav::new(io::Cursor::new(Vec::new()), 32000).unwrap();
        w.bytes = u32::MAX - 36 - 4;
        w.write(&[1, 2]).unwrap();
        assert!(w.write(&[3, 4]).is_err());
        assert_eq!(w.bytes, u32::MAX - 36, "a refused write changes nothing");
    }

    #[test]
    fn refuses_unusable_rates_and_half_pairs() {
        let new = |rate| Wav::new(io::Cursor::new(Vec::new()), rate);
        assert!(new(0).is_err());
        assert!(new(u32::MAX / 4 + 1).is_err());
        let mut w = new(u32::MAX / 4).unwrap();
        assert!(w.write(&[1, 2, 3]).is_err());
        assert_eq!(w.bytes, 0);
    }
}
