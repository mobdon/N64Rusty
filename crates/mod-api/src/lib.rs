//! The interface between the player and a mod.
//!
//! A mod is a shared library (`.dll`, `.so`, `.dylib`) in the `mods` folder
//! next to the player; a name ending in `.off` is a mod turned off. Build one
//! as a `cdylib` that depends on this crate and exports
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn n64rusty_mod_install(api: &'static n64rusty_mod_api::Api) -> bool
//! ```
//!
//! It runs once before the game starts: it checks [`Api::sha256`] to see
//! whether it is for this game, may change the cartridge bytes and set
//! overrides, and returns whether it is active. A mod carries no game data:
//! it reads what it needs from the player's own ROMs ([`Api::find_rom`]).

use std::ffi::c_void;

/// The game's machine state, handed to hooks; only valid during the call.
pub type Ctx = *mut c_void;
/// A function that runs instead of the game's function at an address.
pub type Hook = extern "C" fn(Ctx);

/// Changes whenever [`Api`]'s layout does; the player only loads mods built
/// against the same version.
pub const VERSION: u32 = 1;
/// The name of a mod's install function.
pub const INSTALL: &[u8] = b"n64rusty_mod_install";
/// The name of a mod's `extern "C" fn() -> u32` that returns [`VERSION`].
pub const VERSION_FN: &[u8] = b"n64rusty_mod_version";

/// Exports [`VERSION_FN`] for a mod.
#[macro_export]
macro_rules! declare_version {
    () => {
        #[no_mangle]
        pub extern "C" fn n64rusty_mod_version() -> u32 {
            $crate::VERSION
        }
    };
}

#[repr(C)]
pub struct Api {
    /// RAM, physical addresses from 0. The last [`Api::mod_ram_len`] bytes
    /// are above what the game is told it has: the mods' own.
    pub ram: *mut u8,
    pub ram_len: usize,
    pub mod_ram_len: usize,
    /// The cartridge bytes the game will read (big-endian). Only valid
    /// during install.
    pub cart: *mut u8,
    pub cart_len: usize,
    /// SHA-256 of the played ROM, lowercase hex.
    pub sha256: *const u8,
    pub sha256_len: usize,
    /// Another ROM the player owns, by SHA-256 hex, big-endian; null if not
    /// found. The bytes stay valid for the whole run.
    pub find_rom: extern "C" fn(sha: *const u8, sha_len: usize, len: *mut usize) -> *const u8,
    /// Runs `hook` whenever the game calls the function at `addr`.
    pub set_override: extern "C" fn(addr: u32, hook: Hook),
    /// From a hook: runs the game's own function at `addr`.
    pub call_original: extern "C" fn(ctx: Ctx, addr: u32),
    /// The 32 general registers of `ctx`.
    pub regs: extern "C" fn(ctx: Ctx) -> *mut u64,
}

// SAFETY: the player hands mods one Api for the whole run, and runs the game
// (and so every hook) on one thread at a time.
unsafe impl Sync for Api {}

impl Api {
    #[allow(clippy::mut_from_ref)] // the player owns the memory; mods share it
    pub fn ram(&self) -> &mut [u8] {
        // SAFETY: the player's RAM lives for the whole run.
        unsafe { std::slice::from_raw_parts_mut(self.ram, self.ram_len) }
    }
    /// Physical address where the mods' RAM starts.
    pub fn mod_ram(&self) -> u32 {
        (self.ram_len - self.mod_ram_len) as u32
    }
    #[allow(clippy::mut_from_ref)]
    pub fn cart(&self) -> &mut [u8] {
        // SAFETY: valid during install, as documented.
        unsafe { std::slice::from_raw_parts_mut(self.cart, self.cart_len) }
    }
    pub fn sha256(&self) -> &str {
        // SAFETY: the player passes ASCII hex that lives for the run.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.sha256, self.sha256_len))
        }
    }
    pub fn find_rom(&self, sha256: &str) -> Option<&'static [u8]> {
        let mut len = 0;
        let p = (self.find_rom)(sha256.as_ptr(), sha256.len(), &mut len);
        // SAFETY: the player keeps found ROMs for the whole run.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, len) })
    }
    pub fn reg(&self, ctx: Ctx, i: usize) -> u64 {
        // SAFETY: 32 registers.
        unsafe { *(self.regs)(ctx).add(i & 31) }
    }
    fn at(&self, vaddr: u32) -> usize {
        (vaddr & 0x1FFF_FFFF) as usize
    }
    pub fn read_u8(&self, vaddr: u32) -> u8 {
        self.ram()[self.at(vaddr)]
    }
    pub fn write_u8(&self, vaddr: u32, v: u8) {
        self.ram()[self.at(vaddr)] = v;
    }
    pub fn read_u32(&self, vaddr: u32) -> u32 {
        let a = self.at(vaddr);
        u32::from_be_bytes(self.ram()[a..a + 4].try_into().unwrap())
    }
    pub fn write_u32(&self, vaddr: u32, v: u32) {
        let a = self.at(vaddr);
        self.ram()[a..a + 4].copy_from_slice(&v.to_be_bytes());
    }
}
