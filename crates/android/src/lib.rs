//! The N64Rusty player as an Android app: the same start screen, games,
//! settings and built-in game data as `n64rusty-play`, which it builds in.
//! ROMs, saves and settings live in the app's files folder on the phone
//! (`Android/data/dev.n64rusty.player/files`).

#![cfg(target_os = "android")]

#[allow(dead_code)]
#[path = "../../runner/src/bin/n64rusty-play.rs"]
mod player;

use n64rusty_host::android::AndroidApp;

#[no_mangle]
fn android_main(app: AndroidApp) {
    let home = app
        .external_data_path()
        .or_else(|| app.internal_data_path())
        .unwrap_or_default();
    let _ = std::fs::create_dir_all(&home);
    // Messages go to the phone's log (logcat, tag RustStdoutStderr).
    eprintln!("n64rusty: files in {}", home.display());
    n64rusty_host::android::start(app, home.clone());
    let code = player::start(Vec::new(), &home);
    eprintln!("n64rusty: player ended: {code:?}");
}
