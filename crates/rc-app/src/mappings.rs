//! Button mapping data model: the exact API shapes rc-mapping documents
//! (`VKey`, `ButtonMappings`, `ButtonActions`, `ButtonAction`), mirrored
//! locally so rc-app compiles before the sibling crate lands.
//!
//! When `rc-mapping` exists, delete the type definitions here and re-export
//! from the crate instead (`pub use rc_mapping::{VKey, ButtonMappings, ..}`);
//! every other module keeps calling through [`crate::bridge_api`], which is
//! the only seam that must change. See `bridge_api.rs` for the full recipe.

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
    /// Picker / settings label. `const` so const option lists can call it.
    pub const fn label(self) -> &'static str {
        match self {
            MediaKey::Mute => "Media: Mute",
            MediaKey::VolumeDown => "Media: Vol -",
            MediaKey::VolumeUp => "Media: Vol +",
            MediaKey::PlayPause => "Media: Play",
            MediaKey::Prev => "Media: Prev",
            MediaKey::Next => "Media: Next",
        }
    }
}

/// Mouse button for the `MouseClick` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub const fn label(self) -> &'static str {
        match self {
            MouseButton::Left => "Mouse: Left",
            MouseButton::Right => "Mouse: Right",
            MouseButton::Middle => "Mouse: Middle",
        }
    }
}

/// One configured action for a button gesture (rc-mapping's action enum).
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

/// Which gesture slot of a button a mapping applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Single,
    Double,
    Long,
    HoldRepeat,
}

impl Slot {
    /// All slots, in display and serialization order.
    pub const ALL: [Slot; 4] = [Slot::Single, Slot::Double, Slot::Long, Slot::HoldRepeat];

    /// Short caption the UI puts above the slot's picker ("1", "2", "L", "R").
    pub fn caption(self) -> &'static str {
        match self {
            Slot::Single => "Single",
            Slot::Double => "Double",
            Slot::Long => "Long",
            Slot::HoldRepeat => "Repeat",
        }
    }

    /// 0-based index inside [`ButtonActions`]; also the UI editor key base.
    pub fn index(self) -> usize {
        match self {
            Slot::Single => 0,
            Slot::Double => 1,
            Slot::Long => 2,
            Slot::HoldRepeat => 3,
        }
    }
}

/// All configured actions for one button (rc-mapping's `ButtonActions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonActions {
    pub single: Option<ButtonAction>,
    pub double: Option<ButtonAction>,
    pub long: Option<ButtonAction>,
    pub hold_repeat: Option<ButtonAction>,
}

impl ButtonActions {
    pub fn slot(&self, slot: Slot) -> Option<&ButtonAction> {
        match slot {
            Slot::Single => self.single.as_ref(),
            Slot::Double => self.double.as_ref(),
            Slot::Long => self.long.as_ref(),
            Slot::HoldRepeat => self.hold_repeat.as_ref(),
        }
    }

    pub fn set_slot(&mut self, slot: Slot, action: Option<ButtonAction>) {
        match slot {
            Slot::Single => self.single = action,
            Slot::Double => self.double = action,
            Slot::Long => self.long = action,
            Slot::HoldRepeat => self.hold_repeat = action,
        }
    }

    /// A `Disabled` action counts as unconfigured: it exists so a hand-edited
    /// settings file can spell out "does nothing" explicitly.
    pub fn is_configured(&self) -> bool {
        Slot::ALL.iter().any(|slot| {
            self.slot(*slot).is_some_and(|a| !matches!(a, ButtonAction::Disabled))
        })
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
    pub fn actions_for(&self, button: RemoteButton) -> ButtonActions {
        self.buttons.get(button.name()).cloned().unwrap_or_default()
    }

    pub fn actions_mut(&mut self, button: RemoteButton) -> &mut ButtonActions {
        self.buttons.entry(button.name().to_string()).or_default()
    }

    pub fn slot_action(&self, button: RemoteButton, slot: Slot) -> Option<&ButtonAction> {
        self.buttons.get(button.name()).and_then(|a| a.slot(slot))
    }

    pub fn set_slot_action(&mut self, button: RemoteButton, slot: Slot, action: Option<ButtonAction>) {
        let actions = self.actions_mut(button);
        actions.set_slot(slot, action);
        if !actions.is_configured() {
            // Keep the file tidy: an entry with only Disabled/None slots goes away.
            self.buttons.remove(button.name());
        }
    }

    /// Bit per button (ordinal bit set) for every button with at least one
    /// configured action. This is the `mapped_mask` the capture set derives
    /// from; it is recomputed on every mappings change.
    pub fn mapped_mask(&self) -> u64 {
        let mut mask = 0u64;
        for button in rc_protocol::ALL_BUTTONS {
            if self.actions_for(button).is_configured() {
                mask |= 1u64 << button.ordinal();
            }
        }
        mask
    }

    /// Number of buttons with at least one configured action.
    pub fn configured_buttons(&self) -> usize {
        rc_protocol::ALL_BUTTONS
            .iter()
            .filter(|b| self.actions_for(**b).is_configured())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(keys: &[VKey]) -> ButtonAction {
        ButtonAction::Shortcut { chord: keys.to_vec() }
    }

    #[test]
    fn empty_mappings_map_to_an_empty_mask() {
        assert_eq!(ButtonMappings::default().mapped_mask(), 0);
    }

    #[test]
    fn mapped_mask_sets_one_bit_per_mapped_button() {
        let mut m = ButtonMappings::default();
        m.set_slot_action(RemoteButton::Home, Slot::Single, Some(shortcut(&[vk::F5])));
        m.set_slot_action(RemoteButton::Ok, Slot::Double, Some(ButtonAction::MediaKey { key: MediaKey::Mute }));
        let mask = m.mapped_mask();
        assert_eq!(mask.count_ones(), 2);
        assert_ne!(mask & (1 << RemoteButton::Home.ordinal()), 0);
        assert_ne!(mask & (1 << RemoteButton::Ok.ordinal()), 0);
        assert_eq!(mask & (1 << RemoteButton::Back.ordinal()), 0);
    }

    #[test]
    fn disabled_action_is_not_mapped() {
        let mut m = ButtonMappings::default();
        m.set_slot_action(RemoteButton::Up, Slot::Single, Some(ButtonAction::Disabled));
        assert_eq!(m.mapped_mask(), 0, "Disabled is an explicit no-op, not a mapping");
        // The empty entry is dropped so the settings file stays tidy.
        assert!(!m.buttons.contains_key(RemoteButton::Up.name()));
    }

    #[test]
    fn clearing_the_last_action_removes_the_button_entry() {
        let mut m = ButtonMappings::default();
        m.set_slot_action(RemoteButton::Left, Slot::Long, Some(shortcut(&[vk::END])));
        assert_eq!(m.mapped_mask().count_ones(), 1);
        m.set_slot_action(RemoteButton::Left, Slot::Long, None);
        assert_eq!(m.mapped_mask(), 0);
        assert!(m.buttons.is_empty());
    }

    #[test]
    fn mappings_round_trip_through_json() {
        let mut m = ButtonMappings::default();
        m.set_slot_action(RemoteButton::Back, Slot::Single, Some(shortcut(&[vk::CONTROL, vk::F12])));
        m.set_slot_action(RemoteButton::Back, Slot::Long, Some(ButtonAction::MediaKey { key: MediaKey::PlayPause }));
        m.set_slot_action(RemoteButton::Ok, Slot::Double, Some(ButtonAction::MouseClick { button: MouseButton::Right }));
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ButtonMappings>(&json).unwrap(), m);
        assert!(json.contains(r#""type":"shortcut""#));
        assert!(json.contains(r#""type":"media_key""#));
    }

    #[test]
    fn json_may_spell_out_disabled_slots() {
        let json = r#"{"buttons":{"up":{"single":{"type":"disabled"}}}}"#;
        let m: ButtonMappings = serde_json::from_str(json).unwrap();
        assert_eq!(m.mapped_mask(), 0);
        assert_eq!(
            m.slot_action(RemoteButton::Up, Slot::Single),
            Some(&ButtonAction::Disabled)
        );
    }
}
