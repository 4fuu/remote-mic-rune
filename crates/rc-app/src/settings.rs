//! settings.json under `%LOCALAPPDATA%\RemoteMicRune\`: button mappings plus
//! app preferences. Loading is fault-tolerant: a malformed file is backed up
//! to settings.json.bak and the app starts fresh, never crashing or losing
//! the broken file for inspection.
//!
//! Load/save failures come back as small enums ([`LoadWarning`],
//! [`BackupFail`], [`SaveError`]) carrying the raw OS error text; the UI
//! translates them at display time, because loading runs before the UI
//! language is applied.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use rc_protocol::buttons::VOICE_SYNTHABLE_USAGES;

use crate::i18n::{Lang, Language, tr_fmt};
use crate::mappings::ButtonMappings;

pub const SETTINGS_FILE: &str = "settings.json";
pub const BACKUP_SUFFIX: &str = ".bak";

/// Everything the app persists between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub mappings: ButtonMappings,
    /// Whether enhanced capture is requested from the bridge.
    pub capture_enabled: bool,
    /// Voice-key synthesis target usage (a key in `VOICE_SYNTHABLE_USAGES`),
    /// or None for off. Invalid values load as off.
    pub voice_synth_to: Option<u16>,
    /// The ATVV voice bridge (remote microphone audio -> virtual device).
    pub voice: VoiceSettings,
    pub window: WindowPrefs,
    /// UI language (`System` follows the OS). Files saved before this field
    /// existed load as `System` through the serde default.
    pub language: Language,
}

/// Voice bridge preferences. `gain_db` follows the hardware-verified
/// implementation's default (+10 dB); values outside ±24 dB clamp there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Whether the BLE voice session task runs.
    pub enabled: bool,
    /// The virtual audio device play endpoint name (e.g.
    /// `CABLE Input (VB-Audio Virtual Cable)`), or None until chosen.
    pub output_device: Option<String>,
    /// Post-decode gain in dB.
    pub gain_db: f32,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            output_device: None,
            gain_db: 10.0,
        }
    }
}

/// Window preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPrefs {
    /// None follows the system; Some(true) forces dark, Some(false) light.
    pub dark: Option<bool>,
}

impl AppSettings {
    /// The voice synthesis target after validation: off unless the stored
    /// usage is one the agent whitelist accepts.
    pub fn valid_voice_synth(&self) -> Option<u16> {
        self.voice_synth_to.filter(|u| VOICE_SYNTHABLE_USAGES.contains(u))
    }
}

pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

pub fn backup_path(dir: &Path) -> PathBuf {
    dir.join(format!("{SETTINGS_FILE}{BACKUP_SUFFIX}"))
}

/// The app runtime directory: `$LOCALAPPDATA\RemoteMicRune`. The
/// `REMOTE_MIC_RUNE_DIR` environment variable overrides it (tests and
/// portable setups). Returns None when LOCALAPPDATA is unset (non-Windows
/// hosts without the override); callers then run without persistence.
pub fn app_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("REMOTE_MIC_RUNE_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(rc_protocol::consts::APP_DIR_NAME))
}

/// What loading found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub settings: AppSettings,
    /// Set when the file needed fixing (corrupt file backed up, invalid
    /// value turned off). Shown once in the UI, translated at display time.
    pub warning: Option<LoadWarning>,
}

/// Why loading had to step in. Translated through
/// [`LoadWarning::message`] when the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadWarning {
    /// The stored voice-synth usage was not in the validated list, so it
    /// has been turned off.
    InvalidVoiceSynth,
    /// The file was malformed: it has been backed up to settings.json.bak
    /// and defaults are in effect. `err` is the serde error text.
    CorruptBackedUp { err: String },
    /// The file was malformed and backing it up failed; defaults are in
    /// effect and the broken file is still in place.
    CorruptBackupFailed { err: String, why: BackupFail },
}

impl LoadWarning {
    /// The user-facing message in the UI's language. The embedded serde/OS
    /// error text stays English (it is technical detail, like any OS error).
    pub fn message(&self, lang: Lang) -> String {
        match self {
            LoadWarning::InvalidVoiceSynth => tr_fmt!(
                lang,
                "Voice synthesis target in settings.json is not in the validated list; it has been turned off."
            ),
            LoadWarning::CorruptBackedUp { err } => tr_fmt!(
                lang,
                "settings.json was malformed ({}); it has been backed up to settings.json.bak and defaults are in effect.",
                err
            ),
            LoadWarning::CorruptBackupFailed { err, why } => {
                let why = why.message(lang);
                tr_fmt!(lang, "settings.json is malformed ({}) and backing it up failed: {}", err, why)
            }
        }
    }
}

/// Why backing a corrupt file up failed, translated through
/// [`BackupFail::message`] when the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupFail {
    ReplaceFailed { path: String, err: String },
    MoveAsideFailed { err: String },
}

impl BackupFail {
    pub fn message(&self, lang: Lang) -> String {
        match self {
            BackupFail::ReplaceFailed { path, err } => {
                tr_fmt!(lang, "cannot replace {}: {}", path, err)
            }
            BackupFail::MoveAsideFailed { err } => tr_fmt!(lang, "cannot move it aside: {}", err),
        }
    }
}

/// Why saving failed, translated through [`SaveError::message`] when the UI
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    CreateDirFailed { path: String, err: String },
    SerializeFailed { err: String },
    WriteFailed { path: String, err: String },
    ReplaceFailed { path: String, err: String },
}

impl SaveError {
    pub fn message(&self, lang: Lang) -> String {
        match self {
            SaveError::CreateDirFailed { path, err } => {
                tr_fmt!(lang, "cannot create {}: {}", path, err)
            }
            SaveError::SerializeFailed { err } => {
                tr_fmt!(lang, "cannot serialize settings: {}", err)
            }
            SaveError::WriteFailed { path, err } => tr_fmt!(lang, "cannot write {}: {}", path, err),
            SaveError::ReplaceFailed { path, err } => {
                tr_fmt!(lang, "cannot replace {}: {}", path, err)
            }
        }
    }
}

/// Load settings from `dir`, falling back to defaults. A malformed file is
/// renamed to `settings.json.bak` (replacing any previous backup) and
/// defaults are returned with a warning.
pub fn load(dir: &Path) -> Loaded {
    let path = settings_path(dir);
    let Ok(text) = fs::read_to_string(&path) else {
        // Missing or unreadable file: start fresh either way.
        return Loaded { settings: AppSettings::default(), warning: None };
    };
    match serde_json::from_str::<AppSettings>(&text) {
        Ok(settings) => {
            let mut settings = settings;
            let mut warning = None;
            let valid = settings.valid_voice_synth();
            if settings.voice_synth_to != valid {
                settings.voice_synth_to = valid;
                warning = Some(LoadWarning::InvalidVoiceSynth);
            }
            Loaded { settings, warning }
        }
        Err(err) => {
            let settings = AppSettings::default();
            match backup_corrupt_file(dir, &path) {
                Ok(()) => Loaded {
                    settings,
                    warning: Some(LoadWarning::CorruptBackedUp {
                        err: err.to_string(),
                    }),
                },
                Err(why) => Loaded {
                    settings,
                    warning: Some(LoadWarning::CorruptBackupFailed {
                        err: err.to_string(),
                        why,
                    }),
                },
            }
        }
    }
}

/// Replace any previous backup, then move the corrupt file aside.
fn backup_corrupt_file(dir: &Path, path: &Path) -> Result<(), BackupFail> {
    let backup = backup_path(dir);
    if backup.exists() {
        fs::remove_file(&backup).map_err(|e| BackupFail::ReplaceFailed {
            path: backup.display().to_string(),
            err: e.to_string(),
        })?;
    }
    fs::rename(path, &backup).map_err(|e| BackupFail::MoveAsideFailed { err: e.to_string() })
}

/// Write settings atomically enough for a small config: create the directory
/// if needed, write a temp file, then rename over the target.
pub fn save(dir: &Path, settings: &AppSettings) -> Result<(), SaveError> {
    fs::create_dir_all(dir).map_err(|e| SaveError::CreateDirFailed {
        path: dir.display().to_string(),
        err: e.to_string(),
    })?;
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| SaveError::SerializeFailed { err: e.to_string() })?;
    let path = settings_path(dir);
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    fs::write(&tmp, json).map_err(|e| SaveError::WriteFailed {
        path: tmp.display().to_string(),
        err: e.to_string(),
    })?;
    fs::rename(&tmp, &path).map_err(|e| SaveError::ReplaceFailed {
        path: path.display().to_string(),
        err: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mappings::{vk, ButtonAction, MediaKey, VKey};

    fn unique_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rc-app-test-{name}-{}-{}", std::process::id(), std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn sample() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.capture_enabled = true;
        settings.voice_synth_to = Some(0x00E6);
        settings.window.dark = Some(true);
        settings.language = Language::Zh;
        settings.mappings.set_action(
            rc_protocol::RemoteButton::Home,
            Some(ButtonAction::MediaKey { key: MediaKey::PlayPause }),
        );
        settings
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let dir = unique_dir("roundtrip");
        save(&dir, &sample()).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.settings, sample());
        assert_eq!(loaded.warning, None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_gesture_columns_load_without_a_warning() {
        let dir = unique_dir("legacygestures");
        fs::create_dir_all(&dir).unwrap();
        let old = r#"{
            "capture_enabled": true,
            "mappings": {
                "buttons": {
                    "home": {
                        "single": {"type": "shortcut", "chord": [17, 116]},
                        "double": {"type": "media_key", "key": "mute"},
                        "long": {"type": "disabled"},
                        "hold_repeat": null
                    }
                }
            }
        }"#;
        fs::write(settings_path(&dir), old).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.warning, None);
        assert_eq!(
            loaded.settings.mappings.action_for(rc_protocol::RemoteButton::Home),
            Some(&ButtonAction::Shortcut {
                chord: vec![vk::CONTROL, vk::F5]
            })
        );
        // The removed columns are gone the next time the file is written.
        save(&dir, &loaded.settings).unwrap();
        let saved = fs::read_to_string(settings_path(&dir)).unwrap();
        assert!(!saved.contains("\"double\""));
        assert!(!saved.contains("\"long\""));
        assert!(!saved.contains("\"hold_repeat\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_files_without_a_language_field_load_as_system() {
        let dir = unique_dir("nolang");
        fs::create_dir_all(&dir).unwrap();
        // The shape a pre-language build wrote: no `language` key at all.
        let old = r#"{"capture_enabled": true, "mappings": {"buttons": {}}}"#;
        fs::write(settings_path(&dir), old).unwrap();
        let loaded = load(&dir);
        assert_eq!(
            loaded.settings.language,
            Language::System,
            "serde default applies"
        );
        assert_eq!(
            loaded.settings.capture_enabled, true,
            "the rest of the file still loads"
        );
        assert_eq!(loaded.warning, None);
        // An explicit language survives the same load.
        fs::write(settings_path(&dir), r#"{"language": "zh"}"#).unwrap();
        assert_eq!(load(&dir).settings.language, Language::Zh);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_defaults_without_warning() {
        let dir = unique_dir("missing");
        let loaded = load(&dir);
        assert_eq!(loaded.settings, AppSettings::default());
        assert_eq!(loaded.warning, None);
    }

    #[test]
    fn malformed_file_is_backed_up_and_defaults_load() {
        let dir = unique_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let broken = "{\"capture_enabled\": true, \"mappings\": {";
        fs::write(settings_path(&dir), broken).unwrap();

        let loaded = load(&dir);
        assert_eq!(loaded.settings, AppSettings::default());
        let Some(LoadWarning::CorruptBackedUp { err }) = &loaded.warning else {
            panic!("corruption must warn: {:?}", loaded.warning);
        };
        let expected_err = serde_json::from_str::<AppSettings>(&broken)
            .unwrap_err()
            .to_string();
        assert_eq!(
            *err, expected_err,
            "the serde error text is carried verbatim"
        );

        let backup = backup_path(&dir);
        assert_eq!(fs::read_to_string(&backup).unwrap(), broken);
        assert!(!settings_path(&dir).exists(), "the corrupt file moved aside");

        // Loading again is quiet: the corrupt file is already gone.
        let again = load(&dir);
        assert_eq!(again.warning, None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_shape_json_is_treated_as_corrupt() {
        let dir = unique_dir("wrongshape");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), r#"{"capture_enabled": "yes"}"#).unwrap();
        let loaded = load(&dir);
        assert!(loaded.warning.is_some());
        assert!(backup_path(&dir).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_previous_backup_is_replaced_when_new_corruption_arrives() {
        let dir = unique_dir("twice");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), "first bad").unwrap();
        load(&dir);
        fs::write(settings_path(&dir), "second bad").unwrap();
        load(&dir);
        assert_eq!(fs::read_to_string(backup_path(&dir)).unwrap(), "second bad");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_voice_synth_target_loads_as_off() {
        let dir = unique_dir("synth");
        let mut settings = sample();
        settings.voice_synth_to = Some(0x1234); // not in VOICE_SYNTHABLE_USAGES
        save(&dir, &settings).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.settings.voice_synth_to, None);
        assert_eq!(loaded.warning, Some(LoadWarning::InvalidVoiceSynth));
        // A whitelist target survives the same check.
        let mut ok = AppSettings::default();
        ok.voice_synth_to = Some(0x00E2); // VK_LMENU, in VOICE_SYNTHABLE_USAGES
        assert_eq!(ok.valid_voice_synth(), Some(0x00E2));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn voice_settings_default_when_absent_and_round_trip() {
        // A settings.json written before the voice bridge existed loads
        // with the defaults (off, no device, +10 dB) and no warning.
        let dir = unique_dir("voice-absent");
        let mut old = sample();
        save(&dir, &old).unwrap();
        let on_disk = fs::read_to_string(settings_path(&dir)).unwrap();
        // Simulate the pre-voice file: drop the field entirely.
        let mut value: serde_json::Value =
            serde_json::from_str(&on_disk).expect("saved settings are valid json");
        value
            .as_object_mut()
            .expect("settings root is an object")
            .remove("voice");
        fs::write(settings_path(&dir), value.to_string()).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.settings.voice, VoiceSettings::default());
        assert_eq!(loaded.warning, None);

        // A configured bridge survives a save/load round trip.
        old.voice = VoiceSettings {
            enabled: true,
            output_device: Some("CABLE Input (VB-Audio Virtual Cable)".into()),
            gain_db: 14.0,
        };
        save(&dir, &old).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.settings.voice, old.voice);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn warning_and_save_errors_translate_per_language() {
        let warning = LoadWarning::CorruptBackedUp {
            err: "expected ident".into(),
        };
        assert!(warning
            .message(Lang::En)
            .starts_with("settings.json was malformed"));
        assert!(warning
            .message(Lang::Zh)
            .starts_with("settings.json 格式错误"));
        let save_err = SaveError::CreateDirFailed {
            path: "/x".into(),
            err: "denied".into(),
        };
        assert_eq!(save_err.message(Lang::Zh), "无法创建 /x：denied");
        let backup_err = BackupFail::MoveAsideFailed { err: "busy".into() };
        assert_eq!(backup_err.message(Lang::Zh), "无法将其移开：busy");
    }
}
