//! Embeds `assets/app-icon.ico` as the Windows executable's icon, so Explorer,
//! shortcuts and the taskbar show the same artwork as the window and tray.
//!
//! Runs only when the *target* is Windows: host tests on Linux skip it. The
//! icon itself comes from `assets/app-icon.svg` (see `src/icon.rs`).

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/app-icon.ico");
    res.compile().expect("failed to embed the Windows app icon");
}
