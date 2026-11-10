//! Button mapping data model: the app-side shape of `rc-mapping`'s
//! [`ButtonMappings`] (`VKey`, `ButtonAction`, `ButtonMappings`), mirrored
//! locally so the UI can persist and edit settings without depending on the
//! injection crate at render time.
//!
//! Each button has exactly one action, fired on every press. The `single`
//! field name keeps old `settings.json` files readable: a file written by a
//! build with double/long/repeat columns still loads its single action and
//! ignores the removed columns, which disappear on the next save.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use rc_protocol::RemoteButton;

/// A Windows virtual-key code (the low 16 bits of the `VK_*` constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VKey(pub u16);

/// Canonical `VK_*` codes used by the chord parser and formatter. The table
/// mirrors the Windows key set; host builds and tests reference only a
/// subset, so unused entries are expected.
#[allow(dead_code)]
pub mod vk {
    use super::VKey;

    pub const SHIFT: VKey = VKey(0x10);
    pub const LSHIFT: VKey = VKey(0xA0);
    pub const RSHIFT: VKey = VKey(0xA1);
    pub const CONTROL: VKey = VKey(0x11);
    pub const LCONTROL: VKey = VKey(0xA2);
    pub const RCONTROL: VKey = VKey(0xA3);
    pub const MENU: VKey = VKey(0x12); // Alt
    pub const LMENU: VKey = VKey(0xA4);
    pub const RMENU: VKey = VKey(0xA5);
    pub const LWIN: VKey = VKey(0x5B);
    pub const RWIN: VKey = VKey(0x5C);
    pub const BACK: VKey = VKey(0x08);
    pub const TAB: VKey = VKey(0x09);
    pub const RETURN: VKey = VKey(0x0D);
    pub const ESCAPE: VKey = VKey(0x1B);
    pub const SPACE: VKey = VKey(0x20);
    pub const PRIOR: VKey = VKey(0x21); // Page Up
    pub const NEXT: VKey = VKey(0x22); // Page Down
    pub const END: VKey = VKey(0x23);
    pub const HOME: VKey = VKey(0x24);
    pub const LEFT: VKey = VKey(0x25);
    pub const UP: VKey = VKey(0x26);
    pub const RIGHT: VKey = VKey(0x27);
    pub const DOWN: VKey = VKey(0x28);
    pub const INSERT: VKey = VKey(0x2D);
    pub const DELETE: VKey = VKey(0x2E);
    pub const NUMPAD0: VKey = VKey(0x60);
    pub const NUMPAD9: VKey = VKey(0x69);
    pub const MULTIPLY: VKey = VKey(0x6A);
    pub const ADD: VKey = VKey(0x6B);
    pub const SEPARATOR: VKey = VKey(0x6C);
    pub const SUBTRACT: VKey = VKey(0x6D);
    pub const DECIMAL: VKey = VKey(0x6E);
    pub const DIVIDE: VKey = VKey(0x6F);
    pub const F1: VKey = VKey(0x70);
    pub const F5: VKey = VKey(0x74);
    pub const F12: VKey = VKey(0x7B);
    pub const F24: VKey = VKey(0x87);
    pub const OEM_1: VKey = VKey(0xBA); // ; :
    pub const OEM_PLUS: VKey = VKey(0xBB); // = +
    pub const OEM_COMMA: VKey = VKey(0xBC); // , <
    pub const OEM_MINUS: VKey = VKey(0xBD); // - _
    pub const OEM_PERIOD: VKey = VKey(0xBE); // . >
    pub const OEM_2: VKey = VKey(0xBF); // / ?
    pub const OEM_3: VKey = VKey(0xC0); // ` ~
    pub const OEM_4: VKey = VKey(0xDB); // [ {
    pub const OEM_5: VKey = VKey(0xDC); // \ |
    pub const OEM_6: VKey = VKey(0xDD); // ] }
    pub const OEM_7: VKey = VKey(0xDE); // ' "
}

/// The media keys the V1 picker offers (`rc-mapping`'s MediaKey action).
/// Display labels live at the `tr(` call sites in `ui.rs`/`views.rs`, keyed
/// by the English wording; this model stays label-free.
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

/// Mouse button for the `MouseClick` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// One configured action for a button (rc-mapping's action enum).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    Disabled,
    Shortcut {
        chord: Vec<VKey>,
    },
    MediaKey {
        key: MediaKey,
    },
    MouseClick {
        button: MouseButton,
    },
    MouseMove {
        dx: i32,
        dy: i32,
    },
    Scroll {
        delta: i32,
    },
}

/// All configured actions for one button. The name and `single` field keep
/// the old settings shape; newly saved files carry only this one action.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonActions {
    pub single: Option<ButtonAction>,
}

impl ButtonActions {
    pub fn action(&self) -> Option<&ButtonAction> {
        self.single.as_ref()
    }

    /// A `Disabled` action counts as unconfigured: it exists so a hand-edited
    /// settings file can spell out "does nothing" explicitly.
    pub fn is_configured(&self) -> bool {
        self.single
            .as_ref()
            .is_some_and(|action| !matches!(action, ButtonAction::Disabled))
    }
}

/// Per-button mappings, serialized as JSON in settings.json
/// (rc-mapping's `ButtonMappings`, keyed by [`RemoteButton::name`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonMappings {
    pub buttons: BTreeMap<String, ButtonActions>,
}

impl ButtonMappings {
    /// The configured action for `button`, or `None` when the mapping is
    /// absent or explicitly `Disabled`.
    pub fn action_for(&self, button: RemoteButton) -> Option<&ButtonAction> {
        self.buttons
            .get(button.name())
            .and_then(|actions| actions.action())
            .filter(|action| !matches!(action, ButtonAction::Disabled))
    }

    pub fn set_action(&mut self, button: RemoteButton, action: Option<ButtonAction>) {
        let entry = self.buttons.entry(button.name().to_string()).or_default();
        entry.single = action;
        if !entry.is_configured() {
            // Keep the file tidy: an entry with no action (or only Disabled)
            // goes away.
            self.buttons.remove(button.name());
        }
    }

    /// Bit per button (ordinal bit set) for every button with a configured
    /// action. This is the `mapped_mask` the capture set derives from; it is
    /// recomputed on every mappings change.
    pub fn mapped_mask(&self) -> u64 {
        let mut mask = 0u64;
        for button in rc_protocol::ALL_BUTTONS {
            if self
                .buttons
                .get(button.name())
                .is_some_and(|actions| actions.is_configured())
            {
                mask |= 1u64 << button.ordinal();
            }
        }
        mask
    }

    /// Number of buttons with a configured action.
    pub fn configured_buttons(&self) -> usize {
        self.mapped_mask().count_ones() as usize
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
    fn empty_mappings_map_to_an_empty_mask() {
        assert_eq!(ButtonMappings::default().mapped_mask(), 0);
    }

    #[test]
    fn mapped_mask_sets_one_bit_per_mapped_button() {
        let mut m = ButtonMappings::default();
        m.set_action(RemoteButton::Home, Some(shortcut(&[vk::F5])));
        m.set_action(
            RemoteButton::Ok,
            Some(ButtonAction::MediaKey { key: MediaKey::Mute }),
        );
        let mask = m.mapped_mask();
        assert_eq!(mask.count_ones(), 2);
        assert_ne!(mask & (1 << RemoteButton::Home.ordinal()), 0);
        assert_ne!(mask & (1 << RemoteButton::Ok.ordinal()), 0);
        assert_eq!(mask & (1 << RemoteButton::Back.ordinal()), 0);
    }

    #[test]
    fn disabled_action_is_not_mapped() {
        let mut m = ButtonMappings::default();
        m.set_action(RemoteButton::Up, Some(ButtonAction::Disabled));
        assert_eq!(m.mapped_mask(), 0, "Disabled is an explicit no-op, not a mapping");
        // The empty entry is dropped so the settings file stays tidy.
        assert!(!m.buttons.contains_key(RemoteButton::Up.name()));
    }

    #[test]
    fn clearing_the_action_removes_the_button_entry() {
        let mut m = ButtonMappings::default();
        m.set_action(RemoteButton::Left, Some(shortcut(&[vk::END])));
        assert_eq!(m.mapped_mask().count_ones(), 1);
        m.set_action(RemoteButton::Left, None);
        assert_eq!(m.mapped_mask(), 0);
        assert!(m.buttons.is_empty());
    }

    #[test]
    fn mappings_round_trip_through_json() {
        let mut m = ButtonMappings::default();
        m.set_action(
            RemoteButton::Back,
            Some(shortcut(&[vk::CONTROL, vk::F12])),
        );
        m.set_action(
            RemoteButton::Ok,
            Some(ButtonAction::MouseClick {
                button: MouseButton::Right,
            }),
        );
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ButtonMappings>(&json).unwrap(), m);
        assert!(json.contains(r#""type":"shortcut""#));
        assert!(json.contains(r#""type":"mouse_click""#));
    }

    #[test]
    fn legacy_gesture_columns_load_as_single_and_are_dropped() {
        let json = r#"{
            "buttons": {
                "back": {
                    "single": {"type": "shortcut", "chord": [27]},
                    "double": {"type": "shortcut", "chord": [32]},
                    "long": {"type": "media_key", "key": "play_pause"},
                    "hold_repeat": {"type": "shortcut", "chord": [13]}
                }
            }
        }"#;
        let m: ButtonMappings = serde_json::from_str(json).unwrap();
        assert_eq!(
            m.action_for(RemoteButton::Back),
            Some(&shortcut(&[vk::ESCAPE]))
        );
        let saved = serde_json::to_string(&m).unwrap();
        assert!(!saved.contains("\"double\""));
        assert!(!saved.contains("\"long\""));
        assert!(!saved.contains("\"hold_repeat\""));
        assert!(saved.contains("\"single\""));
    }

    #[test]
    fn json_may_spell_out_a_disabled_action() {
        let json = r#"{"buttons":{"up":{"single":{"type":"disabled"}}}}"#;
        let m: ButtonMappings = serde_json::from_str(json).unwrap();
        assert_eq!(m.mapped_mask(), 0);
        assert_eq!(m.action_for(RemoteButton::Up), None);
    }
}
