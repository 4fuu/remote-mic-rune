//! The Rune UI: one window with five sections (Status, Capture, Buttons,
//! Helper, Settings). All state changes funnel through [`AppState`] methods
//! so they are testable headlessly; the view only places elements.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rune::{AppHandle, TextEdit, Tx};

use rc_protocol::{ALL_BUTTONS, RemoteButton};

use crate::bridge_api::{BridgeStatus, SharedBridge, SharedEngine, UiNotice};
use crate::i18n::{tr, tr_fmt, Lang, Language};
use crate::mappings::{ButtonAction, MediaKey, MouseButton};
use crate::settings::AppSettings;
use crate::task::TaskStatus;

/// The action picker's options (V1 editor) in the UI's language. Media keys,
/// shortcut and mouse clicks; `MouseMove`/`Scroll` actions can only arrive
/// from hand-edited settings files and display under their own short labels
/// (see `ButtonAction` matching in `action_short_label`).
///
/// The ORDER is the stable action identity: the kit hands back the chosen
/// display string, which [`AppState::choose_action`] looks up in the same
/// list it was shown from, so translating the labels cannot break selection.
pub fn picker_options(lang: Lang) -> [&'static str; 11] {
    [
        tr(lang, "Disabled"),
        tr(lang, "Media: Mute"),
        tr(lang, "Media: Vol -"),
        tr(lang, "Media: Vol +"),
        tr(lang, "Media: Play"),
        tr(lang, "Media: Prev"),
        tr(lang, "Media: Next"),
        tr(lang, "Shortcut"),
        tr(lang, "Mouse: Left"),
        tr(lang, "Mouse: Right"),
        tr(lang, "Mouse: Middle"),
    ]
}

/// The action [`picker_options`]' index stands for. Index 0 (`Disabled`)
/// maps to None: the action is cleared. Indexes past the list cannot occur
/// from a picker (the kit only hands back shown options), and callers treat
/// unknown choices as "leave the mapping untouched".
pub fn action_for_picker_index(index: usize) -> Option<ButtonAction> {
    match index {
        1 => Some(ButtonAction::MediaKey {
            key: MediaKey::Mute,
        }),
        2 => Some(ButtonAction::MediaKey {
            key: MediaKey::VolumeDown,
        }),
        3 => Some(ButtonAction::MediaKey {
            key: MediaKey::VolumeUp,
        }),
        4 => Some(ButtonAction::MediaKey {
            key: MediaKey::PlayPause,
        }),
        5 => Some(ButtonAction::MediaKey {
            key: MediaKey::Prev,
        }),
        6 => Some(ButtonAction::MediaKey {
            key: MediaKey::Next,
        }),
        7 => Some(ButtonAction::Shortcut { chord: Vec::new() }),
        8 => Some(ButtonAction::MouseClick {
            button: MouseButton::Left,
        }),
        9 => Some(ButtonAction::MouseClick {
            button: MouseButton::Right,
        }),
        10 => Some(ButtonAction::MouseClick {
            button: MouseButton::Middle,
        }),
        _ => None,
    }
}

/// The window's sections, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Status,
    Capture,
    Buttons,
    Helper,
    Settings,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::Status,
        Section::Capture,
        Section::Buttons,
        Section::Helper,
        Section::Settings,
    ];

    /// Display labels in the UI's language, in [`Section::ALL`] order (the
    /// segmented control's pick callback is index-based, so the tabs' action
    /// identity survives any wording).
    pub fn labels(lang: Lang) -> [&'static str; 5] {
        [
            tr(lang, "Status"),
            tr(lang, "Capture"),
            tr(lang, "Buttons"),
            tr(lang, "Helper"),
            tr(lang, "Settings"),
        ]
    }

    pub fn index(self) -> usize {
        Section::ALL.iter().position(|s| *s == self).expect("ALL covers every section")
    }

    pub fn from_index(index: usize) -> Section {
        Section::ALL.get(index).copied().unwrap_or(Section::Status)
    }
}

/// Bridge and engine handles the UI actions call into. None in headless
/// state-level tests that do not exercise the services.
#[derive(Clone, Default)]
pub struct Services {
    pub bridge: Option<SharedBridge>,
    pub engine: Option<SharedEngine>,
}

/// What main.rs hands the app before `init()` runs (`fn() -> S` cannot take
/// arguments, so startup data travels through a OnceLock).
pub struct Startup {
    pub settings: AppSettings,
    pub settings_dir: Option<PathBuf>,
    pub load_warning: Option<crate::settings::LoadWarning>,
    pub launch_error: Option<String>,
    pub bridge_status: BridgeStatus,
    pub services: Services,
    pub ui: Option<AppHandle<AppState>>,
}

static STARTUP: std::sync::OnceLock<Startup> = std::sync::OnceLock::new();

/// Called by main before the app runs.
pub fn set_startup(startup: Startup) {
    if STARTUP.set(startup).is_err() {
        panic!("startup data set more than once");
    }
}

fn take_startup() -> Startup {
    // The OnceLock may be unset in tests; fall back to defaults.
    let startup = STARTUP.get_or_init(|| Startup {
        settings: AppSettings::default(),
        settings_dir: None,
        load_warning: None,
        launch_error: None,
        bridge_status: BridgeStatus::default(),
        services: Services::default(),
        ui: None,
    });
    Startup {
        settings: startup.settings.clone(),
        settings_dir: startup.settings_dir.clone(),
        load_warning: startup.load_warning.clone(),
        launch_error: startup.launch_error.clone(),
        bridge_status: startup.bridge_status.clone(),
        services: startup.services.clone(),
        ui: startup.ui.clone(),
    }
}

/// Helper task commands the Helper section's buttons run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperCommand {
    Install,
    Remove,
    Start,
    Stop,
    RefreshStatus,
}

/// The app state: settings plus live UI state.
pub struct AppState {
    pub settings: AppSettings,
    /// Where settings.json lives (None: persistence off, e.g. headless tests).
    pub settings_dir: Option<PathBuf>,
    /// The resolved UI language (settings.language with `System` resolved),
    /// recomputed when the setting changes; every render reads it.
    pub lang: Lang,
    /// Active section tab.
    pub section: Section,
    /// Corruption note from loading; shown until dismissed.
    pub warning: Option<String>,
    /// Set when the bridge could not start (another instance, pipe error):
    /// the window shows the reason and an Exit button.
    pub launch_error: Option<String>,
    pub bridge: BridgeStatus,
    /// Pressed-state mask by button ordinal, from pump notices.
    pub pressed: u64,
    pub services: Services,
    pub ui: Option<AppHandle<AppState>>,
    /// Which action picker popover is open (its key), one at a time.
    pub open_picker: Option<String>,
    /// Shortcut chord drafts per button, created when the button's action is
    /// `Shortcut`; editing parses and persists immediately.
    pub shortcut_edits: BTreeMap<RemoteButton, TextEdit>,
    /// Parse errors per button, shown under the draft field.
    pub chord_errors: BTreeMap<RemoteButton, String>,
    /// Latest helper task status (`--task-status` / schtasks fallback).
    pub task_status: Option<TaskStatus>,
    /// Last helper command result line.
    pub task_note: Option<String>,
    /// A helper command is running; buttons are disabled meanwhile.
    pub task_busy: bool,
    /// Last settings save failure, shown in the footer.
    pub save_error: Option<String>,
    /// The Buttons mapping canvas' laid-out width in points, fed back by the
    /// canvas element (`El::on_drawn_layout`) so the view can scale the
    /// spatial layout to narrow windows. Starts at the design width.
    pub buttons_canvas_w: f32,
}

impl AppState {
    /// State for headless state tests: no services, no persistence. The
    /// language is forced — `System` and `En` both render English, an
    /// explicit `Zh` renders Chinese — so no test depends on the host
    /// locale.
    #[cfg(test)]
    pub fn for_test(settings: AppSettings) -> AppState {
        let mut state = AppState {
            lang: match settings.language {
                Language::Zh => Lang::Zh,
                _ => Lang::En,
            },
            settings,
            settings_dir: None,
            section: Section::Status,
            warning: None,
            launch_error: None,
            bridge: BridgeStatus::default(),
            pressed: 0,
            services: Services::default(),
            ui: None,
            open_picker: None,
            shortcut_edits: BTreeMap::new(),
            chord_errors: BTreeMap::new(),
            task_status: None,
            task_note: None,
            task_busy: false,
            save_error: None,
            buttons_canvas_w: crate::views::CANVAS_BASE_W,
        };
        // Faithful to startup: preconfigured shortcuts get draft fields.
        state.seed_shortcut_drafts();
        state
    }

    /// Build from the [`Startup`] handoff.
    pub fn from_startup() -> AppState {
        let startup = take_startup();
        let lang = startup.settings.language.resolve();
        let mut state = AppState {
            lang,
            settings: startup.settings,
            settings_dir: startup.settings_dir,
            section: Section::Status,
            warning: startup.load_warning.map(|w| w.message(lang)),
            launch_error: startup.launch_error,
            bridge: startup.bridge_status,
            pressed: 0,
            services: startup.services,
            ui: startup.ui,
            open_picker: None,
            shortcut_edits: BTreeMap::new(),
            chord_errors: BTreeMap::new(),
            task_status: None,
            task_note: None,
            task_busy: false,
            save_error: None,
            buttons_canvas_w: crate::views::CANVAS_BASE_W,
        };
        // Existing shortcut mappings get drafts so their fields show at once.
        state.seed_shortcut_drafts();
        state
    }

    /// Translate one UI string in this app's language (the English source
    /// text is the key).
    pub(crate) fn tr(&self, key: &'static str) -> &'static str {
        tr(self.lang, key)
    }

    /// Pre-creates a shortcut draft for every button already holding a
    /// `Shortcut` action, so its text field shows the chord at once.
    fn seed_shortcut_drafts(&mut self) {
        let mut existing: Vec<(RemoteButton, Vec<crate::mappings::VKey>)> = Vec::new();
        for button in ALL_BUTTONS {
            if let Some(ButtonAction::Shortcut { chord }) = self.settings.mappings.action_for(button) {
                existing.push((button, chord.clone()));
            }
        }
        for (button, chord) in existing {
            self.shortcut_edits
                .entry(button)
                .or_insert_with(|| TextEdit::new(crate::chord::format_chord(&chord)));
        }
    }

    /// Apply one pump notice.
    pub fn apply_notice(&mut self, notice: UiNotice) {
        match notice {
            UiNotice::Status(status) => self.bridge = status,
            UiNotice::PressedChanged(mask) => self.pressed = mask,
            UiNotice::Fatal(message) => self.launch_error = Some(message),
        }
    }

    /// Persist settings to disk, remembering failures for the footer.
    pub fn persist(&mut self) {
        let Some(dir) = self.settings_dir.clone() else { return };
        match crate::settings::save(&dir, &self.settings) {
            Ok(()) => self.save_error = None,
            Err(why) => self.save_error = Some(why.message(self.lang)),
        }
    }

    /// After any mappings change: persist, push the new table to the engine
    /// and recompute the capture set (mapped_mask = mapped buttons).
    fn after_mappings_change(&mut self) {
        self.persist();
        if let Some(engine) = &self.services.engine {
            engine.replace_mappings(&self.settings.mappings);
        }
        self.push_capture_targets();
    }

    /// The capture set recompute: enabled flag plus the mapped-button mask.
    fn push_capture_targets(&mut self) {
        if let Some(bridge) = &self.services.bridge {
            bridge.set_capture_targets(self.settings.capture_enabled, self.settings.mappings.mapped_mask());
        }
    }

    pub fn set_capture_enabled(&mut self, enabled: bool) {
        self.settings.capture_enabled = enabled;
        self.persist();
        self.push_capture_targets();
    }

    pub fn set_voice_synth(&mut self, to: Option<u16>) {
        self.settings.voice_synth_to = to;
        self.persist();
        if let Some(bridge) = &self.services.bridge {
            bridge.set_voice_synth(to);
        }
    }

    /// The Settings section's language picker: re-resolve the render
    /// language, persist, and let the next frame redraw (no restart).
    pub fn set_language(&mut self, language: Language) {
        self.settings.language = language;
        self.lang = language.resolve();
        self.persist();
    }

    /// One picker choose: update the button's action, keep the shortcut
    /// draft in sync, persist and push everything downstream. `choice` is
    /// the display string of an option in [`picker_options`] (the kit hands
    /// it back); its position in this language's list picks the action, so an
    /// unknown or stale choice leaves the mapping untouched.
    pub fn choose_action(&mut self, button: RemoteButton, choice: &str) {
        let Some(index) = picker_options(self.lang).iter().position(|o| *o == choice) else {
            return;
        };
        match action_for_picker_index(index) {
            None => {
                self.settings.mappings.set_action(button, None);
                self.shortcut_edits.remove(&button);
                self.chord_errors.remove(&button);
            }
            Some(ButtonAction::Shortcut { .. }) => {
                let chord = match self.settings.mappings.action_for(button) {
                    Some(ButtonAction::Shortcut { chord }) => chord.clone(),
                    _ => Vec::new(),
                };
                self.shortcut_edits
                    .entry(button)
                    .or_insert_with(|| TextEdit::new(crate::chord::format_chord(&chord)));
                self.chord_errors.remove(&button);
                self.settings
                    .mappings
                    .set_action(button, Some(ButtonAction::Shortcut { chord }));
            }
            Some(action) => {
                self.settings.mappings.set_action(button, Some(action));
                self.shortcut_edits.remove(&button);
                self.chord_errors.remove(&button);
            }
        }
        self.after_mappings_change();
    }

    /// The shortcut draft field changed: parse and persist when valid, flag
    /// the error (leaving the last valid chord in place) when not.
    pub fn shortcut_edited(&mut self, button: RemoteButton) {
        let Some(edit) = self.shortcut_edits.get(&button) else { return };
        let draft = edit.text.clone();
        match crate::chord::parse_chord(self.lang, &draft) {
            Ok(chord) => {
                self.chord_errors.remove(&button);
                self.settings
                    .mappings
                    .set_action(button, Some(ButtonAction::Shortcut { chord }));
                self.after_mappings_change();
            }
            Err(why) => {
                self.chord_errors.insert(button, why);
            }
        }
    }

    pub fn set_dark(&mut self, dark: Option<bool>) {
        self.settings.window.dark = dark;
        self.persist();
    }

    /// Reload settings.json from disk (the Settings section's button). The
    /// file may carry a different language, so the render language resolves
    /// from what loaded.
    pub fn reload_settings(&mut self) {
        let Some(dir) = self.settings_dir.clone() else { return };
        let loaded = crate::settings::load(&dir);
        self.settings = loaded.settings;
        self.lang = self.settings.language.resolve();
        self.warning = loaded.warning.map(|w| w.message(self.lang));
        self.after_mappings_change();
        self.set_voice_synth(self.settings.valid_voice_synth());
    }

    /// Run a helper task command on a worker thread; results land via
    /// `task_command_done` on the UI thread.
    pub fn run_helper_command(&mut self, command: HelperCommand) {
        if self.task_busy {
            return;
        }
        self.task_busy = true;
        self.task_note = None;
        let lang = self.lang;
        let Some(ui) = self.ui.clone() else {
            self.task_busy = false;
            self.task_note =
                Some(tr(lang, "No UI handle: helper commands need the windowed app").to_string());
            return;
        };
        std::thread::spawn(move || {
            let outcome = run_helper_blocking(command, lang);
            ui.update(move |s: &mut AppState, _: &mut Tx| s.task_command_done(outcome));
        });
    }

    /// Worker result lands here on the UI thread.
    pub fn task_command_done(&mut self, outcome: HelperOutcome) {
        self.task_busy = false;
        self.task_note = Some(outcome.note);
        if let Some(status) = outcome.status {
            self.task_status = Some(status);
        }
    }
}

/// What a helper command reports back to the UI thread. The status query
/// runs on the worker thread too, so no subprocess blocks the UI.
pub struct HelperOutcome {
    pub note: String,
    pub status: Option<TaskStatus>,
}

/// One helper command, run to completion (worker thread context). Always
/// follows up with a status query so the screen stays honest.
fn run_helper_blocking(command: HelperCommand, lang: Lang) -> HelperOutcome {
    let note = match command {
        HelperCommand::Install => crate::task::helper_path(lang)
            .and_then(|helper| crate::task::install_task(lang, &helper))
            .map(|()| tr(lang, "Helper task installed (UAC answered)").to_string()),
        HelperCommand::Remove => crate::task::helper_path(lang)
            .and_then(|helper| crate::task::remove_task(lang, &helper))
            .map(|()| tr(lang, "Helper task removed").to_string()),
        HelperCommand::Start => crate::task::start_task(lang).and_then(|out| {
            if out.ok() {
                Ok(tr(lang, "Helper start requested").to_string())
            } else {
                Err(tr_fmt!(lang, "schtasks /run failed: {}", first_line(lang, &out.stderr, &out.stdout)))
            }
        }),
        HelperCommand::Stop => crate::task::stop_task(lang).and_then(|out| {
            if out.ok() {
                Ok(tr(
                    lang,
                    "Helper stop requested (it exits on disconnect by design; the bridge keeps running)",
                )
                .to_string())
            } else {
                Err(tr_fmt!(lang, "schtasks /end failed: {}", first_line(lang, &out.stderr, &out.stdout)))
            }
        }),
        HelperCommand::RefreshStatus => Ok(String::new()),
    };
    let (mut note, failed) = match note {
        Ok(note) => (note, false),
        Err(why) => (why, true),
    };
    let status = if failed {
        None
    } else {
        match crate::task::query_task_status(lang) {
            Ok(status) => {
                if note.is_empty() {
                    note = status.label(lang);
                }
                Some(status)
            }
            Err(why) => {
                if note.is_empty() {
                    note = why;
                }
                None
            }
        }
    };
    HelperOutcome { note, status }
}

fn first_line(lang: Lang, a: &str, b: &str) -> String {
    let source = if a.trim().is_empty() { b } else { a };
    source
        .lines()
        .next()
        .unwrap_or(tr(lang, "(no output)"))
        .chars()
        .take(120)
        .collect()
}

// The view layer lives in `views.rs` (SwiftUI-style inset-grouped form).
pub use crate::views::view;

// ---------------------------------------------------------------------------
// Tests: pure state transitions plus headless runtime renders (they need a
// GPU, so they skip unless RC_APP_UI_TESTS=1; the orb has lavapipe).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::bridge_api::{BridgePhase, StartError};
    use crate::mappings::{ButtonMappings, VKey, vk};
    use rc_protocol::RemoteButton;

    fn ui_enabled() -> bool {
        match std::env::var("RC_APP_UI_TESTS") {
            Ok(v) => v != "0",
            Err(_) => false,
        }
    }

    #[test]
    fn section_snapshots() {
        let Some(dir) = std::env::var_os("RC_APP_SNAP_DIR").map(std::path::PathBuf::from) else {
            return;
        };
        // The snapshots exist to verify the localized UI: Chinese unless a
        // run asks for English (RC_APP_SNAP_LANG=en) for comparison.
        let snapshot_lang = match std::env::var("RC_APP_SNAP_LANG").as_deref() {
            Ok("en") => Language::En,
            _ => Language::Zh,
        };
        std::fs::create_dir_all(&dir).unwrap();
        use rune::render::{Gpu, OFFSCREEN_FORMAT, Offscreen, Renderer, write_png};

        let gpu = Gpu::headless();
        let mut settings = settings_with_mappings();
        settings.window.dark = Some(false); // the SwiftUI-style light look
        settings.capture_enabled = true;
        settings.language = snapshot_lang;
        // `buttons` renders narrow (the scale-down regression case) and
        // `buttons-wide` at the window the app now opens at: cards pin to the
        // content edges, the photo stays centered, sizes stay at scale 1.
        for (name, section, win_w) in [
            ("status", Section::Status, 780.0),
            ("capture", Section::Capture, 780.0),
            ("buttons", Section::Buttons, 780.0),
            ("helper", Section::Helper, 780.0),
            ("settings", Section::Settings, 780.0),
            ("buttons-wide", Section::Buttons, 1180.0),
        ] {
            let mut state = AppState::for_test(settings.clone());
            state.section = section;
            state.bridge = BridgeStatus {
                phase: BridgePhase::Listening,
                helper_pid: Some(4812),
                generation: 3,
                ..BridgeStatus::default()
            };
            // The mapping canvas is fixed geometry: render it in a window
            // tall enough to show the whole canvas (intro, the canvas itself,
            // and the volume_mute card below it).
            let win_h = if section == Section::Buttons {
                crate::views::mapping_canvas_h(&state) + 290.0
            } else {
                720.0
            };
            let (w, h) = ((win_w * 2.0) as u32, (win_h * 2.0) as u32); // 2x scale
            let mut rt = rune::Runtime::new(state, view, (win_w, win_h), rune::Env::new(false, 2.0));
            rt.frame(0.0);
            // The canvas' laid-out width feeds back into state between
            // frames; land the re-laid-out spatial geometry before drawing.
            rt.frame(1.0 / 60.0);
            rt.settle_all();
            rt.frame(1.0 / 60.0);
            let mut renderer = Renderer::new(&gpu.device, OFFSCREEN_FORMAT);
            renderer.prepare(&gpu.device, &gpu.queue, &mut rt.text.atlas, &rt.instances, (w, h));
            renderer.upload_images(&gpu.device, &gpu.queue, &mut rt.images);
            let target = Offscreen::new(&gpu.device, w, h);
            let mut enc = gpu.device.create_command_encoder(&Default::default());
            renderer.draw(&mut enc, &target.view, [0.95, 0.95, 0.96, 1.0]);
            target.copy(&mut enc);
            gpu.queue.submit([enc.finish()]);
            let out = target.read(&gpu.device);
            write_png(&dir.join(format!("v2-{name}.png")), w, h, &out).unwrap();
        }
    }

    fn mappings_with_shortcut() -> ButtonMappings {
        let mut m = ButtonMappings::default();
        m.set_action(
            RemoteButton::Home,
            Some(ButtonAction::Shortcut { chord: vec![vk::CONTROL, vk::F5] }),
        );
        m
    }

    fn settings_with_mappings() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.capture_enabled = true;
        settings.mappings = mappings_with_shortcut();
        settings
    }

    // -- pure state transitions ---------------------------------------------

    #[test]
    fn picker_options_round_trip_to_actions() {
        // The option order is the action identity, in every language.
        assert_eq!(action_for_picker_index(0), None, "Disabled clears the action");
        assert_eq!(
            action_for_picker_index(7),
            Some(ButtonAction::Shortcut { chord: vec![] })
        );
        assert_eq!(
            action_for_picker_index(4),
            Some(ButtonAction::MediaKey {
                key: MediaKey::PlayPause
            })
        );
        assert_eq!(
            action_for_picker_index(9),
            Some(ButtonAction::MouseClick {
                button: MouseButton::Right
            })
        );
        for lang in [Lang::En, Lang::Zh] {
            let options = picker_options(lang);
            assert_eq!(options.len(), 11);
            // Every label finds its own index again — choose_action's lookup.
            for (index, label) in options.iter().enumerate() {
                assert_eq!(
                    options.iter().position(|o| o == label),
                    Some(index),
                    "{label:?}"
                );
            }
        }
        assert_eq!(picker_options(Lang::Zh)[4], "媒体：播放");
        assert_eq!(picker_options(Lang::En)[0], "Disabled");
    }

    #[test]
    fn choosing_a_chinese_label_selects_the_action() {
        let mut s = AppState::for_test(AppSettings::default());
        s.lang = Lang::Zh;
        s.choose_action(RemoteButton::Ok, "媒体：静音");
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Ok),
            Some(&ButtonAction::MediaKey {
                key: MediaKey::Mute
            })
        );
        s.choose_action(RemoteButton::Ok, "快捷键");
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Ok),
            Some(&ButtonAction::Shortcut { chord: vec![] })
        );
    }

    #[test]
    fn choose_media_action_persists_and_propagates() {
        let bridge = crate::service::StubBridge::start(std::sync::mpsc::channel().0, false).unwrap();
        let engine = crate::service::StubEngine::new();
        let mut s = AppState::for_test(settings_with_mappings());
        s.services = Services {
            bridge: Some(Arc::clone(&bridge) as SharedBridge),
            engine: Some(Arc::clone(&engine) as SharedEngine),
        };

        s.choose_action(RemoteButton::Ok, "Media: Play");
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Ok),
            Some(&ButtonAction::MediaKey {
                key: MediaKey::PlayPause
            })
        );
        assert_eq!(
            engine.state().mappings.as_ref().map(|m| m.mapped_mask()),
            Some(s.settings.mappings.mapped_mask()),
        );
        assert_eq!(
            bridge.capture_requests().last(),
            Some(&(true, s.settings.mappings.mapped_mask())),
        );
    }

    #[test]
    fn choosing_disabled_clears_the_action() {
        let mut s = AppState::for_test(settings_with_mappings());
        s.choose_action(RemoteButton::Home, "Disabled");
        assert_eq!(s.settings.mappings.action_for(RemoteButton::Home), None);
        assert!(!s.settings.mappings.buttons.contains_key(RemoteButton::Home.name()));
    }

    #[test]
    fn choosing_an_unknown_option_leaves_the_mapping_alone() {
        let mut s = AppState::for_test(settings_with_mappings());
        // A stale option (language switched mid-edit, hand-driven call)
        // must not wipe the mapping.
        s.choose_action(RemoteButton::Home, "不存在");
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Home),
            Some(&ButtonAction::Shortcut {
                chord: vec![vk::CONTROL, vk::F5]
            })
        );
    }

    #[test]
    fn shortcut_drafts_parse_persist_and_flag_errors() {
        let mut s = AppState::for_test(AppSettings::default());
        // Picking Shortcut creates the draft field.
        s.choose_action(RemoteButton::Back, "Shortcut");
        assert!(s.shortcut_edits.contains_key(&RemoteButton::Back));

        s.shortcut_edits.get_mut(&RemoteButton::Back).unwrap().text = "ctrl+alt+f9".to_string();
        s.shortcut_edited(RemoteButton::Back);
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Back),
            Some(&ButtonAction::Shortcut { chord: vec![vk::CONTROL, vk::MENU, VKey(0x78)] })
        );
        assert!(!s.chord_errors.contains_key(&RemoteButton::Back));

        s.shortcut_edits.get_mut(&RemoteButton::Back).unwrap().text = "ctrl+".to_string();
        s.shortcut_edited(RemoteButton::Back);
        assert!(
            s.chord_errors.contains_key(&RemoteButton::Back),
            "invalid input flags an error"
        );
        // The last valid chord survives the invalid edit.
        assert_eq!(
            s.settings.mappings.action_for(RemoteButton::Back),
            Some(&ButtonAction::Shortcut { chord: vec![vk::CONTROL, vk::MENU, VKey(0x78)] })
        );
    }

    #[test]
    fn existing_shortcuts_get_draft_fields_on_startup() {
        // for_test mirrors from_startup's draft seeding, so a preconfigured
        // shortcut already has its text field content.
        let s = AppState::for_test(settings_with_mappings());
        assert_eq!(s.shortcut_edits[&RemoteButton::Home].text, "Ctrl+F5");
    }

    #[test]
    fn notices_update_status_and_pressed_masks() {
        let mut s = AppState::for_test(AppSettings::default());
        s.apply_notice(UiNotice::PressedChanged(1 << RemoteButton::Up.ordinal()));
        assert_eq!(s.pressed, 1 << RemoteButton::Up.ordinal());
        let mut status = BridgeStatus::default();
        status.phase = BridgePhase::Connected;
        status.helper_pid = Some(99);
        s.apply_notice(UiNotice::Status(status.clone()));
        assert_eq!(s.bridge, status);
        s.apply_notice(UiNotice::Fatal("gone".into()));
        assert_eq!(s.launch_error.as_deref(), Some("gone"));
    }

    #[test]
    fn capture_toggle_flows_to_bridge() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = crate::service::StubBridge::start(tx, false).unwrap();
        let mut s = AppState::for_test(settings_with_mappings());
        s.services.bridge = Some(Arc::clone(&bridge) as SharedBridge);

        s.set_capture_enabled(false);
        assert!(!s.settings.capture_enabled);
        assert_eq!(bridge.capture_requests().last(), Some(&(false, mappings_with_shortcut().mapped_mask())));
        s.set_capture_enabled(true);
        assert_eq!(bridge.capture_requests().len(), 2);
    }

    #[test]
    fn voice_synth_flows_to_bridge_and_settings() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = crate::service::StubBridge::start(tx, false).unwrap();
        let mut s = AppState::for_test(AppSettings::default());
        s.services.bridge = Some(Arc::clone(&bridge) as SharedBridge);
        s.set_voice_synth(Some(0x1234)); // invalid target: stored but reported off
        assert_eq!(bridge.synth_requests(), vec![Some(0x1234)]);
        assert_eq!(s.settings.valid_voice_synth(), None);
        s.set_voice_synth(Some(0x00E6));
        assert_eq!(s.settings.voice_synth_to, Some(0x00E6));
    }

    #[test]
    fn set_language_re_resolves_and_persists() {
        let dir = std::env::temp_dir().join(format!(
            "rc-app-lang-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = AppState::for_test(AppSettings::default());
        s.settings_dir = Some(dir.clone());

        s.set_language(Language::Zh);
        assert_eq!(
            s.lang,
            Lang::Zh,
            "the render language re-resolves immediately"
        );
        let on_disk = crate::settings::load(&dir);
        assert_eq!(
            on_disk.settings.language,
            Language::Zh,
            "the choice persists"
        );
        assert_eq!(on_disk.warning, None);

        s.set_language(Language::En);
        assert_eq!(s.lang, Lang::En);
        assert_eq!(crate::settings::load(&dir).settings.language, Language::En);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn section_index_round_trips() {
        for (i, section) in Section::ALL.iter().enumerate() {
            assert_eq!(section.index(), i);
            assert_eq!(Section::from_index(i), *section);
        }
        assert_eq!(Section::from_index(99), Section::Status);
        // Labels stay in ALL order in both languages, so index-based tab
        // selection is language-proof.
        for lang in [Lang::En, Lang::Zh] {
            let labels = Section::labels(lang);
            assert_eq!(labels.len(), Section::ALL.len());
        }
        assert_eq!(Section::labels(Lang::Zh)[0], "状态");
        assert_eq!(Section::labels(Lang::En)[0], "Status");
    }

    // -- headless renders -----------------------------------------------------

    #[test]
    fn view_renders_status_section_and_helpers() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let mut rt = rune::Runtime::new(AppState::for_test(settings_with_mappings()), view, (760.0, 680.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        assert!(rt.find("title").is_some(), "window title renders");
        // The custom titlebar band: the window's full-width top band at the
        // declared height, with the folded-in title inside it and the page
        // content starting below the band.
        let band = rt.presented("titlebar").expect("titlebar band renders").0;
        assert_eq!(band[0], 0.0, "band spans from the window's leading edge");
        assert!((band[1]).abs() < 0.5, "band sits at the window's top: {:?}", band);
        assert!(
            (band[2] - 760.0).abs() < 0.5 && (band[3] - crate::views::TITLEBAR_HEIGHT).abs() < 0.5,
            "band spans the window at the titlebar height: {:?}",
            band
        );
        let (tx, ty) = rt.find("title").expect("title renders");
        assert!(
            ty > band[1] && ty < band[1] + band[3],
            "title sits inside the band: ({tx}, {ty}) vs {:?}",
            band
        );
        assert!(rt.find("section-tabs").is_some(), "section tabs render");
        let tabs = rt.presented("section-tabs").expect("section tabs render").0;
        assert!(
            tabs[1] >= band[1] + band[3],
            "page content starts below the band: tabs {:?} vs band {:?}",
            tabs,
            band
        );
        assert!(rt.find("kv-phase-value").is_some(), "status section shows by default");
        assert!(rt.find("phase-badge").is_some(), "phase badge renders");
        // The badge is folded into the band too, and stays inside it.
        let badge = rt.presented("phase-badge").expect("phase badge renders").0;
        assert!(
            badge[1] >= band[1] && badge[1] + badge[3] <= band[1] + band[3],
            "badge sits inside the band: {:?} vs {:?}",
            badge,
            band
        );
        // Default bridge phase is Stopped, so the idle guidance card shows.
        assert!(
            rt.find("group-getting-started").is_some(),
            "idle status shows the getting-started card"
        );
    }

    #[test]
    fn buttons_section_lists_all_thirteen_rows() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let mut state = AppState::for_test(settings_with_mappings());
        state.section = Section::Buttons;
        let mut rt = rune::Runtime::new(state, view, (760.0, 900.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        // The canvas' laid-out width feeds back into state between frames;
        // land the re-laid-out spatial geometry before asserting.
        rt.frame(1.0 / 60.0);
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        for button in ALL_BUTTONS {
            assert!(
                rt.find(&format!("row-{}", button.name())).is_some(),
                "row for {} renders",
                button.name()
            );
            // kit Select keys its trigger element `{key}-trigger`.
            assert!(
                rt.find(&format!("pick-{}-trigger", button.name())).is_some(),
                "action picker for {} renders",
                button.name()
            );
            for old in ["double", "long", "repeat"] {
                assert!(
                    rt.find(&format!("pick-{}-{old}-trigger", button.name())).is_none(),
                    "{} still has a {old} picker",
                    button.name()
                );
            }
        }
        // The preconfigured shortcut got a draft field.
        assert!(rt.find("shortcut-pick-home").is_some(), "shortcut field renders");

        // Geometry: every placed card sits fully inside the mapping canvas,
        // and the cards of each column never overlap (the canvas height is
        // computed from the cards' rendered heights, see views.rs).
        let canvas = rt
            .presented("mapping-canvas")
            .expect("mapping canvas renders")
            .0;
        let mut left: Vec<[f32; 4]> = Vec::new();
        let mut right: Vec<[f32; 4]> = Vec::new();
        for p in crate::views::PLACEMENTS.iter() {
            let [x, y, w, h] = rt
                .presented(&format!("row-{}", p.button.name()))
                .unwrap_or_else(|| panic!("card for {} renders", p.button.name()))
                .0;
            assert!(
                x >= canvas[0] - 0.5
                    && y >= canvas[1] - 0.5
                    && x + w <= canvas[0] + canvas[2] + 0.5
                    && y + h <= canvas[1] + canvas[3] + 0.5,
                "{} card [{x}, {y}, {w}, {h}] leaves the canvas {:?}",
                p.button.name(),
                canvas
            );
            (if p.left { &mut left } else { &mut right }).push([x, y, w, h]);
        }
        for (mut column, side) in [(left, "left"), (right, "right")] {
            column.sort_by_key(|r| (r[1] * 100.0) as i64);
            for pair in column.windows(2) {
                assert!(
                    pair[1][1] >= pair[0][1] + pair[0][3] - 0.5,
                    "{side} column cards overlap: {:?} vs {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
        // The photo is centered horizontally in the canvas.
        let photo = rt.presented("remote-photo").expect("remote photo renders").0;
        assert!(
            (photo[0] - canvas[0] - (canvas[2] - photo[2]) / 2.0).abs() < 1.0,
            "photo is centered: photo {:?} in canvas {:?}",
            photo,
            canvas
        );
        // Every leader line connects its card to the photo: the elbow's
        // vertical segment stays inside the photo's band, the horizontal
        // segment meets the card's center at its near edge, and its far end
        // lands on the photo (see `leader_lines` in views.rs).
        let [photo_x, photo_y, photo_w, photo_h] = photo;
        for p in crate::views::PLACEMENTS.iter() {
            let name = p.button.name();
            let [_, vy, _, vh] = rt
                .presented(&format!("line-v-{name}"))
                .unwrap_or_else(|| panic!("vertical line for {name} renders"))
                .0;
            assert!(
                vy >= photo_y - 0.5 && vy + vh <= photo_y + photo_h + 0.5,
                "{name}'s leader line escapes the photo band: y {vy}..{} vs photo {}..{}",
                vy + vh,
                photo_y,
                photo_y + photo_h
            );
            let [hx, hy, hw, _] = rt
                .presented(&format!("line-h-{name}"))
                .unwrap_or_else(|| panic!("horizontal line for {name} renders"))
                .0;
            let [cx, cy, cw, ch] = rt
                .presented(&format!("row-{name}"))
                .unwrap_or_else(|| panic!("card for {name} renders"))
                .0;
            assert!(
                (hy - (cy + ch / 2.0)).abs() <= 1.0,
                "{name}'s horizontal line misses the card's center line: {hy} vs {}",
                cy + ch / 2.0
            );
            let near = if p.left { hx } else { hx + hw };
            let far = if p.left { hx + hw } else { hx };
            let card_edge = if p.left { cx + cw } else { cx };
            assert!(
                (near - card_edge).abs() <= 1.0,
                "{name}'s horizontal line misses its card's near edge: {near} vs {card_edge}"
            );
            assert!(
                far >= photo_x - 0.5 && far <= photo_x + photo_w + 0.5,
                "{name}'s horizontal line ends off the photo: {far} vs {}..{}",
                photo_x,
                photo_x + photo_w
            );
        }
    }

    #[test]
    fn capture_switch_click_toggles_settings_and_bridge() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = crate::service::StubBridge::start(tx, false).unwrap();
        let mut state = AppState::for_test(settings_with_mappings());
        state.services.bridge = Some(Arc::clone(&bridge) as SharedBridge);
        state.section = Section::Capture;
        assert!(state.settings.capture_enabled);

        let mut rt = rune::Runtime::new(state, view, (760.0, 680.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        let (x, y) = rt.find("capture-switch").expect("capture switch renders");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
        assert!(!rt.state.settings.capture_enabled, "the click toggled the setting");
        assert_eq!(
            bridge.capture_requests().last(),
            Some(&(false, mappings_with_shortcut().mapped_mask())),
            "the toggle recomputed the capture set on the bridge"
        );
    }

    #[test]
    fn section_tabs_switch_content() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let mut rt = rune::Runtime::new(AppState::for_test(AppSettings::default()), view, (760.0, 680.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        rt.update(|s: &mut AppState, _| s.section = Section::Helper);
        rt.frame(1.0 / 60.0);
        assert!(rt.find("helper-buttons").is_some(), "helper section shows its buttons");
        rt.update(|s: &mut AppState, _| s.section = Section::Capture);
        rt.frame(1.0 / 60.0);
        assert!(rt.find("capture-switch").is_some(), "capture section shows its switch");
    }

    #[test]
    fn launch_error_draws_the_error_overlay() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let mut state = AppState::for_test(AppSettings::default());
        state.launch_error = Some(StartError::AnotherInstance.message(Lang::En));
        let mut rt = rune::Runtime::new(state, view, (760.0, 680.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        assert!(rt.find("error-overlay").is_some(), "the conflict overlay renders");
        assert!(rt.find("error-exit").is_some(), "the Exit button renders");
    }

    #[test]
    fn pressed_notice_lights_the_row_dot() {
        if !ui_enabled() {
            eprintln!("skipping: set RC_APP_UI_TESTS=1 with a GPU present");
            return;
        }
        let mut rt = rune::Runtime::new(AppState::for_test(AppSettings::default()), view, (760.0, 680.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        rt.update(|s: &mut AppState, _| s.section = Section::Buttons);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.pressed, 0);
        rt.update(|s: &mut AppState, _| s.apply_notice(UiNotice::PressedChanged(1 << RemoteButton::Ok.ordinal())));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.pressed, 1 << RemoteButton::Ok.ordinal());
        assert!(rt.find("pressed-ok").is_some(), "the Ok row shows its pressed label");
    }
}
