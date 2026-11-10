//! One icon for every surface: the window/taskbar, the tray, and the Windows
//! executable resource.
//!
//! `assets/app-icon.png` (1024 px) is the master artwork, embedded here for
//! Rune and decoded for the tray. `app-icon.ico` (16–256 px) is generated
//! from it and compiled into `rc-app.exe` by `build.rs`, so every surface
//! shows the same remote.

/// The 1024 px master PNG. `build.rs` embeds the ICO; Rune takes the PNG.
pub const PNG: &[u8] = include_bytes!("../assets/app-icon.png");

/// The window, taskbar and Alt+Tab icon. Rune applies it to windows opened
/// after this call; call it before `App::new`/`run`. A no-op outside
/// Windows and macOS.
pub fn install() {
    let _ = rune::set_app_icon(PNG.to_vec());
}

/// The same artwork as straight-alpha RGBA at `size` px, for the tray.
pub fn tray_rgba(size: u32) -> Vec<u8> {
    let decoded = image::load_from_memory(PNG)
        .expect("embedded app icon is a valid png")
        .to_rgba8();
    image::imageops::resize(&decoded, size, size, image::imageops::FilterType::Lanczos3).into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_square_and_resizes_to_the_tray() {
        let decoded = image::load_from_memory(PNG).expect("png").to_rgba8();
        let (w, h) = decoded.dimensions();
        assert_eq!(w, h, "the app icon must be square");
        let tray = tray_rgba(32);
        assert_eq!(tray.len(), 32 * 32 * 4);
        assert!(tray.iter().skip(3).step_by(4).any(|&a| a == 255), "the tile is opaque");
        assert!(tray[3] < 16, "the rounded corner stays transparent");
    }
}
