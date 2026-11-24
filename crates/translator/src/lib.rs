//! Translates N64 (MIPS VR4300, big-endian) machine code into Rust source.
//!
//! The generated Rust links against `n64rusty-runtime`. No translation
//! logic exists yet; the next step adds the MIPS instruction translator with
//! branch delay slots, tested per instruction against a CPU emulator using
//! only synthetic code (never game bytes).

/// Crate version, for reports written next to generated output.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!super::VERSION.is_empty());
    }
}
