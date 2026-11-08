//! The Rune UI: one window with five sections (Status, Capture, Buttons,
//! Helper, Settings). All state changes funnel through [`AppState`] methods
//! so they are testable headlessly; the view only places elements.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rune::{AppHandle, Cx, El, ScrollSpec, TextEdit, Tx, col, el, row, text};
use rune::tok::{radius, space, type_};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, Kit, SegmentedControl, Select, Switch, TextInput, root};

use rc_protocol::{ALL_BUTTONS, RemoteButton};
use rc_protocol::buttons::ALWAYS_CAPTURED_BUTTONS;

use crate::bridge_api::{BridgePhase, BridgeStatus, SharedBridge, SharedEngine, UiNotice};
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
fn slot_key(button: RemoteButton, slot: Slot) -> u8 {
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

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The whole window. `dark` follows the setting, else the system.
pub fn view(s: &AppState, cx: &mut Cx<AppState>) -> El {
    let dark = s.settings.window.dark.unwrap_or(cx.system_dark());
    let k = Kit::neutral(dark);
    let mut layer = Vec::new();

    let mut body = col()
        .key("window")
        .w_full()
        .h_full()
        .fill(k.c.page)
        .px(space::S6)
        .py(space::S6)
        .gap(space::S6);

    body = body.child(header(s, &k));
    if let Some(warning) = &s.warning {
        body = body.child(banner(warning, &k));
    }
    body = body.child(
        SegmentedControl::new(&Section::LABELS, Some(s.section.index()))
            .label("Section")
            .fit()
            .on_pick(|s: &mut AppState, index, _: &mut Tx| s.section = Section::from_index(index))
            .build(&k)
            .key("section-tabs"),
    );

    let content = match s.section {
        Section::Status => status_section(s, &k),
        Section::Capture => capture_section(s, &k, &mut layer),
        Section::Buttons => buttons_section(s, &k, &mut layer),
        Section::Helper => helper_section(s, &k),
        Section::Settings => settings_section(s, &k),
    };
    // The scroll viewport needs a bounded ancestor and min_h(0) columns.
    body = body.child(content.grow(1.0).min_h(0.0));
    body = body.child(footer(s, &k));

    if let Some(error) = &s.launch_error {
        layer.push(error_overlay(error, &k));
    }
    root(body, layer)
}

fn header(s: &AppState, k: &Kit) -> El {
    row()
        .key("header")
        .w_full()
        .center_y()
        .gap(space::S3)
        .child(text("Remote Mic Rune").key("title").font(type_::TITLE).color(k.c.text_base))
        .child(text(format!("v{}", env!("CARGO_PKG_VERSION")))
            .font(type_::LABEL)
            .color(k.c.text_faint))
        .child(el().grow(1.0))
        .child(badge_for_phase(&s.bridge, &k))
}

fn badge_for_phase(status: &BridgeStatus, k: &Kit) -> El {
    let (label, variant) = match status.phase {
        BridgePhase::Stopped => ("Bridge stopped", BadgeVariant::Neutral),
        BridgePhase::Listening => ("Listening for helper", BadgeVariant::Neutral),
        BridgePhase::Connected => ("Connected", BadgeVariant::Accent),
    };
    Badge::new(label).variant(variant).build(k).key("phase-badge")
}

fn banner(message: &str, k: &Kit) -> El {
    row()
        .key("warning-banner")
        .w_full()
        .padding(space::S3)
        .gap(space::S3)
        .radius(radius::MD)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_strong)
        .child(text(message.to_string()).grow(1.0).font(type_::BODY).color(k.c.text_muted))
}

fn error_overlay(message: &str, k: &Kit) -> El {
    col()
        .key("error-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(k.c.page)
        .child(
            col()
                .key("error-card")
                .w(420.0)
                .padding(space::S6)
                .gap(space::S3)
                .radius(radius::LG)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_strong)
                .child(
                    text("Cannot start")
                        .key("error-title")
                        .font(type_::TITLE)
                        .color(k.c.text_base),
                )
                .child(text(message.to_string()).font(type_::BODY).color(k.c.text_muted))
                .child(
                    Button::new("Exit")
                        .variant(ButtonVariant::Submit)
                        .on_press_tx(|_: &mut AppState, tx: &mut Tx| tx.quit())
                        .build(k)
                        .key("error-exit"),
                ),
        )
}

fn kv_row(key: &str, label: &str, value: String, k: &Kit) -> El {
    row()
        .key(format!("kv-{key}"))
        .w_full()
        .center_y()
        .gap(space::S3)
        .child(
            text(label.to_string())
                .w(150.0)
                .font(type_::LABEL)
                .color(k.c.text_muted),
        )
        .child(text(value).key(format!("kv-{key}-value")).grow(1.0).font(type_::BODY).color(k.c.text_base))
}

fn status_section(s: &AppState, k: &Kit) -> El {
    let mut list = col().key("status-section").w_full().padding(space::S6).gap(space::S2);
    list = list.child(kv_row("phase", "Bridge phase", crate::status::phase_label(s.bridge.phase).to_string(), k));
    list = list.child(kv_row(
        "pid",
        "Helper pid",
        s.bridge
            .helper_pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "none".to_string()),
        k,
    ));
    list = list.child(kv_row("ownership", "Ownership", crate::status::ownership_line(&s.bridge), k));
    list = list.child(kv_row(
        "acked",
        "Agent acked",
        crate::status::acked_buttons_line(&s.bridge.acked_usages),
        k,
    ));
    list = list.child(kv_row(
        "recovering",
        "Recovery",
        if s.bridge.recovering { "Device-node recovery in progress" } else { "Idle" }.to_string(),
        k,
    ));
    list = list.child(kv_row(
        "capture",
        "Capture set",
        crate::status::acked_buttons_line(&s.bridge.captured_usages),
        k,
    ));
    list = list.child(kv_row("counters", "Counters", crate::status::counters_line(&s.bridge.counters), k));
    list = list.child(kv_row(
        "configured",
        "Buttons configured",
        format!("{} of {}", s.settings.mappings.configured_buttons(), ALL_BUTTONS.len()),
        k,
    ));
    list = list.child(
        text("Counters update every 500 ms from the bridge snapshot.")
            .font(type_::LABEL)
            .color(k.c.text_faint),
    );
    list
}

fn capture_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mapped = s.settings.mappings.mapped_mask();
    let synth = s.settings.valid_voice_synth();
    let synth_label = match synth {
        Some(0x00E2) => "Left Alt (0x00E2)",
        Some(0x00E6) => "Right Alt (0x00E6)",
        _ => "Off",
    };
    const SYNTH_OPTIONS: [&str; 3] = ["Off", "Left Alt (0x00E2)", "Right Alt (0x00E6)"];

    let mut list = col().key("capture-section").w_full().padding(space::S6).gap(space::S3);
    list = list.child(
        Switch::new(s.settings.capture_enabled)
            .label("Enhanced capture")
            .on_toggle(|s: &mut AppState| {
                s.set_capture_enabled(!s.settings.capture_enabled);
            })
            .build(k)
            .key("capture-switch"),
    );
    let always_names: Vec<&str> = ALWAYS_CAPTURED_BUTTONS.iter().map(|b| b.name()).collect();
    list = list.child(
        text(format!(
            "While enabled, {} are always captured, even with no mappings configured.",
            always_names.join(", ")
        ))
        .font(type_::BODY)
        .color(k.c.text_muted),
    );
    list = list.child(kv_row("mask", "Mapped buttons", format!("{}/13", mapped.count_ones()), k));
    list = list.child(kv_row(
        "set",
        "Requested set",
        crate::status::acked_buttons_line(&s.bridge.captured_usages),
        k,
    ));
    list = list.child(
        col()
            .key("synth-row")
            .w_full()
            .gap(space::S1)
            .child(text("Voice key synthesis").font(type_::LABEL).color(k.c.text_muted))
            .child(
                Select::new("synth-picker", &SYNTH_OPTIONS)
                    .width(200.0)
                    .current(Some(synth_label))
                    .open(s.open_picker.as_deref() == Some("synth-picker"))
                    .on_toggle(|s: &mut AppState| {
                        s.open_picker = match &s.open_picker {
                            Some(key) if key == "synth-picker" => None,
                            _ => Some("synth-picker".to_string()),
                        }
                    })
                    .on_close(|s: &mut AppState| s.open_picker = None)
                    .on_choose(|s: &mut AppState, choice: &str, _: &mut Tx| {
                        s.open_picker = None;
                        let to = match choice {
                            "Left Alt (0x00E2)" => Some(0x00E2),
                            "Right Alt (0x00E6)" => Some(0x00E6),
                            _ => None,
                        };
                        s.set_voice_synth(to);
                    })
                    .build(k, layer),
            )
            .child(
                text("Synthesizes the voice key (0x003E) into an Alt usage at the report layer; \
                      targets are hardware-validated.")
                    .font(type_::LABEL)
                    .color(k.c.text_faint),
            ),
    );
    list
}

/// One button's mapping row: name, pressed dot, and the three V1 pickers
/// (single / double / long). `hold_repeat` stays serialized but has no V1
/// editor.
fn button_row(s: &AppState, button: RemoteButton, k: &Kit, layer: &mut Vec<El>) -> El {
    let pressed = s.pressed & (1 << button.ordinal()) != 0;
    let mut card = col()
        .key(format!("row-{}", button.name()))
        .w_full()
        .padding(space::S3)
        .gap(space::S2)
        .radius(radius::MD)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_muted);

    card = card.child(
        row()
            .key(format!("row-head-{}", button.name()))
            .w_full()
            .center_y()
            .gap(space::S2)
            .child(
                el()
                    .key(format!("dot-{}", button.name()))
                    .size(8.0, 8.0)
                    .radius(4.0)
                    .fill(if pressed { k.c.bg_accent } else { k.c.border_base }),
            )
            .child(
                text(button.name().to_string())
                    .key(format!("name-{}", button.name()))
                    .font(type_::BODY)
                    .grow(1.0)
                    .color(k.c.text_base),
            )
            .child(
                text(if pressed { "pressed" } else { "" })
                    .key(format!("pressed-{}", button.name().to_ascii_lowercase()))
                    .font(type_::LABEL)
                    .color(k.c.text_accent),
            ),
    );

    let mut slots_row = row()
        .key(format!("row-slots-{}", button.name()))
        .w_full()
        .gap(space::S3)
        .pb(space::S1);
    for slot in [Slot::Single, Slot::Double, Slot::Long] {
        let key = format!("pick-{}-{}", button.name(), slot.caption().to_ascii_lowercase());
        let current = s
            .settings
            .mappings
            .slot_action(button, slot)
            .map(|a| a.picker_label())
            .unwrap_or_else(|| "Disabled".to_string());
        let mut slot_col = col().key(format!("col-{key}")).gap(space::S1);
        slot_col = slot_col.child(text(slot.caption().to_string()).font(type_::LABEL).color(k.c.text_muted));
        slot_col = slot_col.child(
            Select::new(key.clone(), &PICKER_OPTIONS)
                .width(150.0)
                .current(Some(current.as_str()))
                .open(s.open_picker.as_deref() == Some(key.as_str()))
                .on_toggle({
                    let key = key.clone();
                    move |s: &mut AppState| {
                        s.open_picker = match &s.open_picker {
                            Some(open) if *open == key => None,
                            _ => Some(key.clone()),
                        }
                    }
                })
                .on_close(|s: &mut AppState| s.open_picker = None)
                .on_choose(move |s: &mut AppState, choice: &str, _: &mut Tx| {
                    s.open_picker = None;
                    s.choose_action(button, slot, choice);
                })
                .build(k, layer),
        );

        // When the slot holds a shortcut, show its draft field with a parse
        // error underneath when invalid.
        let slot_key = slot_key(button, slot);
        if let Some(edit) = s.shortcut_edits.get(&slot_key) {
            let error = s.chord_errors.get(&slot_key);
            let mut field_col = col().key(format!("shortcut-{key}")).gap(space::S1);
            field_col = field_col.child(
                TextInput::new(edit)
                    .placeholder("Ctrl+Alt+F5")
                    .width(Some(150.0))
                    .invalid(error.is_some())
                    .on_edit(move |s: &mut AppState| match s.shortcut_edits.get_mut(&slot_key) {
                        Some(edit) => edit,
                        None => panic!("shortcut draft exists for edited slot"),
                    })
                    .on_change(move |s: &mut AppState, _: &mut Tx| s.shortcut_edited(button, slot))
                    .build(k),
            );
            if let Some(why) = error {
                field_col = field_col.child(
                    text(why.clone())
                        .key(format!("chord-error-{key}"))
                        .font(type_::LABEL)
                        .color(rune::oklch(0.55, 0.19, 25.0)),
                );
            }
            slots_row = slots_row.child(field_col);
        }
        slots_row = slots_row.child(slot_col);
    }
    card = card.child(slots_row);
    card
}

fn buttons_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mut list = col()
        .key("buttons-section")
        .w_full()
        .padding(space::S6)
        .gap(space::S2)
        .scroll(ScrollSpec { stick_bottom: false })
        .shrink();
    list = list.child(
        text("Pick what each press, double press and long press does. Changes persist immediately \
              and update the capture set.")
            .font(type_::BODY)
            .color(k.c.text_muted),
    );
    for button in ALL_BUTTONS {
        list = list.child(button_row(s, button, k, layer));
    }
    list
}

fn helper_section(s: &AppState, k: &Kit) -> El {
    let status_line = s
        .task_status
        .as_ref()
        .map(|st| st.label())
        .unwrap_or_else(|| "Not checked yet".to_string());

    let mut list = col().key("helper-section").w_full().padding(space::S6).gap(space::S3);
    list = list.child(
        text("The helper runs elevated via one scheduled task: install it once (UAC), then start \
              and stop it without prompts. It connects to the bridge pipe and does all elevated \
              work. It exits on app disconnect by design; the bridge keeps running.")
            .wrap()
            .font(type_::BODY)
            .color(k.c.text_muted),
    );
    let mut buttons_row = row().key("helper-buttons").gap(space::S2);
    for (key, label, variant, command) in [
        ("install", "Install helper task (UAC once)", ButtonVariant::Submit, HelperCommand::Install),
        ("start", "Start helper", ButtonVariant::Neutral, HelperCommand::Start),
        ("stop", "Stop helper", ButtonVariant::Neutral, HelperCommand::Stop),
        ("remove", "Remove task", ButtonVariant::Danger, HelperCommand::Remove),
    ] {
        buttons_row = buttons_row.child(
            Button::new(label)
                .variant(variant)
                .disabled(s.task_busy)
                .on_press(move |s: &mut AppState| s.run_helper_command(command))
                .build(k)
                .key(format!("helper-{key}")),
        );
    }
    list = list.child(buttons_row);
    list = list.child(
        row()
            .key("helper-status-row")
            .gap(space::S2)
            .center_y()
            .child(
                Button::new("Check status")
                    .variant(ButtonVariant::Ghost)
                    .disabled(s.task_busy)
                    .on_press(|s: &mut AppState| s.run_helper_command(HelperCommand::RefreshStatus))
                    .build(k)
                    .key("helper-refresh"),
            )
            .child(text(status_line).key("helper-status").font(type_::BODY).color(k.c.text_base)),
    );
    if s.task_busy {
        list = list.child(
            text("Working…")
                .key("helper-busy")
                .font(type_::LABEL)
                .color(k.c.text_muted),
        );
    }
    if let Some(note) = &s.task_note {
        list = list.child(
            text(note.clone())
                .key("helper-note")
                .font(type_::LABEL)
                .color(k.c.text_muted),
        );
    }
    list
}

fn settings_section(s: &AppState, k: &Kit) -> El {
    const DARK_OPTIONS: [&str; 3] = ["Auto", "Light", "Dark"];
    let selected = match s.settings.window.dark {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let path = crate::settings::app_dir()
        .map(|dir| crate::settings::settings_path(&dir).display().to_string())
        .unwrap_or_else(|| "settings.json (LOCALAPPDATA unavailable: not persisted this run)".to_string());

    let mut list = col().key("settings-section").w_full().padding(space::S6).gap(space::S3);
    list = list.child(
        col()
            .key("appearance")
            .gap(space::S1)
            .child(text("Appearance").font(type_::LABEL).color(k.c.text_muted))
            .child(
                SegmentedControl::new(&DARK_OPTIONS, Some(selected))
                    .label("Appearance")
                    .fit()
                    .on_pick(|s: &mut AppState, index, _: &mut Tx| {
                        s.set_dark(match index {
                            1 => Some(false),
                            2 => Some(true),
                            _ => None,
                        });
                    })
                    .build(k)
                    .key("appearance-tabs"),
            ),
    );
    list = list.child(kv_row("path", "Settings file", path, k));
    list = list.child(
        row()
            .key("settings-actions")
            .gap(space::S2)
            .child(
                Button::new("Save now")
                    .variant(ButtonVariant::Neutral)
                    .on_press(|s: &mut AppState| {
                        s.persist();
                    })
                    .build(k)
                    .key("settings-save"),
            )
            .child(
                Button::new("Reload from disk")
                    .variant(ButtonVariant::Neutral)
                    .on_press(|s: &mut AppState| s.reload_settings())
                    .build(k)
                    .key("settings-reload"),
            ),
    );
    if let Some(error) = &s.save_error {
        list = list.child(
            text(error.clone())
                .key("settings-save-error")
                .font(type_::LABEL)
                .color(rune::oklch(0.55, 0.19, 25.0)),
        );
    }
    list
}

fn footer(s: &AppState, k: &Kit) -> El {
    let pressed = crate::engine::pressed_names(s.pressed).join(", ");
    let pressed = if pressed.is_empty() { "none".to_string() } else { pressed };
    row()
        .key("footer")
        .w_full()
        .center_y()
        .gap(space::S3)
        .child(
            text(format!("Pressed: {pressed}"))
                .key("footer-pressed")
                .font(type_::LABEL)
                .color(k.c.text_faint),
        )
        .child(el().grow(1.0))
        .child(
            text(if s.settings.capture_enabled {
                "Enhanced capture on"
            } else {
                "Enhanced capture off"
            })
            .key("footer-capture")
            .font(type_::LABEL)
            .color(k.c.text_faint),
        )
}

// ---------------------------------------------------------------------------
// Tests: pure state transitions plus headless runtime renders (they need a
// GPU, so they skip unless RC_APP_UI_TESTS=1; the orb has lavapipe).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::bridge_api::StartError;
    use crate::mappings::{ButtonMappings, VKey, vk};
    use rc_protocol::RemoteButton;

    fn ui_enabled() -> bool {
        match std::env::var("RC_APP_UI_TESTS") {
            Ok(v) => v != "0",
            Err(_) => false,
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


