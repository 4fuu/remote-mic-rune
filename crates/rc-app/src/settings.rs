//! settings.json under `%LOCALAPPDATA%\RemoteMicRune\`: button mappings plus
//! app preferences. Loading is fault-tolerant: a malformed file is backed up
//! to settings.json.bak and the app starts fresh, never crashing or losing
//! the broken file for inspection.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use rc_protocol::buttons::VOICE_SYNTHABLE_USAGES;

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
    pub window: WindowPrefs,
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
    /// Set when the file was corrupt: it has been moved to settings.json.bak
    /// and defaults are in effect. Shown once in the UI.
    pub warning: Option<String>,
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
                warning = Some("Voice synthesis target in settings.json is not in the \
                                validated list; it has been turned off."
                    .to_string());
            }
            Loaded { settings, warning }
        }
        Err(err) => {
            let moved = backup_corrupt_file(dir, &path);
            let note = match moved {
                Ok(()) => format!(
                    "settings.json was malformed ({err}); it has been backed up to \
                     {SETTINGS_FILE}{BACKUP_SUFFIX} and defaults are in effect."
                ),
                Err(backup_err) => format!(
                    "settings.json is malformed ({err}) and backing it up failed: {backup_err}"
                ),
            };
            Loaded { settings: AppSettings::default(), warning: Some(note) }
        }
    }
}

/// Replace any previous backup, then move the corrupt file aside.
fn backup_corrupt_file(dir: &Path, path: &Path) -> Result<(), String> {
    let backup = backup_path(dir);
    if backup.exists() {
        fs::remove_file(&backup).map_err(|e| format!("cannot replace {}: {e}", backup.display()))?;
    }
    fs::rename(path, &backup).map_err(|e| format!("cannot move it aside: {e}"))
}

/// Write settings atomically enough for a small config: create the directory
/// if needed, write a temp file, then rename over the target.
pub fn save(dir: &Path, settings: &AppSettings) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(settings).map_err(|e| format!("cannot serialize settings: {e}"))?;
    let path = settings_path(dir);
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mappings::{ButtonAction, MediaKey, Slot};

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
        settings.mappings.set_slot_action(
            rc_protocol::RemoteButton::Home,
            Slot::Single,
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
        let warning = loaded.warning.expect("corruption must warn");
        assert!(warning.contains("backed up"), "warning: {warning}");

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
        assert!(loaded.warning.is_some(), "the clamped value is reported");
        // A whitelist target survives the same check.
        let mut ok = AppSettings::default();
        ok.voice_synth_to = Some(0x00E2); // VK_LMENU, in VOICE_SYNTHABLE_USAGES
        assert_eq!(ok.valid_voice_synth(), Some(0x00E2));
        fs::remove_dir_all(&dir).ok();
    }
}
