//! Mods: shared libraries in the `mods` folder next to the program (see
//! `n64rusty-mod-api`). Each one checks the ROM hash itself; one built for
//! another API version is skipped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use n64rusty_mod_api::{Api, Hook, INSTALL, VERSION, VERSION_FN};
use n64rusty_runtime::Ctx;

/// Mod hooks by the guest address they replace, in load order: the last
/// runs first, and its `call_original` reaches the one before.
static HOOKS: Mutex<Option<HashMap<u32, Vec<Hook>>>> = Mutex::new(None);

thread_local! {
    /// The hooked calls running on this thread: the address and which of
    /// its hooks is running. A guest call into the same address again
    /// pushes its own entry, so its chain starts from the last hook.
    static STACK: RefCell<Vec<(u32, usize)>> = const { RefCell::new(Vec::new()) };
}

/// What a mod passed to [`switch_game`], for the next game's mods.
static NEXT_HANDOFF: Mutex<Option<Vec<u8>>> = Mutex::new(None);
/// What the game played before passed on: this run's [`handoff`].
static HANDOFF: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// What a hooked address ran before the mods: a replaced system routine.
static REPLACED: Mutex<Option<HashMap<u32, n64rusty_runtime::GuestFn>>> = Mutex::new(None);

/// The mods turned on, loaded; the `mods` folder is made if missing, so
/// players can find it.
pub fn load() -> Vec<(String, libloading::Library)> {
    if let Some(dir) = n64rusty_host::menu::mods_dir() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut libs = Vec::new();
    for (path, on) in n64rusty_host::menu::mods() {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if !on {
            continue;
        }
        let path = runnable(&path);
        // SAFETY: a mod is code the player chose to put in the mods folder.
        let lib = match unsafe { libloading::Library::new(&path) } {
            Ok(l) => l,
            Err(e) => {
                eprintln!("warning: mod {name}: {e}");
                continue;
            }
        };
        // SAFETY: the mod API's documented signature.
        let v = unsafe { lib.get::<extern "C" fn() -> u32>(VERSION_FN) }.map(|f| f());
        if v.ok() != Some(VERSION) {
            eprintln!("warning: mod {name} is for another player version; skipped");
            continue;
        }
        libs.push((name, lib));
    }
    libs
}

/// Where the mod at `path` is loaded from: a fresh copy for each game the
/// program plays. Loading the same file again would hand back the copy
/// already loaded, with its statics still set up for the game before
/// (its RAM among them), and writing over a loaded file breaks its code.
/// On Android the copy also has to be in the app's own folder: code cannot
/// run from the shared folder the mods are put in.
fn runnable(path: &std::path::Path) -> std::path::PathBuf {
    static LOADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    #[cfg(target_os = "android")]
    let dir = n64rusty_host::android::private_dir().map(|d| d.join("mods"));
    #[cfg(not(target_os = "android"))]
    let dir = Some(std::env::temp_dir().join("n64rusty-mods"));
    let (Some(dir), Some(name)) = (dir, path.file_name()) else {
        return path.to_path_buf();
    };
    let n = LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n == 0 {
        // Copies from earlier runs; any still loaded somewhere stay.
        for old in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(old.path());
        }
    }
    let copy = dir.join(format!(
        "{}-{n}-{}",
        std::process::id(),
        name.to_string_lossy()
    ));
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::copy(path, &copy)) {
        Ok(_) => copy,
        Err(_) => path.to_path_buf(),
    }
}

extern "C" fn find_rom(sha: *const u8, sha_len: usize, len: *mut usize) -> *const u8 {
    // SAFETY: the mod passes a string it owns.
    let sha = unsafe { std::slice::from_raw_parts(sha, sha_len) };
    match std::str::from_utf8(sha).ok().and_then(crate::find_rom) {
        Some(rom) => {
            let rom = rom.leak();
            // SAFETY: the mod passes somewhere to write the length.
            unsafe { *len = rom.len() };
            rom.as_ptr()
        }
        None => std::ptr::null(),
    }
}

extern "C" fn switch_game(
    sha: *const u8,
    sha_len: usize,
    data: *const u8,
    data_len: usize,
) -> bool {
    // SAFETY: the mod passes a string and bytes it owns.
    let (sha, data) = unsafe {
        (
            std::slice::from_raw_parts(sha, sha_len),
            std::slice::from_raw_parts(data, data_len),
        )
    };
    let Some(sha) = std::str::from_utf8(sha).ok() else {
        return false;
    };
    let Some((path, _)) = crate::find_rom_path(sha) else {
        println!("mod switch: no ROM with SHA-256 {sha}");
        return false;
    };
    println!("switch to {}", path.display());
    *NEXT_HANDOFF.lock().unwrap() = Some(data.to_vec());
    if crate::WINDOW.load(std::sync::atomic::Ordering::Relaxed) {
        // The window stops the game, as for a ROM picked in its menu.
        n64rusty_host::one_window::request(n64rusty_host::one_window::Next::Rom(path));
    } else {
        crate::os::stop();
    }
    true
}

extern "C" fn handoff(len: *mut usize) -> *const u8 {
    match HANDOFF.lock().unwrap().as_ref() {
        Some(h) => {
            // SAFETY: the mod passes somewhere to write the length.
            unsafe { *len = h.len() };
            // Kept until the next game's install, so valid for the run.
            h.as_ptr()
        }
        None => std::ptr::null(),
    }
}

extern "C" fn set_override(addr: u32, hook: Hook) {
    HOOKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(addr)
        .or_default()
        .push(hook);
}

/// The hooks on `addr`, copied: hooks call back in here.
fn chain(addr: u32) -> Vec<Hook> {
    HOOKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|h| h.get(&addr).cloned())
        .unwrap_or_default()
}

fn ctx(c: *mut c_void) -> &'static mut Ctx {
    // SAFETY: hooks only get the context the player handed them.
    unsafe { &mut *(c as *mut Ctx) }
}

/// From a hook on `addr`: the hook loaded before it, else the game's own.
extern "C" fn call_original(c: *mut c_void, addr: u32) {
    let below = STACK.with_borrow_mut(|s| match s.last_mut() {
        Some((a, i)) if *a == addr && *i > 0 => {
            *i -= 1;
            Some(*i)
        }
        _ => None,
    });
    let Some(i) = below else {
        return original(ctx(c), addr);
    };
    chain(addr)[i](c);
    // Back to the calling hook, which may call the original again.
    STACK.with_borrow_mut(|s| {
        if let Some(top) = s.last_mut() {
            top.1 = i + 1;
        }
    });
}

/// Runs what `addr` ran before the mods hooked it.
fn original(c: &mut Ctx, addr: u32) {
    let replaced = REPLACED
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|r| r.get(&addr).copied());
    match replaced {
        Some(f) => f(c),
        None => c.call_original(addr),
    }
}

extern "C" fn regs(c: *mut c_void) -> *mut u64 {
    ctx(c).r.as_mut_ptr()
}

/// The override every mod hook goes through.
fn dispatch(c: &mut Ctx) {
    let addr = c.last_call();
    let hooks = chain(addr);
    let Some(&last) = hooks.last() else {
        return original(c, addr);
    };
    STACK.with_borrow_mut(|s| s.push((addr, hooks.len() - 1)));
    last(c as *mut Ctx as *mut c_void);
    STACK.with_borrow_mut(|s| s.pop());
}

/// Routes every hooked address through [`dispatch`]; a hook on a replaced
/// system routine wraps the replacement, so this runs after those are in.
pub fn route(c: &mut Ctx) {
    for a in hooked().into_keys() {
        if let Some(f) = c.override_at(a) {
            REPLACED
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(a, f);
        }
        c.set_override(a, dispatch);
    }
}

/// The hooked addresses, with how many hooks each has.
fn hooked() -> HashMap<u32, usize> {
    HOOKS
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .map(|(a, h)| (*a, h.len()))
        .collect()
}

/// Drops hooks set since `before` was taken.
fn drop_hooks_since(before: &HashMap<u32, usize>) {
    if let Some(h) = HOOKS.lock().unwrap().as_mut() {
        for (a, hooks) in h.iter_mut() {
            hooks.truncate(before.get(a).copied().unwrap_or(0));
        }
        h.retain(|_, hooks| !hooks.is_empty());
    }
}

/// Runs each mod's install; keeps the active ones loaded for the run.
pub fn install(
    libs: Vec<(String, libloading::Library)>,
    c: &mut Ctx,
    cart: &mut Vec<u8>,
    sha256: &'static str,
) {
    // Hooks of the game played before in this program are not this game's.
    *HOOKS.lock().unwrap() = None;
    *REPLACED.lock().unwrap() = None;
    // A handoff is for the one game played next.
    *HANDOFF.lock().unwrap() = NEXT_HANDOFF.lock().unwrap().take();
    if libs.is_empty() {
        return;
    }
    let ram = c.mem.bytes_mut();
    let api: &'static Api = Box::leak(Box::new(Api {
        ram: ram.as_mut_ptr(),
        ram_len: ram.len(),
        mod_ram_len: crate::MOD_RAM,
        cart: cart.as_mut_ptr(),
        cart_len: cart.len(),
        sha256: sha256.as_ptr(),
        sha256_len: sha256.len(),
        find_rom,
        set_override,
        call_original,
        regs,
        switch_game,
        handoff,
    }));
    for (name, lib) in libs {
        // SAFETY: the mod API's documented signature.
        let Ok(f) = (unsafe { lib.get::<extern "C" fn(&'static Api) -> bool>(INSTALL) }) else {
            eprintln!("warning: mod {name} has no install function");
            continue;
        };
        let before = hooked();
        if f(api) {
            println!("mod {name}: on");
            std::mem::forget(lib);
        } else {
            // Its library unloads below: hooks it set must not outlive it.
            drop_hooks_since(&before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn hook(_: *mut c_void) {}

    /// The tests share the hook table: one at a time.
    static ONE: Mutex<()> = Mutex::new(());

    #[test]
    fn a_declined_mod_leaves_no_hooks() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        set_override(0x8000_1000, hook);
        let before = hooked();
        set_override(0x8000_2000, hook);
        drop_hooks_since(&before);
        let h = hooked();
        assert!(h.contains_key(&0x8000_1000) && !h.contains_key(&0x8000_2000));
    }

    static SEEN: Mutex<Vec<char>> = Mutex::new(Vec::new());
    const CHAINED: u32 = 0x8000_4000;

    fn seen(m: char) {
        SEEN.lock().unwrap().push(m);
    }
    extern "C" fn a(c: *mut c_void) {
        seen('A');
        call_original(c, CHAINED);
    }
    extern "C" fn b(c: *mut c_void) {
        seen('B');
        call_original(c, CHAINED);
    }
    extern "C" fn b_alone(_: *mut c_void) {
        seen('B');
    }

    /// Runs the game's call of [`CHAINED`] (its own function marks `o`)
    /// with `hooks` set in load order; returns the marks.
    fn run_chain(hooks: &[Hook]) -> Vec<char> {
        fn orig(_: &mut Ctx) {
            seen('o');
        }
        *HOOKS.lock().unwrap() = None;
        *REPLACED.lock().unwrap() = None;
        SEEN.lock().unwrap().clear();
        let mut c = Ctx::new(n64rusty_runtime::Memory::new(16));
        c.set_override(CHAINED, orig);
        for h in hooks {
            set_override(CHAINED, *h);
        }
        route(&mut c);
        c.call(CHAINED);
        std::mem::take(&mut *SEEN.lock().unwrap())
    }

    #[test]
    fn hooks_on_one_address_chain_in_load_order() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(run_chain(&[a, b]), ['B', 'A', 'o']);
    }

    #[test]
    fn a_hook_that_skips_the_original_ends_the_chain() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(run_chain(&[a, b_alone]), ['B']);
    }

    #[test]
    fn a_declined_mod_drops_only_its_own_hook() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        *HOOKS.lock().unwrap() = None;
        set_override(CHAINED, a);
        let before = hooked();
        set_override(CHAINED, b);
        drop_hooks_since(&before);
        assert_eq!(hooked()[&CHAINED], 1);
    }

    #[test]
    fn a_switch_to_an_unknown_rom_asks_for_nothing() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        *crate::ROM_PATH.lock().unwrap() = Some(std::env::temp_dir().join("none.z64"));
        *NEXT_HANDOFF.lock().unwrap() = None;
        let sha = "00".repeat(32);
        assert!(!switch_game(sha.as_ptr(), sha.len(), [1u8].as_ptr(), 1));
        assert!(NEXT_HANDOFF.lock().unwrap().is_none());
        assert!(n64rusty_host::one_window::take_request().is_none());
    }

    #[test]
    fn a_switch_hands_its_data_to_the_next_game_only() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("n64rusty-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The smallest cartridge a ROM reads as: big-endian magic, zeros.
        let mut cart = vec![0u8; 0x101000];
        cart[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let other = dir.join("other.z64");
        std::fs::write(&other, &cart).unwrap();
        let sha = n64rusty_rom::Rom::from_bytes(cart).unwrap().sha256;
        *crate::ROM_PATH.lock().unwrap() = Some(dir.join("played.z64"));
        crate::WINDOW.store(true, std::sync::atomic::Ordering::Relaxed);
        let data = b"OOT>\x01";
        let ok = switch_game(sha.as_ptr(), sha.len(), data.as_ptr(), data.len());
        crate::WINDOW.store(false, std::sync::atomic::Ordering::Relaxed);
        let asked = n64rusty_host::one_window::take_request();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(ok);
        assert_eq!(asked, Some(n64rusty_host::one_window::Next::Rom(other)));
        let mut c = Ctx::new(n64rusty_runtime::Memory::new(16));
        let mut len = 0;
        install(Vec::new(), &mut c, &mut Vec::new(), "");
        let p = handoff(&mut len);
        // SAFETY: the handoff just returned.
        assert_eq!(unsafe { std::slice::from_raw_parts(p, len) }, data);
        install(Vec::new(), &mut c, &mut Vec::new(), "");
        assert!(handoff(&mut len).is_null());
    }

    #[test]
    fn a_hook_on_a_replaced_routine_wraps_the_replacement() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        fn replacement(c: &mut Ctx) {
            c.r[2] = 7;
        }
        extern "C" fn wrap(c: *mut c_void) {
            call_original(c, 0x8000_3000);
            ctx(c).r[2] += 1;
        }
        let mut c = Ctx::new(n64rusty_runtime::Memory::new(16));
        c.set_override(0x8000_3000, replacement);
        set_override(0x8000_3000, wrap);
        route(&mut c);
        c.call(0x8000_3000);
        assert_eq!(c.r[2], 8);
    }
}
