//! Host audio for recompiled N64 games.
//!
//! On the console a game's audio library builds a list of 64-bit commands
//! each frame and hands it to the RSP, whose audio microcode decodes ADPCM
//! samples, resamples, applies volume envelopes and mixes into a stereo
//! buffer in RAM. The game then gives that buffer to the audio interface
//! (AI), which plays it at the frequency the game chose.
//!
//! This crate does both halves on the host:
//!
//! - [`AudioRsp::run`] interprets a command list against guest RAM. Call it
//!   where the game starts an audio task on the RSP. Make the interpreter
//!   with [`AudioRsp::new`] for games on the standard audio library, or
//!   [`AudioRsp::n_audio`] for games on the alternative `n_audio` library,
//!   whose microcode has its own command layout.
//! - [`AudioOut`] takes the finished buffers (call [`AudioOut::set_frequency`]
//!   and [`AudioOut::play`] where the game sets the AI frequency and queues
//!   the next AI buffer) and writes them to a WAV file and, with the
//!   `speaker` feature, to the default output device. [`AudioOut::set_muted`]
//!   silences the speakers.
//!
//! Playback runs at the rate the game asks for; nothing here changes game
//! timing.

#![forbid(unsafe_code)]

mod out;
mod rsp;
mod wav;

pub use out::AudioOut;
#[cfg(feature = "speaker")]
pub use out::Speaker;
pub use rsp::AudioRsp;
pub use wav::Wav;
