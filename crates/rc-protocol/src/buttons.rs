//! Semantic buttons and their HID usages (report id 0x01 of the RC003
//! keyboard collection, VID 0x2717 / PID 0x32B8).
//!
//! The voice key (usage 0x003E) is intentionally NOT part of the button
//! mapping system: it is the remote's ATVV voice-session key, and here it
//! is only ever used for report-layer synthesis (see
//! [`VOICE_KEY_HID_USAGE`]).

use serde::{Deserialize, Serialize};

/// The 13 semantic buttons the enhanced capture chain can own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RemoteButton {
    Back,
    Ok,
    Tv,
    Home,
    Right,
    Left,
    Down,
    Up,
    Menu,
    Power,
    VolumeMute,
    VolumeUp,
    VolumeDown,
}

/// Stable order of all buttons. Masks, logs and UI all rely on this order.
pub const ALL_BUTTONS: [RemoteButton; 13] = [
    RemoteButton::Back,
    RemoteButton::Ok,
    RemoteButton::Tv,
    RemoteButton::Home,
    RemoteButton::Right,
    RemoteButton::Left,
    RemoteButton::Down,
    RemoteButton::Up,
    RemoteButton::Menu,
    RemoteButton::Power,
    RemoteButton::VolumeMute,
    RemoteButton::VolumeUp,
    RemoteButton::VolumeDown,
];

/// Report-id-1 usages of every capturable button. This table is the whitelist
/// the agent enforces on `targets` commands.
pub const ENHANCED_CAPTURE_BUTTON_USAGES: [(RemoteButton, u16); 13] = [
    (RemoteButton::Back, 0x00F1),
    (RemoteButton::Ok, 0x0028),
    (RemoteButton::Tv, 0x0035),
    (RemoteButton::Home, 0x004A),
    (RemoteButton::Right, 0x004F),
    (RemoteButton::Left, 0x0050),
    (RemoteButton::Down, 0x0051),
    (RemoteButton::Up, 0x0052),
    (RemoteButton::Menu, 0x0065),
    (RemoteButton::Power, 0x0066),
    (RemoteButton::VolumeMute, 0x007F),
    (RemoteButton::VolumeUp, 0x0080),
    (RemoteButton::VolumeDown, 0x0081),
];

/// Voice key usage. Never remapped by the mapping engine; only the report
/// layer may synthesize it into a keyboard usage (whitelist below).
pub const VOICE_KEY_HID_USAGE: u16 = 0x003E;

/// Synthesis targets validated on real hardware:
/// 0x00E6 -> VK_RMENU (right alt), 0x00E2 -> VK_LMENU (left alt).
/// Any other target usage must be rejected (per-key verification required
/// before extending this list).
pub const VOICE_SYNTHABLE_USAGES: &[u16] = &[0x00E2, 0x00E6];

/// Buttons captured unconditionally while enhanced capture is enabled.
///
/// Back / VolumeUp / VolumeDown are dropped by `kbdhid`'s HID->VK mapping, so
/// no native path can ever see them; capturing them costs nothing observable
/// and gives press feedback before any mapping is configured.
pub const ALWAYS_CAPTURED_BUTTONS: [RemoteButton; 3] = [
    RemoteButton::Back,
    RemoteButton::VolumeUp,
    RemoteButton::VolumeDown,
];

impl RemoteButton {
    /// Position in [`ALL_BUTTONS`]; the bit index in usage masks.
    pub fn ordinal(self) -> u32 {
        ALL_BUTTONS
            .iter()
            .position(|b| *b == self)
            .expect("ALL_BUTTONS covers every RemoteButton") as u32
    }

    pub fn from_ordinal(ord: u32) -> Option<Self> {
        ALL_BUTTONS.get(ord as usize).copied()
    }

    /// HID usage inside keyboard report 0x01.
    pub fn usage(self) -> u16 {
        ENHANCED_CAPTURE_BUTTON_USAGES
            .iter()
            .find(|(b, _)| *b == self)
            .expect("every button has a usage")
            .1
    }

    pub fn from_usage(usage: u16) -> Option<Self> {
        ENHANCED_CAPTURE_BUTTON_USAGES
            .iter()
            .find(|(_, u)| *u == usage)
            .map(|(b, _)| *b)
    }

    /// Human-readable name, stable for logs and config files.
    pub fn name(self) -> &'static str {
        match self {
            RemoteButton::Back => "back",
            RemoteButton::Ok => "ok",
            RemoteButton::Tv => "tv",
            RemoteButton::Home => "home",
            RemoteButton::Right => "right",
            RemoteButton::Left => "left",
            RemoteButton::Down => "down",
            RemoteButton::Up => "up",
            RemoteButton::Menu => "menu",
            RemoteButton::Power => "power",
            RemoteButton::VolumeMute => "volume_mute",
            RemoteButton::VolumeUp => "volume_up",
            RemoteButton::VolumeDown => "volume_down",
        }
    }
}

/// Validate that every usage is a known, non-duplicated button usage.
/// Usage 0 is rejected: in a report slot it means "empty slot", never a key.
pub fn validate_usage_set(usages: &[u16]) -> bool {
    if usages.is_empty() {
        return true;
    }
    usages
        .iter()
        .all(|u| *u != 0 && RemoteButton::from_usage(*u).is_some())
        && {
            let mut sorted = usages.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            sorted.len() == usages.len()
        }
}

/// The capture set the app requests while enhanced capture is on:
/// always-captured buttons plus every currently mapped button.
pub fn capture_set(mapped_mask: u64) -> Vec<u16> {
    let mut usages: Vec<u16> = Vec::new();
    for button in ALWAYS_CAPTURED_BUTTONS {
        usages.push(button.usage());
    }
    for button in ALL_BUTTONS {
        if mapped_mask & (1u64 << button.ordinal()) != 0 {
            let u = button.usage();
            if !usages.contains(&u) {
                usages.push(u);
            }
        }
    }
    usages.sort_unstable();
    usages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_table_round_trips() {
        for button in ALL_BUTTONS {
            assert_eq!(RemoteButton::from_usage(button.usage()), Some(button));
            assert_eq!(RemoteButton::from_ordinal(button.ordinal()), Some(button));
        }
        assert_eq!(RemoteButton::from_usage(VOICE_KEY_HID_USAGE), None);
    }

    #[test]
    fn validate_usage_set_rejects_bad_input() {
        assert!(validate_usage_set(&[]));
        assert!(validate_usage_set(&[0x00F1, 0x0080]));
        assert!(!validate_usage_set(&[0])); // empty-slot marker is not a key
        assert!(!validate_usage_set(&[0x00F1, 0x00F1])); // duplicates
        assert!(!validate_usage_set(&[0x003E])); // voice key is not a button
        assert!(!validate_usage_set(&[0x1234])); // unknown usage
    }

    #[test]
    fn capture_set_always_includes_dropped_three() {
        let set = capture_set(0);
        for b in ALWAYS_CAPTURED_BUTTONS {
            assert!(set.contains(&b.usage()), "missing {:?}", b);
        }
        let mapped = 1u64 << RemoteButton::Home.ordinal();
        let set = capture_set(mapped);
        assert!(set.contains(&RemoteButton::Home.usage()));
        assert_eq!(set.len(), 4); // 3 always + home
    }
}
