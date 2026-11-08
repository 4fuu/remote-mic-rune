//! Shortcut chord text: parsing `Ctrl+Alt+F5`-style strings into VKey chords
//! and formatting chords back for display.
//!
//! Parsing rules:
//! - Tokens split on `+`, surrounding whitespace trimmed, matching is
//!   case-insensitive.
//! - Modifier tokens (`Ctrl`, `Alt`, `Shift`, `Win`) may appear in any order
//!   and are deduplicated; the stored chord is always canonical
//!   (Ctrl, Alt, Shift, Win, then the key).
//! - Exactly one non-modifier key is required; a chord of only modifiers is
//!   an error.
//! - Unknown tokens, empty tokens and more than one key are errors. The app
//!   never persists an invalid chord.

use crate::mappings::{VKey, vk};

/// Longest chord the parser accepts (4 modifiers + 1 key).
const MAX_TOKENS: usize = 5;

/// Parse a user-typed chord such as `Ctrl+Alt+F5` into its VKeys.
pub fn parse_chord(text: &str) -> Result<Vec<VKey>, String> {
    let mut modifiers: Vec<VKey> = Vec::new();
    let mut key: Option<VKey> = None;

    let tokens: Vec<&str> = text.split('+').map(str::trim).collect();
    if tokens.iter().any(|t| t.is_empty()) {
        return Err(format!("{text:?} has an empty key before or after '+'"));
    }
    if tokens.len() > MAX_TOKENS {
        return Err(format!("{text:?} has too many '+' groups (max {MAX_TOKENS})"));
    }
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some(vk::CONTROL),
            "lctrl" | "lcontrol" => Some(vk::LCONTROL),
            "rctrl" | "rcontrol" => Some(vk::RCONTROL),
            "alt" => Some(vk::MENU),
            "lalt" => Some(vk::LMENU),
            "ralt" => Some(vk::RMENU),
            "shift" => Some(vk::SHIFT),
            "lshift" => Some(vk::LSHIFT),
            "rshift" => Some(vk::RSHIFT),
            "win" | "windows" | "meta" | "super" => Some(vk::LWIN),
            "lwin" => Some(vk::LWIN),
            "rwin" => Some(vk::RWIN),
            _ => None,
        };
        if let Some(m) = modifier {
            if key.is_some() {
                return Err(format!("{text:?}: modifier {token:?} must come before the key"));
            }
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
            continue;
        }
        if key.is_some() {
            return Err(format!("{text:?} has more than one non-modifier key"));
        }
        key = Some(parse_key(token)?);
    }
    let Some(key) = key else {
        return Err(format!("{text:?} has no key, only modifiers"));
    };

    // Canonical order: Ctrl, Alt, Shift, Win, key — with the left/right
    // physical variants right after their generic spelling.
    let mut chord = Vec::with_capacity(modifiers.len() + 1);
    for m in [
        vk::CONTROL,
        vk::LCONTROL,
        vk::RCONTROL,
        vk::MENU,
        vk::LMENU,
        vk::RMENU,
        vk::SHIFT,
        vk::LSHIFT,
        vk::RSHIFT,
        vk::LWIN,
        vk::RWIN,
    ] {
        if modifiers.contains(&m) {
            chord.push(m);
        }
    }
    chord.push(key);
    Ok(chord)
}

/// Format a chord for display, e.g. `Ctrl+Alt+F5`. Empty input gives `""`.
pub fn format_chord(chord: &[VKey]) -> String {
    chord
        .iter()
        .map(|k| key_label(*k))
        .collect::<Vec<_>>()
        .join("+")
}

/// A single non-modifier key name -> VKey.
fn parse_key(token: &str) -> Result<VKey, String> {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "esc" | "escape" => Ok(vk::ESCAPE),
        "space" => Ok(vk::SPACE),
        "enter" | "return" => Ok(vk::RETURN),
        "tab" => Ok(vk::TAB),
        "backspace" => Ok(vk::BACK),
        "del" | "delete" => Ok(vk::DELETE),
        "ins" | "insert" => Ok(vk::INSERT),
        "home" => Ok(vk::HOME),
        "end" => Ok(vk::END),
        "pgup" | "pageup" => Ok(vk::PRIOR),
        "pgdn" | "pagedown" => Ok(vk::NEXT),
        "left" => Ok(vk::LEFT),
        "up" => Ok(vk::UP),
        "right" => Ok(vk::RIGHT),
        "down" => Ok(vk::DOWN),
        "num0" | "numpad0" => Ok(vk::NUMPAD0),
        "num1" | "numpad1" => Ok(VKey(vk::NUMPAD0.0 + 1)),
        "num2" | "numpad2" => Ok(VKey(vk::NUMPAD0.0 + 2)),
        "num3" | "numpad3" => Ok(VKey(vk::NUMPAD0.0 + 3)),
        "num4" | "numpad4" => Ok(VKey(vk::NUMPAD0.0 + 4)),
        "num5" | "numpad5" => Ok(VKey(vk::NUMPAD0.0 + 5)),
        "num6" | "numpad6" => Ok(VKey(vk::NUMPAD0.0 + 6)),
        "num7" | "numpad7" => Ok(VKey(vk::NUMPAD0.0 + 7)),
        "num8" | "numpad8" => Ok(VKey(vk::NUMPAD0.0 + 8)),
        "num9" | "numpad9" => Ok(vk::NUMPAD9),
        "num+" | "numpad+" | "add" => Ok(vk::ADD),
        "num-" | "numpad-" | "subtract" => Ok(vk::SUBTRACT),
        "num*" | "numpad*" | "multiply" => Ok(vk::MULTIPLY),
        "num/" | "numpad/" | "divide" => Ok(vk::DIVIDE),
        "," | "comma" => Ok(vk::OEM_COMMA),
        "." | "period" => Ok(vk::OEM_PERIOD),
        ";" | "semicolon" => Ok(vk::OEM_1),
        "=" | "equal" => Ok(vk::OEM_PLUS),
        "-" => Ok(vk::OEM_MINUS),
        "/" | "slash" => Ok(vk::OEM_2),
        "`" | "backquote" => Ok(vk::OEM_3),
        "[" => Ok(vk::OEM_4),
        "\\" | "backslash" => Ok(vk::OEM_5),
        "]" => Ok(vk::OEM_6),
        "'" => Ok(vk::OEM_7),
        other => {
            let mut chars = other.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if c.is_ascii_lowercase() {
                    return Ok(VKey(0x41 + (c as u16 - 'a' as u16)));
                }
                if c.is_ascii_digit() {
                    return Ok(VKey(0x30 + (c as u16 - '0' as u16)));
                }
            }
            if let Some(rest) = other.strip_prefix('f') {
                if let Ok(n) = rest.parse::<u16>() {
                    if (1..=24).contains(&n) {
                        return Ok(VKey(vk::F1.0 + n - 1));
                    }
                    return Err(format!("{token:?} is out of range (F1-F24)"));
                }
            }
            Err(format!("{token:?} is not a key this app knows"))
        }
    }
}

/// Human name for a VKey, matching the parser's accepted spellings.
pub fn key_label(key: VKey) -> String {
    let named: Option<&'static str> = match key {
        vk::CONTROL => Some("Ctrl"),
        vk::LCONTROL => Some("LCtrl"),
        vk::RCONTROL => Some("RCtrl"),
        vk::MENU => Some("Alt"),
        vk::LMENU => Some("LAlt"),
        vk::RMENU => Some("RAlt"),
        vk::SHIFT => Some("Shift"),
        vk::LSHIFT => Some("LShift"),
        vk::RSHIFT => Some("RShift"),
        vk::LWIN | vk::RWIN => Some("Win"),
        vk::ESCAPE => Some("Esc"),
        vk::SPACE => Some("Space"),
        vk::RETURN => Some("Enter"),
        vk::TAB => Some("Tab"),
        vk::BACK => Some("Backspace"),
        vk::DELETE => Some("Delete"),
        vk::INSERT => Some("Insert"),
        vk::HOME => Some("Home"),
        vk::END => Some("End"),
        vk::PRIOR => Some("PageUp"),
        vk::NEXT => Some("PageDown"),
        vk::LEFT => Some("Left"),
        vk::UP => Some("Up"),
        vk::RIGHT => Some("Right"),
        vk::DOWN => Some("Down"),
        vk::OEM_COMMA => Some(","),
        vk::OEM_PERIOD => Some("."),
        vk::OEM_1 => Some(";"),
        vk::OEM_PLUS => Some("="),
        vk::OEM_MINUS => Some("-"),
        vk::OEM_2 => Some("/"),
        vk::OEM_3 => Some("`"),
        vk::OEM_4 => Some("["),
        vk::OEM_5 => Some("\\"),
        vk::OEM_6 => Some("]"),
        vk::OEM_7 => Some("'"),
        vk::ADD => Some("Num+"),
        vk::SUBTRACT => Some("Num-"),
        vk::MULTIPLY => Some("Num*"),
        vk::DIVIDE => Some("Num/"),
        vk::DECIMAL => Some("Num."),
        _ => None,
    };
    if let Some(label) = named {
        return label.to_string();
    }
    let VKey(c) = key;
    if (0x30..=0x39).contains(&c) {
        return char::from_u32(c as u32).unwrap_or('?').to_string();
    }
    if (0x41..=0x5A).contains(&c) {
        return char::from_u32(c as u32).unwrap_or('?').to_string();
    }
    if (vk::F1.0..=vk::F24.0).contains(&c) {
        return format!("F{}", c - vk::F1.0 + 1);
    }
    if (vk::NUMPAD0.0..=vk::NUMPAD9.0).contains(&c) {
        return format!("Num{}", c - vk::NUMPAD0.0);
    }
    format!("VK_{:04X}", c)
}

/// True when `key` is one of the modifier keys (Ctrl/Alt/Shift/Win).
#[cfg(test)]
fn is_modifier(key: VKey) -> bool {
    matches!(key, vk::CONTROL | vk::MENU | vk::SHIFT | vk::LWIN | vk::RWIN)
        || matches!(key.0, 0xA0..=0xA5) // left/right Ctrl, Shift and Alt variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_key_parses_without_modifiers() {
        assert_eq!(parse_chord("F5").unwrap(), vec![vk::F5]);
        assert_eq!(parse_chord("a").unwrap(), vec![VKey(0x41)]);
        assert_eq!(parse_chord("7").unwrap(), vec![VKey(0x37)]);
    }

    #[test]
    fn modifiers_and_key_parse_in_canonical_order() {
        assert_eq!(
            parse_chord("Ctrl+Alt+F5").unwrap(),
            vec![vk::CONTROL, vk::MENU, vk::F5]
        );
        // Side-specific modifiers parse, label and re-parse (a settings file
        // may carry the physical VK_LCONTROL rather than generic VK_CONTROL).
        let sided = parse_chord("LCtrl+F5").unwrap();
        assert_eq!(sided, vec![vk::LCONTROL, vk::F5]);
        assert_eq!(format_chord(&sided), "LCtrl+F5");
        assert_eq!(parse_chord(&format_chord(&sided)).unwrap(), sided);
        assert_eq!(format_chord(&[vk::RMENU, vk::F5]), "RAlt+F5");
        // Any spelling, any order, same result.
        assert_eq!(
            parse_chord("alt+CTRL+f5").unwrap(),
            parse_chord("Ctrl+Alt+F5").unwrap()
        );
        assert_eq!(
            parse_chord("Shift+Win+Down").unwrap(),
            vec![vk::SHIFT, vk::LWIN, vk::DOWN]
        );
        assert_eq!(parse_chord("Control+P").unwrap(), vec![vk::CONTROL, VKey(0x50)]);
    }

    #[test]
    fn duplicate_modifiers_are_deduplicated() {
        assert_eq!(parse_chord("Ctrl+Ctrl+A").unwrap(), vec![vk::CONTROL, VKey(0x41)]);
    }

    #[test]
    fn named_and_numpad_keys_parse() {
        assert_eq!(parse_chord("Esc").unwrap(), vec![vk::ESCAPE]);
        assert_eq!(parse_chord("Space").unwrap(), vec![vk::SPACE]);
        assert_eq!(parse_chord("PageUp").unwrap(), vec![vk::PRIOR]);
        assert_eq!(parse_chord("Num4").unwrap(), vec![VKey(0x64)]);
        assert_eq!(parse_chord("Ctrl+Num/").unwrap(), vec![vk::CONTROL, vk::DIVIDE]);
    }

    #[test]
    fn empty_and_partial_input_is_rejected() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("   ").is_err());
        assert!(parse_chord("Ctrl+").is_err());
        assert!(parse_chord("+A").is_err());
        assert!(parse_chord("Ctrl++A").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse_chord("Bogus").is_err());
        assert!(parse_chord("Ctrl+Bogus").is_err());
        assert!(parse_chord("F25").is_err());
        assert!(parse_chord("Ctrl+Shift").is_err(), "modifiers alone are not a chord");
    }

    #[test]
    fn a_key_after_a_modifier_key_is_rejected() {
        assert!(parse_chord("Ctrl+F1+Alt").is_err());
    }

    #[test]
    fn format_round_trips_canonical_chords() {
        for text in ["Ctrl+Alt+F5", "Shift+Down", "Ctrl+Shift+Esc", "Win+Num4", "F12"] {
            assert_eq!(format_chord(&parse_chord(text).unwrap()), text, "round trip {text}");
        }
    }

    #[test]
    fn format_handles_empty_and_unknown_keys() {
        assert_eq!(format_chord(&[]), "");
        assert_eq!(format_chord(&[VKey(0xDEAD)]), "VK_DEAD");
    }

    #[test]
    fn modifier_detection_covers_both_win_keys() {
        assert!(is_modifier(vk::LWIN));
        assert!(is_modifier(vk::RWIN));
        assert!(!is_modifier(vk::F5));
    }
}
