//! The RC003 physical remote photo, embedded and decoded once.
//!
//! Source: the reference project's `public/RC003-remote-photo@2x.webp`
//! (404×820 PNG with transparent margins; the visible remote occupies
//! ≈46.3%×93.7% of the frame). Displayed at 250×508 points, matching the
//! reference mapping canvas geometry (see docs in `views.rs`).

use rune::Pixels;

thread_local! {
    static PHOTO: Pixels = {
        let bytes: &[u8] = include_bytes!("../assets/rc003-remote@2x.png");
        let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .expect("embedded rc003 photo is a valid png")
            .to_rgba8();
        let (width, height) = decoded.dimensions();
        let pixels = Pixels::new();
        pixels.set(width, height, decoded.into_raw());
        pixels
    };
}

/// The shared remote photo. Clone-per-call is cheap (an Arc handle); pass a
/// reference to the element: `el().pixels(&remote_photo())`.
pub fn remote_photo() -> Pixels {
    PHOTO.with(|p| p.clone())
}
