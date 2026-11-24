//! The plug-in point for graphics, audio and input.
//!
//! The system library replacements call a [`Devices`] implementation when
//! the game hands work to the hardware. The default, [`Headless`], records
//! nothing and reports no input, which is what headless tests use. The
//! renderer, audio mixer and window/controller crates each implement the
//! part they own.

use n64rusty_runtime::Ctx;

/// One controller's state, as the system library reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    /// Whether a controller is plugged into this port.
    pub connected: bool,
    /// Button bits, as the console reports them.
    pub buttons: u16,
    /// Stick position, -128..127.
    pub stick_x: i8,
    pub stick_y: i8,
}

/// Hardware the game talks to through the system library.
pub trait Devices: Send {
    /// A task was started on the RSP (`osSpTaskStartGo`). `task` is the
    /// guest address of the `OSTask`; type 1 is graphics, 2 is audio.
    /// RAM can be read through `ctx.mem`.
    fn rsp_task(&mut self, ctx: &Ctx, task: u32) {
        let _ = (ctx, task);
    }

    /// The next audio buffer (`osAiSetNextBuffer`): `len` bytes of 16-bit
    /// big-endian stereo samples at guest address `addr`.
    fn audio(&mut self, ctx: &Ctx, addr: u32, len: u32) {
        let _ = (ctx, addr, len);
    }

    /// The audio sample rate requested with `osAiSetFrequency`.
    fn audio_rate(&mut self, hz: u32) {
        let _ = hz;
    }

    /// A video retrace happened; `framebuffer` is the last buffer the game
    /// asked to show.
    fn retrace(&mut self, frame: u64, framebuffer: u32) {
        let _ = (frame, framebuffer);
    }

    /// Controller state for the four ports.
    fn pads(&mut self) -> [Pad; 4] {
        let mut p = [Pad::default(); 4];
        p[0].connected = true;
        p
    }
}

/// No output and no input: one controller plugged in, nothing pressed.
pub struct Headless;

impl Devices for Headless {}
