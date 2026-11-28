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
/// A [`Hook`] that also gets the data it was set with
/// ([`Api::set_override_with`]): the mod's own state, without statics.
pub type HookWith = extern "C" fn(Ctx, *mut c_void);

/// Changes whenever [`Api`]'s layout does; the player only loads mods built
/// against the same version. A field only appended keeps a mod built before
/// working, as it never reads past the fields it knows, but still bumps the
/// version: a mod using the new field must not load in an older player,
/// whose Api ends before it, so the mod would call garbage.
pub const VERSION: u32 = 2; // 2: Api::set_override_with appended
/// The name of a mod's install function.
pub const INSTALL: &[u8] = b"n64rusty_mod_install";
/// The name of a mod's `extern "C" fn() -> u32` that returns [`VERSION`].
pub const VERSION_FN: &[u8] = b"n64rusty_mod_version";
/// Largest handoff, in bytes, [`Api::switch_game`] passes on to the next game.
pub const HANDOFF_MAX: usize = 4096;
/// Drops the top 3 bits of a KSEG0/KSEG1 address, leaving the physical one.
const PHYS_MASK: u32 = 0x1FFF_FFFF;

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

/// What the player hands a mod's install: the game's memory and the calls a
/// mod makes back into the player.
#[repr(C)]
pub struct Api {
    /// RAM, physical addresses from 0. The last [`Api::mod_ram_len`] bytes
    /// are above what the game is told it has: the mods' own.
    pub ram: *mut u8,
    /// Bytes at [`Api::ram`], the mods' own included.
    pub ram_len: usize,
    /// Bytes at the end of [`Api::ram`] that are the mods' own.
    pub mod_ram_len: usize,
    /// The cartridge bytes the game will read (big-endian). Only valid
    /// during install.
    pub cart: *mut u8,
    /// Bytes at [`Api::cart`].
    pub cart_len: usize,
    /// SHA-256 of the played ROM, lowercase hex.
    pub sha256: *const u8,
    /// Bytes at [`Api::sha256`].
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
    // Fields are only appended; bump VERSION when one changes.
    /// Ends this game and plays the ROM with this SHA-256 hex next, in the
    /// same window; that game's mods get `data` from [`Api::handoff`].
    /// False if the player has no such ROM.
    pub switch_game:
        extern "C" fn(sha: *const u8, sha_len: usize, data: *const u8, data_len: usize) -> bool,
    /// What the game played before passed to [`Api::switch_game`]; null if
    /// nothing.
    pub handoff: extern "C" fn(len: *mut usize) -> *const u8,
    /// Runs `hook` with `data` whenever the game calls the function at
    /// `addr`. `data` is the mod's own, passed back unchanged; hooks set
    /// this way and with [`Api::set_override`] chain in load order alike.
    pub set_override_with: extern "C" fn(addr: u32, hook: HookWith, data: *mut c_void),
}

// SAFETY: the player hands mods one Api for the whole run, and runs the game
// (and so every hook) on one thread at a time.
unsafe impl Sync for Api {}

impl Api {
    /// All of RAM; see [`Api::ram`]. [`Api::read_u8`] and the other reads
    /// and writes are the safe way in.
    ///
    /// # Safety
    ///
    /// While the slice is in use, nothing else may read or write RAM: no
    /// other slice from this function, no [`Api::read_u8`]-style call, and
    /// no call into the game ([`Api::call_original`]), which writes RAM too.
    #[allow(clippy::mut_from_ref)] // the player owns the memory; mods share it
    pub unsafe fn ram(&self) -> &mut [u8] {
        // SAFETY: the player's RAM lives for the whole run; the caller keeps
        // this the only access while it lives.
        unsafe { std::slice::from_raw_parts_mut(self.ram, self.ram_len) }
    }
    /// Physical address where the mods' RAM starts.
    pub fn mod_ram(&self) -> u32 {
        (self.ram_len - self.mod_ram_len) as u32
    }
    /// The cartridge bytes; see [`Api::cart`].
    ///
    /// # Safety
    ///
    /// Only during install: the player drops the pointer afterwards. While
    /// the slice is in use, no other slice from this function may be.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn cart(&self) -> &mut [u8] {
        // SAFETY: valid during install, and the only access, as the caller
        // promises.
        unsafe { std::slice::from_raw_parts_mut(self.cart, self.cart_len) }
    }
    /// See [`Api::sha256`].
    pub fn sha256(&self) -> &str {
        // SAFETY: the player passes ASCII hex that lives for the run.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.sha256, self.sha256_len))
        }
    }
    /// See [`Api::find_rom`].
    pub fn find_rom(&self, sha256: &str) -> Option<&'static [u8]> {
        let mut len = 0;
        let p = (self.find_rom)(sha256.as_ptr(), sha256.len(), &mut len);
        // SAFETY: the player keeps found ROMs for the whole run.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, len) })
    }
    /// See [`Api::switch_game`]; `data` is at most [`HANDOFF_MAX`] bytes
    /// (false if longer).
    pub fn switch_game(&self, sha256: &str, data: &[u8]) -> bool {
        data.len() <= HANDOFF_MAX
            && (self.switch_game)(sha256.as_ptr(), sha256.len(), data.as_ptr(), data.len())
    }
    /// See [`Api::handoff`].
    pub fn handoff(&self) -> Option<&'static [u8]> {
        let mut len = 0;
        let p = (self.handoff)(&mut len);
        // SAFETY: the player keeps the handoff for the whole run.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, len) })
    }
    /// See [`Api::set_override_with`]; `state` lives for the run, and hooks
    /// get it back with [`state`].
    pub fn set_override_with<T: Sync>(&self, addr: u32, hook: HookWith, state: &'static T) {
        (self.set_override_with)(addr, hook, state as *const T as *mut c_void);
    }
    /// General register `i` (0 to 31) of `ctx`. Only the low 5 bits of `i`
    /// are used, so 32 reads register 0.
    pub fn reg(&self, ctx: Ctx, i: usize) -> u64 {
        // SAFETY: 32 registers.
        unsafe { *(self.regs)(ctx).add(i & 31) }
    }
    /// A pointer to the `len` bytes of RAM at `vaddr` (KSEG0/KSEG1 or
    /// physical). Panics if they run past the end of RAM.
    fn at(&self, vaddr: u32, len: usize) -> *mut u8 {
        let a = (vaddr & PHYS_MASK) as usize;
        assert!(
            a + len <= self.ram_len,
            "mod access to {vaddr:#010x}+{len} is past the end of RAM ({:#x} bytes)",
            self.ram_len
        );
        // SAFETY: inside RAM, just checked. Reads and writes go through raw
        // pointers, never a slice, so they cannot alias one.
        unsafe { self.ram.add(a) }
    }
    /// Copies the bytes at `vaddr` into `out`. This and the other reads and
    /// writes panic if the address is past the end of RAM.
    pub fn read_bytes(&self, vaddr: u32, out: &mut [u8]) {
        let p = self.at(vaddr, out.len());
        // SAFETY: `at` checked the range; `out` is the mod's own memory.
        unsafe { std::ptr::copy(p, out.as_mut_ptr(), out.len()) }
    }
    /// Copies `data` into RAM at `vaddr`.
    pub fn write_bytes(&self, vaddr: u32, data: &[u8]) {
        let p = self.at(vaddr, data.len());
        // SAFETY: `at` checked the range; `data` may even be read from RAM.
        unsafe { std::ptr::copy(data.as_ptr(), p, data.len()) }
    }
    /// The byte at `vaddr`.
    pub fn read_u8(&self, vaddr: u32) -> u8 {
        let mut b = [0; 1];
        self.read_bytes(vaddr, &mut b);
        b[0]
    }
    /// Writes the byte at `vaddr`.
    pub fn write_u8(&self, vaddr: u32, v: u8) {
        self.write_bytes(vaddr, &[v]);
    }
    /// The big-endian word at `vaddr`.
    pub fn read_u32(&self, vaddr: u32) -> u32 {
        let mut b = [0; 4];
        self.read_bytes(vaddr, &mut b);
        u32::from_be_bytes(b)
    }
    /// Writes the big-endian word at `vaddr`.
    pub fn write_u32(&self, vaddr: u32, v: u32) {
        self.write_bytes(vaddr, &v.to_be_bytes());
    }
}

/// In a [`HookWith`]: the state given to [`Api::set_override_with`].
///
/// # Safety
///
/// `data` must be the pointer the player passed this hook, set by
/// [`Api::set_override_with`] with a `&'static T` of this same `T`.
pub unsafe fn state<T>(data: *mut c_void) -> &'static T {
    // SAFETY: the caller's promise above.
    unsafe { &*(data as *const T) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Physical RAM the fake game is told it has (4 MiB, no Expansion Pak).
    const GAME_RAM: usize = 0x40_0000;
    /// The mods' own bytes above it.
    const MOD_RAM: usize = 0x1000;
    // VR4300 manual, kernel address space: KSEG0 and KSEG1 map to physical 0 by dropping the
    // top 3 bits (cached and uncached views of the same RAM).
    const KSEG0: u32 = 0x8000_0000;
    const KSEG1: u32 = 0xA000_0000;
    const PHYS: u32 = 0x1234;

    thread_local! {
        static SWITCHES: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
        static OVERRIDE: Cell<Option<(u32, *mut c_void)>> = const { Cell::new(None) };
    }

    extern "C" fn no_rom(_: *const u8, _: usize, _: *mut usize) -> *const u8 {
        std::ptr::null()
    }
    extern "C" fn no_override(_: u32, _: Hook) {}
    extern "C" fn no_call(_: Ctx, _: u32) {}
    // The fake game's ctx is its register file.
    extern "C" fn regs_of(ctx: Ctx) -> *mut u64 {
        ctx as *mut u64
    }
    extern "C" fn record_switch(sha: *const u8, sha_len: usize, d: *const u8, n: usize) -> bool {
        // SAFETY: Api::switch_game passes a str and a slice with their lengths.
        let (sha, d) = unsafe {
            (
                std::slice::from_raw_parts(sha, sha_len),
                std::slice::from_raw_parts(d, n),
            )
        };
        let sha = String::from_utf8(sha.to_vec()).unwrap();
        SWITCHES.with(|s| s.borrow_mut().push((sha, d.to_vec())));
        true
    }
    extern "C" fn no_handoff(_: *mut usize) -> *const u8 {
        std::ptr::null()
    }
    extern "C" fn record_override_with(addr: u32, _: HookWith, data: *mut c_void) {
        OVERRIDE.with(|o| o.set(Some((addr, data))));
    }
    extern "C" fn noop_hook(_: Ctx, _: *mut c_void) {}

    /// An Api over `ram`, whose last `mod_ram_len` bytes are the mods'.
    fn fake_api(ram: &mut [u8], mod_ram_len: usize) -> Api {
        const SHA: &str = "ab";
        // Each test starts with no recorded calls, whatever thread it runs on.
        SWITCHES.with(|s| s.borrow_mut().clear());
        OVERRIDE.with(|o| o.set(None));
        Api {
            ram: ram.as_mut_ptr(),
            ram_len: ram.len(),
            mod_ram_len,
            cart: std::ptr::null_mut(),
            cart_len: 0,
            sha256: SHA.as_ptr(),
            sha256_len: SHA.len(),
            find_rom: no_rom,
            set_override: no_override,
            call_original: no_call,
            regs: regs_of,
            switch_game: record_switch,
            handoff: no_handoff,
            set_override_with: record_override_with,
        }
    }

    fn switches() -> Vec<(String, Vec<u8>)> {
        SWITCHES.with(|s| s.borrow().clone())
    }

    #[test]
    fn switch_game_refuses_data_over_4096_bytes_without_calling_the_player() {
        let mut ram = vec![0; 16];
        let api = fake_api(&mut ram, 0);
        assert!(!api.switch_game("cd", &[0; HANDOFF_MAX + 1]));
        assert!(switches().is_empty());
    }

    #[test]
    fn switch_game_passes_exactly_4096_bytes_and_the_sha_to_the_player() {
        let mut ram = vec![0; 16];
        let api = fake_api(&mut ram, 0);
        let data: Vec<u8> = (0..HANDOFF_MAX).map(|i| i as u8).collect();
        assert!(api.switch_game("cd", &data));
        assert_eq!(switches(), vec![("cd".to_string(), data)]);
    }

    #[test]
    fn switch_game_passes_empty_data() {
        let mut ram = vec![0; 16];
        let api = fake_api(&mut ram, 0);
        assert!(api.switch_game("cd", &[]));
        assert_eq!(switches(), vec![("cd".to_string(), vec![])]);
    }

    #[test]
    fn mod_ram_starts_where_the_games_ram_ends() {
        let mut ram = vec![0; GAME_RAM + MOD_RAM];
        assert_eq!(fake_api(&mut ram, MOD_RAM).mod_ram(), GAME_RAM as u32);
    }

    #[test]
    fn mod_ram_is_the_end_of_ram_when_mods_have_none() {
        let mut ram = vec![0; GAME_RAM];
        assert_eq!(fake_api(&mut ram, 0).mod_ram(), GAME_RAM as u32);
    }

    #[test]
    fn read_u8_sees_the_same_byte_through_kseg0_kseg1_and_physical() {
        let mut ram = vec![0; 0x2000];
        ram[PHYS as usize] = 0x5A;
        let api = fake_api(&mut ram, 0);
        for addr in [PHYS, KSEG0 | PHYS, KSEG1 | PHYS] {
            assert_eq!(api.read_u8(addr), 0x5A, "{addr:#010x}");
        }
    }

    #[test]
    fn write_u8_through_kseg1_lands_at_the_physical_address() {
        let mut ram = vec![0; 0x2000];
        let api = fake_api(&mut ram, 0);
        api.write_u8(KSEG1 | PHYS, 0xA5);
        assert_eq!(ram[PHYS as usize], 0xA5);
    }

    #[test]
    fn read_u32_is_big_endian_and_masks_kseg0() {
        let mut ram = vec![0; 0x2000];
        ram[PHYS as usize..PHYS as usize + 4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        let api = fake_api(&mut ram, 0);
        // The VR4300 in the N64 runs big-endian: the first byte is the top one.
        assert_eq!(api.read_u32(KSEG0 | PHYS), 0x1234_5678);
        assert_eq!(api.read_u32(KSEG1 | PHYS), 0x1234_5678);
    }

    #[test]
    fn write_u32_stores_big_endian_at_the_physical_address() {
        let mut ram = vec![0; 0x2000];
        let api = fake_api(&mut ram, 0);
        api.write_u32(KSEG0 | PHYS, 0xDEAD_BEEF);
        let p = PHYS as usize;
        assert_eq!(ram[p..p + 4], [0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn word_access_reaches_the_last_word_of_ram() {
        let mut ram = vec![0; 0x100];
        let last = (ram.len() - 4) as u32;
        let api = fake_api(&mut ram, 0);
        api.write_u32(KSEG1 | last, 0x0102_0304);
        assert_eq!(api.read_u32(last), 0x0102_0304);
    }

    #[test]
    fn byte_runs_copy_in_and_out_through_kseg1() {
        let mut ram = vec![0; 0x2000];
        let api = fake_api(&mut ram, 0);
        api.write_bytes(KSEG1 | PHYS, &[1, 2, 3]);
        let mut back = [0; 3];
        api.read_bytes(KSEG0 | PHYS, &mut back);
        assert_eq!(back, [1, 2, 3]);
        assert_eq!(ram[PHYS as usize..PHYS as usize + 3], [1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "past the end of RAM")]
    fn a_word_straddling_the_end_of_ram_panics_with_the_address() {
        let mut ram = vec![0; 0x100];
        let api = fake_api(&mut ram, 0);
        api.read_u32(0xFE);
    }

    #[test]
    fn reg_reads_each_of_the_32_registers() {
        let mut ram = vec![0; 16];
        let api = fake_api(&mut ram, 0);
        let mut file: [u64; 32] = std::array::from_fn(|i| 0xFFFF_0000_0000_0000 | i as u64);
        let ctx = file.as_mut_ptr() as Ctx;
        for (i, &want) in file.iter().enumerate() {
            assert_eq!(api.reg(ctx, i), want, "register {i}");
        }
    }

    #[test]
    fn state_gives_a_hook_back_what_set_override_with_was_given() {
        struct Settings {
            speed: u32,
        }
        static SETTINGS: Settings = Settings { speed: 3 };
        const ADDR: u32 = 0x8000_1000;
        let mut ram = vec![0; 16];
        let api = fake_api(&mut ram, 0);
        api.set_override_with(ADDR, noop_hook, &SETTINGS);
        let (addr, data) = OVERRIDE.with(|o| o.get()).expect("player was not called");
        assert_eq!(addr, ADDR);
        // SAFETY: data was set from a &'static Settings just above.
        let back: &Settings = unsafe { state(data) };
        assert!(std::ptr::eq(back, &SETTINGS));
        assert_eq!(back.speed, 3);
    }
}
