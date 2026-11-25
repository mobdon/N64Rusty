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
    ctx(c).call_original(addr);
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
        None => c.call_original(addr),
    }
}

/// Runs each mod's install; keeps the active ones loaded for the run.
pub fn install(
    libs: Vec<(String, libloading::Library)>,
    c: &mut Ctx,
    cart: &mut Vec<u8>,
    sha256: &'static str,
) {
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
        if f(api) {
            println!("mod {name}: on");
            std::mem::forget(lib);
        }
    }
    let hooks: Vec<u32> = HOOKS
        .lock()
        .unwrap()
        .iter()
        .flat_map(|h| h.keys().copied())
        .collect();
    for a in hooks {
        c.set_override(a, dispatch);
    }
}
