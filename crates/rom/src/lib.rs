//! Loads N64 ROM images supplied by the user.
//!
//! - Detects the three common byte orders (`.z64` big-endian, `.v64`
//!   byte-swapped, `.n64` word-swapped) and normalises to big-endian.
//! - Hashes the normalised image with SHA-256. That hash names every
//!   per-game file, so all dumps of the same ROM share one local folder.
//! - Reads the header and identifies the boot code variant from a CRC-32 of
//!   the boot code, which decides where the console really starts executing.
//!
//! Nothing here contains or embeds game data.

#![forbid(unsafe_code)]

use std::fmt;

pub mod signature;

use sha2::{Digest, Sha256};

/// Byte order of a ROM file as found on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// Native big-endian (`.z64`).
    BigEndian,
    /// 16-bit byte-swapped (`.v64`).
    ByteSwapped,
    /// 32-bit little-endian words (`.n64`).
    LittleEndian,
}

impl fmt::Display for ByteOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ByteOrder::BigEndian => "big-endian (.z64)",
            ByteOrder::ByteSwapped => "byte-swapped (.v64)",
            ByteOrder::LittleEndian => "little-endian (.n64)",
        })
    }
}

/// Boot code (CIC) variant, identified by CRC-32 of ROM bytes 0x40..0x1000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cic {
    /// CIC-NUS-6101.
    Cic6101,
    /// CIC-NUS-6102, the most common variant.
    Cic6102,
    /// CIC-NUS-6103: starts the game 1 MiB below the header's entry.
    Cic6103,
    /// CIC-NUS-6105.
    Cic6105,
    /// CIC-NUS-6106: starts the game 2 MiB below the header's entry.
    Cic6106,
    /// Not recognised; the CRC is kept for reporting.
    Unknown(u32),
}

impl Cic {
    fn from_crc(crc: u32) -> Self {
        match crc {
            0x6170_A4A1 => Cic::Cic6101,
            0x90BB_6CB5 => Cic::Cic6102,
            0x0B05_0EE0 => Cic::Cic6103,
            0x98BC_2C86 => Cic::Cic6105,
            0xACC8_580A => Cic::Cic6106,
            other => Cic::Unknown(other),
        }
    }

    /// How far below the header's entry address the boot code really loads
    /// and starts the game. Two variants move it.
    pub fn entry_offset(self) -> u32 {
        match self {
            Cic::Cic6103 => 0x10_0000,
            Cic::Cic6106 => 0x20_0000,
            _ => 0,
        }
    }
}

/// Errors loading a ROM.
#[derive(Debug, PartialEq, Eq)]
pub enum RomError {
    /// No more bytes than the header and boot code (0x1000), or a length
    /// that is not a whole number of words. Holds the file's length.
    /// shortcut: both cases share this variant and its "too small" message,
    /// so an odd-length large dump is reported as too small; split it if
    /// that confuses players.
    TooSmall(usize),
    /// The first word matches no known byte order.
    UnknownByteOrder(u32),
}

impl fmt::Display for RomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RomError::TooSmall(n) => write!(f, "file is too small to be an N64 ROM ({n} bytes)"),
            RomError::UnknownByteOrder(w) => {
                write!(
                    f,
                    "first word {w:#010x} matches no known N64 ROM byte order"
                )
            }
        }
    }
}

impl std::error::Error for RomError {}

/// Size of the boot segment the boot code copies to RAM before starting
/// the game.
pub const BOOT_SEGMENT_SIZE: usize = 0x10_0000;
/// ROM offset of the boot segment.
pub const BOOT_SEGMENT_ROM: usize = 0x1000;

/// A loaded ROM, normalised to big-endian.
pub struct Rom {
    bytes: Vec<u8>,
    /// Byte order the file was in.
    pub order: ByteOrder,
    /// Lower-case hex SHA-256 of the normalised image.
    pub sha256: String,
    /// Boot code variant.
    pub cic: Cic,
}

impl Rom {
    /// Loads a ROM image from bytes in any of the three byte orders.
    pub fn from_bytes(mut bytes: Vec<u8>) -> Result<Self, RomError> {
        if bytes.len() < BOOT_SEGMENT_ROM + 4 || bytes.len() % 4 != 0 {
            return Err(RomError::TooSmall(bytes.len()));
        }
        let first = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let order = match first {
            0x8037_1240 => ByteOrder::BigEndian,
            0x3780_4012 => ByteOrder::ByteSwapped,
            0x4012_3780 => ByteOrder::LittleEndian,
            other => return Err(RomError::UnknownByteOrder(other)),
        };
        match order {
            ByteOrder::BigEndian => {}
            ByteOrder::ByteSwapped => bytes.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
            ByteOrder::LittleEndian => bytes.chunks_exact_mut(4).for_each(|c| c.reverse()),
        }
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let cic = Cic::from_crc(crc32(&bytes[0x40..BOOT_SEGMENT_ROM]));
        Ok(Rom {
            bytes,
            order,
            sha256,
            cic,
        })
    }

    /// The normalised big-endian image.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn word(&self, off: usize) -> u32 {
        u32::from_be_bytes(self.bytes[off..off + 4].try_into().unwrap())
    }

    /// Entry address written in the header.
    pub fn header_entry(&self) -> u32 {
        self.word(0x08)
    }

    /// Address where the boot segment is loaded and the game starts.
    pub fn entry(&self) -> u32 {
        self.header_entry().wrapping_sub(self.cic.entry_offset())
    }

    /// Image name from the header, trimmed.
    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.bytes[0x20..0x34])
            .trim_end_matches([' ', '\0'])
            .to_string()
    }

    /// Four-character game code (media, id, region) from the header.
    pub fn game_code(&self) -> String {
        String::from_utf8_lossy(&self.bytes[0x3B..0x3F]).to_string()
    }

    /// Revision byte from the header.
    pub fn revision(&self) -> u8 {
        self.bytes[0x3F]
    }

    /// The boot segment: the bytes the boot code copies to [`Rom::entry`].
    /// Shorter if the ROM itself is shorter.
    pub fn boot_segment(&self) -> &[u8] {
        let end = (BOOT_SEGMENT_ROM + BOOT_SEGMENT_SIZE).min(self.bytes.len());
        &self.bytes[BOOT_SEGMENT_ROM..end]
    }
}

/// CRC-32 (IEEE), bitwise. Only used on 4 KiB of boot code.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic image: header with a made-up name, zero boot code, and a
    /// counting pattern. No real ROM content.
    fn synthetic() -> Vec<u8> {
        let mut v = vec![0u8; 0x2000];
        v[0..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        v[8..12].copy_from_slice(&0x8000_0400u32.to_be_bytes());
        v[0x20..0x24].copy_from_slice(b"TEST");
        v[0x3B..0x3F].copy_from_slice(b"NXXE");
        for (k, b) in v[0x1000..].iter_mut().enumerate() {
            *b = k as u8;
        }
        v
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn all_byte_orders_give_the_same_image_and_hash() {
        let z64 = synthetic();
        let v64: Vec<u8> = z64.chunks(2).flat_map(|c| [c[1], c[0]]).collect();
        let n64: Vec<u8> = z64
            .chunks(4)
            .flat_map(|c| [c[3], c[2], c[1], c[0]])
            .collect();
        let a = Rom::from_bytes(z64.clone()).unwrap();
        let b = Rom::from_bytes(v64).unwrap();
        let c = Rom::from_bytes(n64).unwrap();
        assert_eq!(a.order, ByteOrder::BigEndian);
        assert_eq!(b.order, ByteOrder::ByteSwapped);
        assert_eq!(c.order, ByteOrder::LittleEndian);
        assert_eq!(a.bytes(), &z64[..]);
        assert_eq!(b.bytes(), &z64[..]);
        assert_eq!(c.bytes(), &z64[..]);
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sha256, c.sha256);
        assert_eq!(a.sha256.len(), 64);
    }

    #[test]
    fn header_fields() {
        let r = Rom::from_bytes(synthetic()).unwrap();
        assert_eq!(r.name(), "TEST");
        assert_eq!(r.game_code(), "NXXE");
        assert_eq!(r.header_entry(), 0x8000_0400);
        assert!(matches!(r.cic, Cic::Unknown(_)));
        assert_eq!(r.entry(), 0x8000_0400);
        assert_eq!(r.boot_segment().len(), 0x1000);
        assert_eq!(r.boot_segment()[5], 5);
    }

    #[test]
    fn moved_entry_for_two_boot_variants() {
        assert_eq!(Cic::Cic6103.entry_offset(), 0x10_0000);
        assert_eq!(Cic::Cic6106.entry_offset(), 0x20_0000);
        assert_eq!(Cic::Cic6102.entry_offset(), 0);
    }

    #[test]
    fn rejects_unknown_and_small() {
        assert_eq!(
            Rom::from_bytes(vec![0; 0x2000]).err(),
            Some(RomError::UnknownByteOrder(0))
        );
        assert_eq!(
            Rom::from_bytes(vec![0; 8]).err(),
            Some(RomError::TooSmall(8))
        );
    }

    /// The fewest bytes a ROM can have: header and boot code, plus one word.
    const MIN_LEN: usize = BOOT_SEGMENT_ROM + 4;

    #[test]
    fn rejects_a_length_that_is_not_a_whole_number_of_words() {
        for extra in 1..4 {
            let mut v = synthetic();
            v.resize(v.len() + extra, 0);
            let len = v.len();
            assert_eq!(Rom::from_bytes(v).err(), Some(RomError::TooSmall(len)));
        }
    }

    #[test]
    fn rejects_a_byte_swapped_file_of_odd_halfword_count() {
        // Swapping halfwords still leaves a partial last word.
        let mut v: Vec<u8> = synthetic().chunks(2).flat_map(|c| [c[1], c[0]]).collect();
        v.extend([0, 0]);
        let len = v.len();
        assert_eq!(Rom::from_bytes(v).err(), Some(RomError::TooSmall(len)));
    }

    #[test]
    fn accepts_the_smallest_whole_word_length_and_rejects_one_word_less() {
        let mut v = synthetic();
        v.truncate(MIN_LEN);
        assert!(Rom::from_bytes(v.clone()).is_ok());
        v.truncate(MIN_LEN - 4);
        assert_eq!(
            Rom::from_bytes(v).err(),
            Some(RomError::TooSmall(MIN_LEN - 4))
        );
    }

    #[test]
    fn length_error_names_the_byte_count() {
        let msg = RomError::TooSmall(0x2001).to_string();
        assert!(msg.contains("8193 bytes"), "{msg}");
    }
}
