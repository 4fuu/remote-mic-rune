//! Button action model: what one button does on every press.
//!
//! Pure data + serde (JSON). Config files key buttons by their stable
//! lowercase names ("back", "ok", ...); unknown button names and unknown
//! action types are rejected with clear errors so a hand-edited config never
//! silently half-applies.

use std::collections::BTreeMap;

use rc_protocol::RemoteButton;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A Windows virtual-key code (`VK_*`). Letters and digits use their ASCII
/// codes; the constants below cover everything the mapping model needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VKey(pub u16);

impl VKey {
    pub const RETURN: VKey = VKey(0x0D);
    pub const ESCAPE: VKey = VKey(0x1B);
    pub const BACK: VKey = VKey(0x08);
    pub const TAB: VKey = VKey(0x09);
    pub const SPACE: VKey = VKey(0x20);
    pub const LEFT: VKey = VKey(0x25);
    pub const UP: VKey = VKey(0x26);
    pub const RIGHT: VKey = VKey(0x27);
    pub const DOWN: VKey = VKey(0x28);
    pub const LSHIFT: VKey = VKey(0xA0);
    pub const RSHIFT: VKey = VKey(0xA1);
    pub const LCONTROL: VKey = VKey(0xA2);
    pub const RCONTROL: VKey = VKey(0xA3);
    pub const LMENU: VKey = VKey(0xA4);
    pub const RMENU: VKey = VKey(0xA5);
    pub const LWIN: VKey = VKey(0x5B);
    pub const RWIN: VKey = VKey(0x5C);
    pub const VOLUME_MUTE: VKey = VKey(0xAD);
    pub const VOLUME_DOWN: VKey = VKey(0xAE);
    pub const VOLUME_UP: VKey = VKey(0xAF);
    pub const MEDIA_NEXT_TRACK: VKey = VKey(0xB0);
    pub const MEDIA_PREV_TRACK: VKey = VKey(0xB1);
    pub const MEDIA_PLAY_PAUSE: VKey = VKey(0xB3);

    /// Virtual-key code for an ASCII letter (A-Z) or digit (0-9), which is
    /// the character's own code. Other bytes return `None`.
    pub fn from_ascii(byte: u8) -> Option<VKey> {
        match byte {
            b'A'..=b'Z' | b'0'..=b'9' => Some(VKey(u16::from(byte))),
            b'a'..=b'z' => Some(VKey(u16::from(byte - b'a' + b'A'))),
            _ => None,
        }
    }

    pub fn code(self) -> u16 {
        self.0
    }
}

/// Media key acted on by the OS media stack (consumer-page equivalents
/// surfaced as virtual keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKey {
    Mute,
    VolumeDown,
    VolumeUp,
    PlayPause,
    Prev,
    Next,
}

impl MediaKey {
    pub fn vk(self) -> VKey {
        match self {
            MediaKey::Mute => VKey::VOLUME_MUTE,
            MediaKey::VolumeDown => VKey::VOLUME_DOWN,
            MediaKey::VolumeUp => VKey::VOLUME_UP,
            MediaKey::PlayPause => VKey::MEDIA_PLAY_PAUSE,
            MediaKey::Prev => VKey::MEDIA_PREV_TRACK,
            MediaKey::Next => VKey::MEDIA_NEXT_TRACK,
        }
    }
}

/// Mouse button for a click action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseClickButton {
    Left,
    Right,
    Middle,
}

/// Pointer move direction (relative pixel move).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Wheel scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
}

/// One configured action for a button. Serde is hand-rolled on the
/// deserialize side: internally tagged serde enums cannot reject unknown
/// fields, and a config must never half-apply silently.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    #[default]
    Disabled,
    /// Keyboard shortcut: `chord` is modifiers first, action key last.
    Shortcut { chord: Vec<VKey> },
    MediaKey { key: MediaKey },
    MouseClick { button: MouseClickButton },
    /// Relative pointer move in pixels.
    MouseMove { dir: MoveDirection, px: i32 },
    /// Wheel scroll in "lines".
    Scroll { dir: ScrollDirection, lines: i32 },
}

impl ButtonAction {
    pub fn is_configured(&self) -> bool {
        *self != ButtonAction::Disabled
    }

    /// Whether the chord switches to the secure desktop (Win+L in any
    /// modifier order, exactly two keys). Such actions must wait for the
    /// physical release before injecting.
    pub fn shortcut_locks_workstation(chord: &[VKey]) -> bool {
        chord.len() == 2
            && chord.contains(&Self::L)
            && (chord.contains(&VKey::LWIN) || chord.contains(&VKey::RWIN))
    }

    const L: VKey = VKey(0x4C);
}

fn action_missing_field(kind: &str, field: &str) -> String {
    format!("`{kind}` action is missing field `{field}`")
}

fn action_unknown_field(kind: &str, field: &str) -> String {
    format!("unknown field `{field}` for `{kind}` action")
}

fn action_bad_field(kind: &str, field: &str, expected: &str) -> String {
    format!("`{kind}` action field `{field}` must be {expected}")
}

/// Deserialize a [`ButtonAction`] from its JSON object form with strict
/// field checking (unknown fields are clear errors, not silently ignored).
fn deserialize_action(value: &serde_json::Value) -> Result<ButtonAction, String> {
    let Some(map) = value.as_object() else {
        return Err("button action must be a JSON object with a `type` field".to_owned());
    };
    let kind = map
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "button action is missing string field `type`".to_owned())?;
    let reject_unknown = |allowed: &[&str]| -> Result<(), String> {
        for key in map.keys() {
            if key != "type" && !allowed.contains(&key.as_str()) {
                return Err(action_unknown_field(kind, key));
            }
        }
        Ok(())
    };
    let chord = || -> Result<Vec<VKey>, String> {
        match map.get("chord") {
            Some(serde_json::Value::Array(items)) => {
                let mut keys = Vec::with_capacity(items.len());
                for item in items {
                    let VKey(code) = serde_json::from_value::<VKey>(item.clone()).map_err(|_| {
                        action_bad_field(kind, "chord", "an array of virtual-key codes")
                    })?;
                    keys.push(VKey(code));
                }
                Ok(keys)
            }
            _ => Err(action_missing_field(kind, "chord")),
        }
    };
    let enum_field = |name: &str, allowed: &[&str]| -> Result<&str, String> {
        let value = map
            .get(name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| action_missing_field(kind, name))?;
        if allowed.contains(&value) {
            Ok(value)
        } else {
            Err(action_bad_field(kind, name, &format!("one of {}", allowed.join(", "))))
        }
    };
    let int_field = |name: &str| -> Result<i32, String> {
        match map.get(name).and_then(|v| v.as_i64()) {
            Some(value) if value >= i32::MIN as i64 && value <= i32::MAX as i64 => Ok(value as i32),
            _ => Err(action_bad_field(kind, name, "an integer")),
        }
    };
    match kind {
        "disabled" => {
            reject_unknown(&[])?;
            Ok(ButtonAction::Disabled)
        }
        "shortcut" => {
            reject_unknown(&["chord"])?;
            Ok(ButtonAction::Shortcut { chord: chord()? })
        }
        "media_key" => {
            reject_unknown(&["key"])?;
            let key = enum_field(
                "key",
                &["mute", "volume_down", "volume_up", "play_pause", "prev", "next"],
            )?;
            Ok(ButtonAction::MediaKey {
                key: serde_json::from_value::<MediaKey>(serde_json::Value::String(key.to_owned()))
                    .expect("checked against the allowed list"),
            })
        }
        "mouse_click" => {
            reject_unknown(&["button"])?;
            let button = enum_field("button", &["left", "right", "middle"])?;
            Ok(ButtonAction::MouseClick {
                button: serde_json::from_value::<MouseClickButton>(serde_json::Value::String(
                    button.to_owned(),
                ))
                .expect("checked against the allowed list"),
            })
        }
        "mouse_move" => {
            reject_unknown(&["dir", "px"])?;
            let dir = enum_field("dir", &["up", "down", "left", "right"])?;
            let px = int_field("px")?;
            Ok(ButtonAction::MouseMove {
                dir: serde_json::from_value::<MoveDirection>(serde_json::Value::String(
                    dir.to_owned(),
                ))
                .expect("checked against the allowed list"),
                px,
            })
        }
        "scroll" => {
            reject_unknown(&["dir", "lines"])?;
            let dir = enum_field("dir", &["up", "down"])?;
            let lines = int_field("lines")?;
            Ok(ButtonAction::Scroll {
                dir: serde_json::from_value::<ScrollDirection>(serde_json::Value::String(
                    dir.to_owned(),
                ))
                .expect("checked against the allowed list"),
                lines,
            })
        }
        other => Err(format!("unknown action type `{other}`")),
    }
}

impl<'de> Deserialize<'de> for ButtonAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        deserialize_action(&value).map_err(serde::de::Error::custom)
    }
}

/// The one action a button fires on every press.
///
/// The `single` field keeps the existing settings shape: files written by
/// builds with gesture columns still load, and the old `double` / `long` /
/// `hold_repeat` keys are ignored (and dropped on the next save).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonActions {
    pub single: ButtonAction,
}

impl ButtonActions {
    /// Whether this button owns a captured action: `Disabled` and a missing
    /// entry both mean "behave natively".
    pub fn is_configured(&self) -> bool {
        self.single.is_configured()
    }
}

/// Button-keyed action table serialized with the stable lowercase button
/// names ("back", "ok", ...). Unknown names are deserialization errors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ButtonMap(pub BTreeMap<RemoteButton, ButtonActions>);

impl Serialize for ButtonMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let raw: BTreeMap<&str, &ButtonActions> = self
            .0
            .iter()
            .map(|(button, actions)| (button.name(), actions))
            .collect();
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ButtonMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, ButtonActions>::deserialize(deserializer)?;
        let mut map = BTreeMap::new();
        for (key, actions) in raw {
            let button = rc_protocol::ALL_BUTTONS
                .iter()
                .find(|button| button.name() == key)
                .ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "unknown button name `{key}` (expected one of: {})",
                        rc_protocol::ALL_BUTTONS
                            .iter()
                            .map(|button| button.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            map.insert(*button, actions);
        }
        Ok(ButtonMap(map))
    }
}

/// The whole button mapping configuration. `Default` is disabled with every
/// action `Disabled` (full fail-open: the remote behaves native).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonMappings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub buttons: ButtonMap,
}

impl ButtonMappings {
    pub fn actions(&self, button: RemoteButton) -> ButtonActions {
        self.buttons.0.get(&button).cloned().unwrap_or_default()
    }

    /// The configured action for `button`, or `None` when the mappings are
    /// disabled or the button has no action.
    pub fn action(&self, button: RemoteButton) -> Option<&ButtonAction> {
        if !self.enabled {
            return None;
        }
        self.buttons
            .0
            .get(&button)
            .map(|actions| &actions.single)
            .filter(|action| action.is_configured())
    }

    /// Bit mask of buttons with a configured action, indexed by
    /// [`RemoteButton::ordinal`]. Empty while disabled: the capture set the
    /// bridge requests derives from this.
    pub fn mapped_mask(&self) -> u64 {
        if !self.enabled {
            return 0;
        }
        let mut mask = 0u64;
        for (button, actions) in &self.buttons.0 {
            if actions.is_configured() {
                mask |= 1u64 << button.ordinal();
            }
        }
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(keys: &[VKey]) -> ButtonAction {
        ButtonAction::Shortcut {
            chord: keys.to_vec(),
        }
    }

    #[test]
    fn vkey_constants_match_windows_values() {
        assert_eq!(VKey::LWIN.code(), 0x5B);
        assert_eq!(VKey::RWIN.code(), 0x5C);
        assert_eq!(VKey::LMENU.code(), 0xA4);
        assert_eq!(VKey::RMENU.code(), 0xA5);
        assert_eq!(VKey::LCONTROL.code(), 0xA2);
        assert_eq!(VKey::LSHIFT.code(), 0xA0);
        assert_eq!(VKey::VOLUME_MUTE.code(), 0xAD);
        assert_eq!(VKey::VOLUME_DOWN.code(), 0xAE);
        assert_eq!(VKey::VOLUME_UP.code(), 0xAF);
        assert_eq!(VKey::MEDIA_PLAY_PAUSE.code(), 0xB3);
        assert_eq!(VKey::MEDIA_PREV_TRACK.code(), 0xB1);
        assert_eq!(VKey::MEDIA_NEXT_TRACK.code(), 0xB0);
        assert_eq!(VKey::LEFT.code(), 0x25);
        assert_eq!(VKey::RIGHT.code(), 0x27);
        assert_eq!(VKey::UP.code(), 0x26);
        assert_eq!(VKey::DOWN.code(), 0x28);
        assert_eq!(VKey::RETURN.code(), 0x0D);
        assert_eq!(VKey::ESCAPE.code(), 0x1B);
        assert_eq!(VKey::BACK.code(), 0x08);
        assert_eq!(VKey::from_ascii(b'A'), Some(VKey(0x41)));
        assert_eq!(VKey::from_ascii(b'7'), Some(VKey(0x37)));
        assert_eq!(VKey::from_ascii(b' '), None);
    }

    #[test]
    fn mappings_round_trip_with_button_name_keys() {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            RemoteButton::Back,
            ButtonActions {
                single: shortcut(&[VKey::ESCAPE]),
            },
        );
        mappings.buttons.0.insert(
            RemoteButton::Up,
            ButtonActions {
                single: ButtonAction::MouseMove {
                    dir: MoveDirection::Up,
                    px: 20,
                },
            },
        );
        mappings.buttons.0.insert(
            RemoteButton::Ok,
            ButtonActions {
                single: ButtonAction::MouseClick {
                    button: MouseClickButton::Left,
                },
                ..ButtonActions::default()
            },
        );
        let json = serde_json::to_string_pretty(&mappings).expect("serialize");
        assert!(json.contains("\"back\""));
        assert!(json.contains("\"type\": \"shortcut\""));
        let parsed: ButtonMappings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, mappings);
    }

    #[test]
    fn default_is_fully_disabled_and_disabled_flag_off() {
        let mappings = ButtonMappings::default();
        assert!(!mappings.enabled);
        assert_eq!(mappings.mapped_mask(), 0);
        for button in rc_protocol::ALL_BUTTONS {
            let actions = mappings.actions(button);
            assert_eq!(actions, ButtonActions::default());
            assert!(!actions.is_configured());
            assert_eq!(actions.single, ButtonAction::Disabled);
        }
        // Empty object parses to the default.
        let parsed: ButtonMappings = serde_json::from_str("{}").expect("parse");
        assert_eq!(parsed, mappings);
    }

    #[test]
    fn legacy_gesture_columns_load_as_single_and_are_dropped() {
        let json = r#"{
            "enabled": true,
            "buttons": {
                "back": {
                    "single": {"type": "shortcut", "chord": [27]},
                    "double": {"type": "shortcut", "chord": [32]},
                    "long": {"type": "disabled"},
                    "hold_repeat": "single"
                }
            }
        }"#;
        let parsed: ButtonMappings = serde_json::from_str(json).expect("legacy file loads");
        assert_eq!(
            parsed.actions(RemoteButton::Back).single,
            shortcut(&[VKey::ESCAPE])
        );
        let saved = serde_json::to_string(&parsed).expect("serialize");
        assert!(!saved.contains("\"double\""));
        assert!(!saved.contains("\"long\""));
        assert!(!saved.contains("\"hold_repeat\""));
        assert!(saved.contains("\"single\""));
    }

    #[test]
    fn unknown_fields_and_names_are_rejected_with_clear_errors() {
        // Unknown struct field.
        let error = serde_json::from_str::<ButtonMappings>(r#"{"enabled":true,"wat":1}"#)
            .expect_err("unknown struct field must fail");
        assert!(error.to_string().contains("unknown field"), "{error}");
        // Unknown button name.
        let error = serde_json::from_str::<ButtonMappings>(r#"{"buttons":{"bck":{}}}"#)
            .expect_err("unknown button must fail");
        assert!(
            error.to_string().contains("unknown button name `bck`"),
            "{error}"
        );
        // Unknown action type.
        let error = serde_json::from_str::<ButtonAction>(r#"{"type":"explode"}"#)
            .expect_err("unknown action type must fail");
        assert!(
            error.to_string().contains("unknown action type `explode`"),
            "{error}"
        );
        // Unknown field inside an action.
        let error = serde_json::from_str::<ButtonAction>(
            r#"{"type":"shortcut","chord":[76],"speed":5}"#,
        )
        .expect_err("unknown action field must fail");
        assert!(
            error.to_string().contains("unknown field `speed` for `shortcut`"),
            "{error}"
        );
        // Missing required field.
        let error = serde_json::from_str::<ButtonAction>(r#"{"type":"shortcut"}"#)
            .expect_err("missing chord must fail");
        assert!(
            error
                .to_string()
                .contains("`shortcut` action is missing field `chord`"),
            "{error}"
        );
        // Disabled action must not carry extra fields.
        let error = serde_json::from_str::<ButtonAction>(r#"{"type":"disabled","chord":[1]}"#)
            .expect_err("disabled with fields must fail");
        assert!(
            error.to_string().contains("unknown field `chord` for `disabled`"),
            "{error}"
        );
    }

    #[test]
    fn lock_workstation_detection() {
        assert!(ButtonAction::shortcut_locks_workstation(&[
            VKey::LWIN,
            VKey(0x4C)
        ]));
        assert!(ButtonAction::shortcut_locks_workstation(&[
            VKey(0x4C),
            VKey::RWIN
        ]));
        assert!(!ButtonAction::shortcut_locks_workstation(&[
            VKey::LCONTROL,
            VKey(0x4C)
        ]));
        assert!(!ButtonAction::shortcut_locks_workstation(&[
            VKey::LWIN,
            VKey(0x4C),
            VKey::LSHIFT
        ]));
    }

    #[test]
    fn mapped_mask_sets_bits_for_configured_buttons_only() {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            RemoteButton::Home,
            ButtonActions {
                single: shortcut(&[VKey::RETURN]),
                ..ButtonActions::default()
            },
        );
        // All-Disabled entry does not count as mapped.
        mappings
            .buttons
            .0
            .insert(RemoteButton::Tv, ButtonActions::default());
        let expected = 1u64 << RemoteButton::Home.ordinal();
        assert_eq!(mappings.mapped_mask(), expected);
        mappings.enabled = false;
        assert_eq!(mappings.mapped_mask(), 0);
    }
}
