//! The Rune UI: one window with five sections (Status, Capture, Buttons,
//! Helper, Settings). All state changes funnel through [`AppState`] methods
//! so they are testable headlessly; the view only places elements.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rune::{AppHandle, TextEdit, Tx};

use rc_protocol::{ALL_BUTTONS, RemoteButton};

use crate::bridge_api::{BridgeStatus, SharedBridge, SharedEngine, UiNotice};
use crate::mappings::{ButtonAction, MediaKey, MouseButton, Slot};
use crate::settings::AppSettings;
use crate::task::TaskStatus;

/// The action picker's options (V1 editor). Media keys, shortcut and mouse
/// clicks; `MouseMove`/`Scroll` actions can only arrive from hand-edited
/// settings files and display under their own labels (see
/// `ButtonAction::picker_label`).
pub const PICKER_OPTIONS: [&str; 11] = [
    "Disabled",
    MediaKey::Mute.label(),
    MediaKey::VolumeDown.label(),
    MediaKey::VolumeUp.label(),
    MediaKey::PlayPause.label(),
    MediaKey::Prev.label(),
    MediaKey::Next.label(),
    "Shortcut",
    MouseButton::Left.label(),
    MouseButton::Right.label(),
    MouseButton::Middle.label(),
];

/// Map a picker label back to its action. `Disabled` maps to None (the slot
/// is cleared); an unknown label leaves the mapping untouched.
pub fn action_from_label(label: &str) -> Option<ButtonAction> {
    match label {
        "Disabled" => None,
        "Shortcut" => Some(ButtonAction::Shortcut { chord: Vec::new() }),
        other => {
            let media = [
                (MediaKey::Mute.label(), MediaKey::Mute),
                (MediaKey::VolumeDown.label(), MediaKey::VolumeDown),
                (MediaKey::VolumeUp.label(), MediaKey::VolumeUp),
                (MediaKey::PlayPause.label(), MediaKey::PlayPause),
                (MediaKey::Prev.label(), MediaKey::Prev),
                (MediaKey::Next.label(), MediaKey::Next),
            ]
            .into_iter()
            .find(|(name, _)| *name == other)
            .map(|(_, key)| key);
            if let Some(key) = media {
                return Some(ButtonAction::MediaKey { key });
            }
            let mouse = [
                (MouseButton::Left.label(), MouseButton::Left),
                (MouseButton::Right.label(), MouseButton::Right),
                (MouseButton::Middle.label(), MouseButton::Middle),
            ]
            .into_iter()
            .find(|(name, _)| *name == other)
            .map(|(_, button)| button);
            mouse.map(|button| ButtonAction::MouseClick { button })
        }
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

    pub const LABELS: [&'static str; 5] = ["Status", "Capture", "Buttons", "Helper", "Settings"];

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
    pub load_warning: Option<String>,
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

/// Per-slot UI editor key (shortcut drafts and chord errors).
pub(crate) fn slot_key(button: RemoteButton, slot: Slot) -> u8 {
    (button.ordinal() * 4 + slot.index() as u32) as u8
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
    /// Shortcut chord drafts per slot key, created when a slot picks
    /// `Shortcut`; editing parses and persists immediately.
    pub shortcut_edits: BTreeMap<u8, TextEdit>,
    /// Parse errors per slot key, shown under the draft field.
    pub chord_errors: BTreeMap<u8, String>,
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
    /// State for headless state tests: no services, no persistence.
    #[cfg(test)]
    pub fn for_test(settings: AppSettings) -> AppState {
        let mut state = AppState {
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
        let mut state = AppState {
            settings: startup.settings,
            settings_dir: startup.settings_dir,
            section: Section::Status,
            warning: startup.load_warning,
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

    /// Pre-creates a shortcut draft for every slot already holding a
    /// `Shortcut` action, so its text field shows the chord at once.
    fn seed_shortcut_drafts(&mut self) {
        let mut existing: Vec<(RemoteButton, Slot, Vec<crate::mappings::VKey>)> = Vec::new();
        for button in ALL_BUTTONS {
            for slot in [Slot::Single, Slot::Double, Slot::Long] {
                if let Some(ButtonAction::Shortcut { chord }) = self.settings.mappings.slot_action(button, slot) {
                    existing.push((button, slot, chord.clone()));
                }
            }
        }
        for (button, slot, chord) in existing {
            let key = slot_key(button, slot);
            self.shortcut_edits
                .entry(key)
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
            Err(why) => self.save_error = Some(why),
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

    /// One picker choose: update the mapping, keep the shortcut draft in
    /// sync, persist and push everything downstream.
    pub fn choose_action(&mut self, button: RemoteButton, slot: Slot, label: &str) {
        let key = slot_key(button, slot);
        match action_from_label(label) {
            None => {
                self.settings.mappings.set_slot_action(button, slot, None);
                self.shortcut_edits.remove(&key);
                self.chord_errors.remove(&key);
            }
            Some(ButtonAction::Shortcut { .. }) => {
                let chord = match self.settings.mappings.slot_action(button, slot) {
                    Some(ButtonAction::Shortcut { chord }) => chord.clone(),
                    _ => Vec::new(),
                };
                self.shortcut_edits
                    .entry(key)
                    .or_insert_with(|| TextEdit::new(crate::chord::format_chord(&chord)));
                self.chord_errors.remove(&key);
                self.settings
                    .mappings
                    .set_slot_action(button, slot, Some(ButtonAction::Shortcut { chord }));
            }
            Some(action) => {
                self.settings.mappings.set_slot_action(button, slot, Some(action));
                self.shortcut_edits.remove(&key);
                self.chord_errors.remove(&key);
            }
        }
        self.after_mappings_change();
    }

    /// The shortcut draft field changed: parse and persist when valid, flag
    /// the error (leaving the last valid chord in place) when not.
    pub fn shortcut_edited(&mut self, button: RemoteButton, slot: Slot) {
        let key = slot_key(button, slot);
        let Some(edit) = self.shortcut_edits.get(&key) else { return };
        let draft = edit.text.clone();
        match crate::chord::parse_chord(&draft) {
            Ok(chord) => {
                self.chord_errors.remove(&key);
                self.settings
                    .mappings
                    .set_slot_action(button, slot, Some(ButtonAction::Shortcut { chord }));
                self.after_mappings_change();
            }
            Err(why) => {
                self.chord_errors.insert(key, why);
            }
        }
    }

    pub fn set_dark(&mut self, dark: Option<bool>) {
        self.settings.window.dark = dark;
        self.persist();
    }

    /// Reload settings.json from disk (the Settings section's button).
    pub fn reload_settings(&mut self) {
        let Some(dir) = self.settings_dir.clone() else { return };
        let loaded = crate::settings::load(&dir);
        self.settings = loaded.settings;
        self.warning = loaded.warning;
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
        let Some(ui) = self.ui.clone() else {
            self.task_busy = false;
            self.task_note = Some("No UI handle: helper commands need the windowed app".to_string());
            return;
        };
        std::thread::spawn(move || {
            let outcome = run_helper_blocking(command);
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
fn run_helper_blocking(command: HelperCommand) -> HelperOutcome {
    let note = match command {
        HelperCommand::Install => {
            crate::task::helper_path()
                .and_then(|helper| crate::task::install_task(&helper))
                .map(|()| "Helper task installed (UAC answered)".to_string())
        }
        HelperCommand::Remove => {
            crate::task::helper_path()
                .and_then(|helper| crate::task::remove_task(&helper))
                .map(|()| "Helper task removed".to_string())
        }
        HelperCommand::Start => crate::task::start_task().and_then(|out| {
            if out.ok() {
                Ok("Helper start requested".to_string())
            } else {
                Err(format!("schtasks /run failed: {}", first_line(&out.stderr, &out.stdout)))
            }
        }),
        HelperCommand::Stop => crate::task::stop_task().and_then(|out| {
            if out.ok() {
                Ok("Helper stop requested (it exits on disconnect by design; the bridge keeps running)".to_string())
            } else {
                Err(format!("schtasks /end failed: {}", first_line(&out.stderr, &out.stdout)))
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
        match crate::task::query_task_status() {
            Ok(status) => {
                if note.is_empty() {
                    note = status.label();
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

fn first_line(a: &str, b: &str) -> String {
    let source = if a.trim().is_empty() { b } else { a };
    source
        .lines()
        .next()
        .unwrap_or("(no output)")
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
        std::fs::create_dir_all(&dir).unwrap();
        use rune::render::{Gpu, OFFSCREEN_FORMAT, Offscreen, Renderer, write_png};

        let gpu = Gpu::headless();
        let mut settings = settings_with_mappings();
        settings.window.dark = Some(false); // the SwiftUI-style light look
        settings.capture_enabled = true;
        for (name, section) in [
            ("status", Section::Status),
            ("capture", Section::Capture),
            ("buttons", Section::Buttons),
            ("helper", Section::Helper),
            ("settings", Section::Settings),
        ] {
            let mut state = AppState::for_test(settings.clone());
            state.section = section;
            state.bridge = BridgeStatus {
                phase: BridgePhase::Listening,
                helper_pid: Some(4812),
                generation: 3,
                ..BridgeStatus::default()
            };
            // The Buttons mapping canvas is a tall fixed geometry: render it
            // in a window tall enough to show the whole canvas (intro, the
            // canvas itself, and the volume_mute card below it).
            let (win_w, win_h) = if section == Section::Buttons {
                (780.0, crate::views::mapping_canvas_h(&state) + 400.0)
            } else {
                (780.0, 720.0)
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
        m.set_slot_action(
            RemoteButton::Home,
            Slot::Single,
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
    fn labels_round_trip_into_actions() {
        assert_eq!(action_from_label("Disabled"), None);
        assert_eq!(
            action_from_label("Media: Vol +"),
            Some(ButtonAction::MediaKey { key: MediaKey::VolumeUp })
        );
        assert_eq!(
            action_from_label("Mouse: Right"),
            Some(ButtonAction::MouseClick { button: MouseButton::Right })
        );
        assert_eq!(
            action_from_label("Shortcut"),
            Some(ButtonAction::Shortcut { chord: vec![] })
        );
        assert_eq!(action_from_label("nonsense"), None);
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

        s.choose_action(RemoteButton::Ok, Slot::Double, "Media: Play");
        assert_eq!(
            s.settings.mappings.slot_action(RemoteButton::Ok, Slot::Double),
            Some(&ButtonAction::MediaKey { key: MediaKey::PlayPause })
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
    fn choosing_disabled_clears_the_slot() {
        let mut s = AppState::for_test(settings_with_mappings());
        s.choose_action(RemoteButton::Home, Slot::Single, "Disabled");
        assert_eq!(s.settings.mappings.slot_action(RemoteButton::Home, Slot::Single), None);
        assert!(!s.settings.mappings.buttons.contains_key(RemoteButton::Home.name()));
    }

    #[test]
    fn shortcut_drafts_parse_persist_and_flag_errors() {
        let mut s = AppState::for_test(AppSettings::default());
        // Picking Shortcut creates the draft field.
        s.choose_action(RemoteButton::Back, Slot::Long, "Shortcut");
        let key = slot_key(RemoteButton::Back, Slot::Long);
        assert!(s.shortcut_edits.contains_key(&key));

        s.shortcut_edits.get_mut(&key).unwrap().text = "ctrl+alt+f9".to_string();
        s.shortcut_edited(RemoteButton::Back, Slot::Long);
        assert_eq!(
            s.settings.mappings.slot_action(RemoteButton::Back, Slot::Long),
            Some(&ButtonAction::Shortcut { chord: vec![vk::CONTROL, vk::MENU, VKey(0x78)] })
        );
        assert!(!s.chord_errors.contains_key(&key));

        s.shortcut_edits.get_mut(&key).unwrap().text = "ctrl+".to_string();
        s.shortcut_edited(RemoteButton::Back, Slot::Long);
        assert!(s.chord_errors.contains_key(&key), "invalid input flags an error");
        // The last valid chord survives the invalid edit.
        assert_eq!(
            s.settings.mappings.slot_action(RemoteButton::Back, Slot::Long),
            Some(&ButtonAction::Shortcut { chord: vec![vk::CONTROL, vk::MENU, VKey(0x78)] })
        );
    }

    #[test]
    fn existing_shortcuts_get_draft_fields_on_startup() {
        // for_test mirrors from_startup's draft seeding, so a preconfigured
        // shortcut already has its text field content.
        let s = AppState::for_test(settings_with_mappings());
        let key = slot_key(RemoteButton::Home, Slot::Single);
        assert_eq!(s.shortcut_edits[&key].text, "Ctrl+F5");
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
    fn section_index_round_trips() {
        for (i, section) in Section::ALL.iter().enumerate() {
            assert_eq!(section.index(), i);
            assert_eq!(Section::from_index(i), *section);
        }
        assert_eq!(Section::from_index(99), Section::Status);
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
        assert!(rt.find("section-tabs").is_some(), "section tabs render");
        assert!(rt.find("kv-phase-value").is_some(), "status section shows by default");
        assert!(rt.find("phase-badge").is_some(), "phase badge renders");
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
                rt.find(&format!("pick-{}-single-trigger", button.name())).is_some(),
                "single picker for {} renders",
                button.name()
            );
        }
        // The preconfigured shortcut got a draft field.
        assert!(rt.find("shortcut-pick-home-single").is_some(), "shortcut field renders");

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
        state.launch_error = Some(StartError::AnotherInstance.message());
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


