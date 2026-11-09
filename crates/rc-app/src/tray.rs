//! Tray icon via `rune::tray` (Shell_NotifyIcon on Windows). Best effort:
//! platforms without a tray (or without a tray host running) just skip it.
//!
//! Autostart (registry Run key or Startup folder shortcut) is a TODO on
//! purpose: it needs either a registry dependency or hand-rolled FFI that
//! is not worth pulling for V1. See the final delivery notes.

use rune::tray::{Tray, TrayEvent, TrayIcon, TrayItem, TraySpec};
use rune::AppHandle;

use crate::i18n::{tr, Lang};
use crate::icon;
use crate::ui::AppState;

/// Keep the tray alive for the app's lifetime: dropping `Tray` removes the
/// icon. `Tray::new` spawns its own thread, so the app's loop is untouched.
/// The menu is built once at startup in the startup language; switching the
/// UI language later rebuilds the window live but the tray only on restart.
static TRAY: std::sync::OnceLock<Tray> = std::sync::OnceLock::new();

/// The menu's items: stable action ids (`show`, `quit`) with labels in the
/// UI's language, so the click handler never depends on the label text.
pub fn menu_items(lang: Lang) -> Vec<TrayItem> {
    vec![
        TrayItem::action("show", tr(lang, "Show Remote Mic Rune")),
        TrayItem::Separator,
        TrayItem::action("quit", tr(lang, "Quit")),
    ]
}

pub fn spawn(handle: AppHandle<AppState>, lang: Lang) {
    // The same artwork as the window and the exe, at the tray's native
    // 32 px; Windows scales it to the taskbar's DPI.
    let icon = TrayIcon::rgba(32, 32, icon::tray_rgba(32));
    let spec = TraySpec::new("RemoteMicRune", "Remote Mic Rune", icon)
        .tooltip("Remote Mic Rune")
        .menu(menu_items(lang));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_items_carry_stable_ids() {
        let items = menu_items(Lang::En);
        let show = &items[0];
        let quit = &items[2];
        assert_eq!(
            *show,
            TrayItem::Action { id: "show".into(), label: "Show Remote Mic Rune".into(), enabled: true }
        );
        assert_eq!(*quit, TrayItem::Action { id: "quit".into(), label: "Quit".into(), enabled: true });
        // Chinese labels translate; the ids the click handler switches on
        // stay put.
        let zh = menu_items(Lang::Zh);
        assert_eq!(
            zh[0],
            TrayItem::Action {
                id: "show".into(),
                label: "显示 Remote Mic Rune".into(),
                enabled: true
            }
        );
        assert_eq!(
            zh[2],
            TrayItem::Action {
                id: "quit".into(),
                label: "退出".into(),
                enabled: true
            }
        );
    }
}
