//! Mods: shared libraries in the `mods` folder next to the program (see
//! `n64rusty-mod-api`). Each one checks the ROM hash itself; one built for
//! another API version is skipped.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use n64rusty_mod_api::{Api, Hook, INSTALL, VERSION, VERSION_FN};
use n64rusty_runtime::Ctx;

/// Mod hooks by the guest address they replace.
static HOOKS: Mutex<Option<HashMap<u32, Hook>>> = Mutex::new(None);

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

extern "C" fn set_override(addr: u32, hook: Hook) {
    HOOKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(addr, hook);
}

fn ctx(c: *mut c_void) -> &'static mut Ctx {
    // SAFETY: hooks only get the context the player handed them.
    unsafe { &mut *(c as *mut Ctx) }
}

extern "C" fn call_original(c: *mut c_void, addr: u32) {
    original(ctx(c), addr);
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
    let hook = HOOKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|h| h.get(&addr).copied());
    match hook {
        Some(h) => h(c as *mut Ctx as *mut c_void),
        None => original(c, addr),
    }
}

/// Routes every hooked address through [`dispatch`]; a hook on a replaced
/// system routine wraps the replacement, so this runs after those are in.
pub fn route(c: &mut Ctx) {
    for a in hooked() {
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

fn hooked() -> Vec<u32> {
    HOOKS
        .lock()
        .unwrap()
        .iter()
        .flat_map(|h| h.keys().copied())
        .collect()
}

/// Drops hooks set since `before` was taken.
fn drop_hooks_since(before: &[u32]) {
    if let Some(h) = HOOKS.lock().unwrap().as_mut() {
        h.retain(|a, _| before.contains(a));
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
        assert!(h.contains(&0x8000_1000) && !h.contains(&0x8000_2000));
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
