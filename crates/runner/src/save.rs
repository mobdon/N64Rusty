//! Cartridge save memory (battery-backed SRAM), kept in a file.
//!
//! The game reaches SRAM with PI DMA at physical addresses 0x0800_0000 and
//! up. The file holds the bytes as the game sees them, in order; it is read
//! once at start and rewritten after every write, so nothing is lost when
//! the window closes or the run panics. Without a file the memory only
//! lives for the run, which keeps headless runs repeatable.

use std::path::{Path, PathBuf};

/// Physical start of the SRAM domain on the PI bus.
pub const SRAM_BASE: u32 = 0x0800_0000;
/// The SRAM domain ends where cartridge ROM starts.
const SRAM_END: u32 = 0x1000_0000;
/// Standard SRAM size; larger saves grow the memory as the game writes.
const SRAM_SIZE: usize = 0x8000;
/// The most save memory kept: banked SRAM reaches past 0xC_0000. A write
/// beyond is dropped, so a stray DMA cannot grow the save without bound.
const SRAM_MAX: usize = 0x10_0000;

/// Save memory and the file that backs it.
pub struct Save {
    file: Option<PathBuf>,
    sram: Vec<u8>,
}

impl Save {
    /// Memory that lives only for this run.
    pub fn in_memory() -> Self {
        Save {
            file: None,
            sram: vec![0; SRAM_SIZE],
        }
    }

    /// Memory backed by `file`, read now if it exists.
    pub fn open(file: &Path) -> std::io::Result<Self> {
        let mut sram = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        if sram.len() < SRAM_SIZE {
            sram.resize(SRAM_SIZE, 0);
        }
        Ok(Save {
            file: Some(file.to_path_buf()),
            sram,
        })
    }

    /// The SRAM offset of physical PI address `phys`, if it is in SRAM.
    pub fn sram_offset(phys: u32) -> Option<usize> {
        (SRAM_BASE..SRAM_END)
            .contains(&phys)
            .then(|| (phys - SRAM_BASE) as usize)
    }

    /// Copies SRAM from `off` into `out`.
    pub fn read(&self, off: usize, out: &mut [u8]) {
        for (k, b) in out.iter_mut().enumerate() {
            *b = self.sram.get(off + k).copied().unwrap_or(0);
        }
    }

    /// Writes `data` at `off` and rewrites the file.
    pub fn write(&mut self, off: usize, data: &[u8]) {
        if off + data.len() > SRAM_MAX {
            eprintln!(
                "warning: save memory write at {off:#x}+{:#x} is past {SRAM_MAX:#x}, dropped",
                data.len()
            );
            return;
        }
        if self.sram.len() < off + data.len() {
            self.sram.resize(off + data.len(), 0);
        }
        self.sram[off..off + data.len()].copy_from_slice(data);
        if let Some(f) = &self.file {
            if let Err(e) = write_file(f, &self.sram) {
                eprintln!("warning: save not written to {}: {e}", f.display());
            }
        }
    }
}

/// Replaces `path` with `bytes` through a temporary file, so a crash mid-way
/// leaves the old save whole.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("n64rusty-save-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sram_domain() {
        assert_eq!(Save::sram_offset(0x0800_0000), Some(0));
        assert_eq!(Save::sram_offset(0x0800_7FFF), Some(0x7FFF));
        assert_eq!(Save::sram_offset(0x1000_0000), None);
        assert_eq!(Save::sram_offset(0x0500_0000), None);
    }

    #[test]
    fn missing_file_starts_blank_and_write_creates_it() {
        let f = dir("new").join("save.sra");
        let mut s = Save::open(&f).unwrap();
        let mut out = [9u8; 4];
        s.read(0x100, &mut out);
        assert_eq!(out, [0; 4]);
        s.write(0x100, &[1, 2, 3, 4]);
        let bytes = std::fs::read(&f).unwrap();
        assert_eq!(bytes.len(), SRAM_SIZE);
        assert_eq!(&bytes[0x100..0x104], &[1, 2, 3, 4]);
        assert!(!f.with_extension("tmp").exists());
    }

    #[test]
    fn reopen_reads_back_what_was_written() {
        let f = dir("reopen").join("save.sra");
        Save::open(&f).unwrap().write(0x7FFE, &[0xAB, 0xCD]);
        let s = Save::open(&f).unwrap();
        let mut out = [0u8; 3];
        s.read(0x7FFD, &mut out);
        assert_eq!(out, [0, 0xAB, 0xCD]);
    }

    #[test]
    fn writes_past_the_standard_size_grow_the_save() {
        let f = dir("grow").join("save.sra");
        let mut s = Save::open(&f).unwrap();
        s.write(0x8000, &[7; 16]);
        assert_eq!(std::fs::read(&f).unwrap().len(), 0x8010);
        let mut out = [0u8; 2];
        s.read(0x800F, &mut out);
        assert_eq!(out, [7, 0]);
    }

    #[test]
    fn writes_past_the_limit_are_dropped() {
        let mut s = Save::in_memory();
        s.write(0x7FF_0000, &[1; 16]);
        assert_eq!(s.sram.len(), SRAM_SIZE);
    }

    #[test]
    fn in_memory_writes_no_file() {
        let mut s = Save::in_memory();
        s.write(0, &[5]);
        let mut out = [0u8];
        s.read(0, &mut out);
        assert_eq!(out, [5]);
    }
}
