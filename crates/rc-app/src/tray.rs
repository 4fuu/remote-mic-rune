//! Tray icon via `rune::tray` (Shell_NotifyIcon on Windows). Best effort:
//! platforms without a tray (or without a tray host running) just skip it.
//!
//! Autostart (registry Run key or Startup folder shortcut) is a TODO on
//! purpose: it needs either a registry dependency or hand-rolled FFI that
//! is not worth pulling for V1. See the final delivery notes.

use rune::tray::{Tray, TrayEvent, TrayIcon, TrayItem, TraySpec};
use rune::AppHandle;

use crate::ui::AppState;

/// Keep the tray alive for the app's lifetime: dropping `Tray` removes the
/// icon. `Tray::new` spawns its own thread, so the app's loop is untouched.
static TRAY: std::sync::OnceLock<Tray> = std::sync::OnceLock::new();

pub fn spawn(handle: AppHandle<AppState>) {
    let icon = icon_rgba(24, 24);
    let spec = TraySpec::new("RemoteMicRune", "Remote Mic Rune", icon)
        .tooltip("Remote Mic Rune")
        .menu(vec![
            TrayItem::action("show", "Show Remote Mic Rune"),
            TrayItem::Separator,
            TrayItem::action("quit", "Quit"),
        ]);
    match Tray::new(spec, move |event: TrayEvent| {
        match event {
            // Primary click: bring the window up, focused.
            TrayEvent::Activate { .. } => {
                handle.show();
                handle.focus();
            }
            TrayEvent::Menu(id) => match id.as_str() {
                "show" => {
                    handle.show();
                    handle.focus();
                }
                "quit" => handle.quit(),
                _ => {}
            },
            // TrayEvent is non-exhaustive (pre-release API): ignore additions.
            _ => {}
        }
    }) {
        Ok(tray) => {
            // Ignore a failed install: the static only ever holds one tray.
            let _ = TRAY.set(tray);
        }
        Err(err) => eprintln!("rc-app: tray icon unavailable: {err}"),
    }
}

/// A simple painted icon: rounded dark square, light mic capsule and stand.
/// Straight (not premultiplied) RGBA rows, as `TrayIcon` requires.
fn icon_rgba(width: u32, height: u32) -> TrayIcon {
    TrayIcon::rgba(width, height, icon_pixels(width, height))
}

/// The icon's pixels, kept separate from the TrayIcon wrapper so tests can
/// read them (`TrayIcon`'s fields are crate-private).
fn icon_pixels(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let put = |rgba: &mut [u8], x: u32, y: u32, (r, g, b, a): (u8, u8, u8, u8)| {
        let index = ((y * width + x) * 4) as usize;
        rgba[index..index + 4].copy_from_slice(&[r, g, b, a]);
    };
    let inside = |x: u32, y: u32| x < width && y < height;

    let w = width as f32;
    let h = height as f32;
    for y in 0..height {
        for x in 0..width {
            // Rounded-rect background (2 pt corner radius on a 24 pt icon).
            let cx = x.min(width - 1 - x) as f32 + 0.5;
            let cy = y.min(height - 1 - y) as f32 + 0.5;
            let corner = (2.0_f32 - cx.min(2.0)).max(0.0).hypot((2.0_f32 - cy.min(2.0)).max(0.0));
            if corner > 2.0 {
                continue;
            }
            // Coverage softening near the corner edge.
            let alpha = ((2.0 - corner) / 1.0).clamp(0.0, 1.0);
            let a = (alpha * 255.0) as u8;

            // Mic capsule: ellipse centred at (w/2, h*0.38), radii (4, 6).
            let dx = (x as f32 + 0.5 - w / 2.0) / 4.0;
            let dy = (y as f32 + 0.5 - h * 0.38) / 6.0;
            let capsule = dx * dx + dy * dy <= 1.0;
            // Cradle arc: half circle radius 7 centred at (w/2, h*0.40).
            let cradle_dx = x as f32 + 0.5 - w / 2.0;
            let cradle_dy = y as f32 + 0.5 - h * 0.40;
            let cradle_r = (cradle_dx * cradle_dx + cradle_dy * cradle_dy).sqrt();
            let cradle = (cradle_r - 7.0).abs() < 0.9 && cradle_dy > -1.0;
            // Stem and base.
            let stem = (cradle_dx.abs() < 1.0) && y + 2 >= h as u32 * 55 / 100 && y + 2 <= h as u32 * 62 / 100;
            let base = (y + 2 == h as u32 * 66 / 100) && cradle_dx.abs() <= 4.0;

            let ink = capsule || cradle || stem || base;
            if ink && inside(x, y) {
                put(&mut rgba, x, y, (236, 240, 244, a));
            } else if inside(x, y) {
                put(&mut rgba, x, y, (30, 34, 40, a));
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_exactly_width_times_height_times_four_bytes() {
        let icon = icon_pixels(24, 24);
        assert_eq!(icon.len(), 24 * 24 * 4);
        // Some pixels are opaque (the icon body) and corners stay transparent.
        assert!(icon.iter().skip(3).step_by(4).any(|&a| a == 255), "body pixels are opaque");
        assert_eq!(icon[3], 0, "the top-left corner is transparent");
    }

    #[test]
    fn tray_menu_items_carry_stable_ids() {
        let items = vec![
            TrayItem::action("show", "Show Remote Mic Rune"),
            TrayItem::Separator,
            TrayItem::action("quit", "Quit"),
        ];
        let show = &items[0];
        let quit = &items[2];
        assert_eq!(
            *show,
            TrayItem::Action { id: "show".into(), label: "Show Remote Mic Rune".into(), enabled: true }
        );
        assert_eq!(*quit, TrayItem::Action { id: "quit".into(), label: "Quit".into(), enabled: true });
    }
}
