//! Injection backend: turn a button action into synthetic input.
//!
//! The [`KeyInjector`] trait is the seam between the pure engine and the OS.
//! The production impl (Windows only) drives `SendInput`; tests use the
//! recording fake. Injected events carry the OS `LLKHF_INJECTED` flag —
//! expected and acceptable: the remote synthesizes shortcuts the target
//! app is meant to receive.

pub use crate::actions::{MediaKey, MouseClickButton, MoveDirection, ScrollDirection, VKey};

/// Injection failure surfaces. `SendInput` reports how many events landed;
/// anything short of the full batch is an error so the engine can surface
/// the failure instead of silently dropping a shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectError {
    EmptyChord,
    ChordTooLong(usize),
    DuplicateKey(VKey),
    PartialDelivery { sent: usize, expected: usize },
    Backend(String),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InjectError::EmptyChord => write!(f, "a shortcut chord must contain at least one key"),
            InjectError::ChordTooLong(len) => {
                write!(f, "a shortcut chord may contain at most {MAX_CHORD_KEYS} keys, got {len}")
            }
            InjectError::DuplicateKey(key) => {
                write!(f, "shortcut chord contains duplicate key {:#04x}", key.code())
            }
            InjectError::PartialDelivery { sent, expected } => write!(
                f,
                "SendInput delivered only {sent}/{expected} events"
            ),
            InjectError::Backend(message) => write!(f, "SendInput backend failed: {message}"),
        }
    }
}

impl std::error::Error for InjectError {}

/// Cap on the number of keys in one chord (modifiers + action key).
pub const MAX_CHORD_KEYS: usize = 6;

/// Validate a chord before injection: non-empty, within the cap, no
/// duplicate keys (a duplicated key would desync the down/up pairing).
pub fn validate_chord(chord: &[VKey]) -> Result<(), InjectError> {
    if chord.is_empty() {
        return Err(InjectError::EmptyChord);
    }
    if chord.len() > MAX_CHORD_KEYS {
        return Err(InjectError::ChordTooLong(chord.len()));
    }
    for (index, key) in chord.iter().enumerate() {
        if chord[..index].contains(key) {
            return Err(InjectError::DuplicateKey(*key));
        }
    }
    Ok(())
}

/// The injection seam. `&self` so the engine can hold it as
/// `Box<dyn KeyInjector>` and call it while shared.
pub trait KeyInjector: Send {
    /// Tap a chord: press all keys in order (modifiers first, action key
    /// last), then release them in reverse order.
    fn send_chord(&self, chord: &[VKey]) -> Result<(), InjectError>;
    /// Tap a media key.
    fn media(&self, key: MediaKey) -> Result<(), InjectError>;
    /// Click a mouse button (down + up pair).
    fn mouse_click(&self, button: MouseClickButton) -> Result<(), InjectError>;
    /// Move the pointer relatively.
    fn mouse_move(&self, dir: MoveDirection, px: i32) -> Result<(), InjectError>;
    /// Scroll the wheel.
    fn scroll(&self, dir: ScrollDirection, lines: i32) -> Result<(), InjectError>;
}

#[cfg(test)]
#[derive(Default)]
pub struct RecordingInjector {
    ops: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl RecordingInjector {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, op: String) -> Result<(), InjectError> {
        self.ops().push(op);
        Ok(())
    }

    fn ops(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        match self.ops.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn recorded(&self) -> Vec<String> {
        self.ops().clone()
    }
}

#[cfg(test)]
impl KeyInjector for RecordingInjector {
    fn send_chord(&self, chord: &[VKey]) -> Result<(), InjectError> {
        validate_chord(chord)?;
        let keys: Vec<String> = chord.iter().map(|key| format!("{:#04x}", key.code())).collect();
        self.record(format!("chord {}", keys.join(",")))
    }

    fn media(&self, key: MediaKey) -> Result<(), InjectError> {
        self.record(format!("media {key:?}"))
    }

    fn mouse_click(&self, button: MouseClickButton) -> Result<(), InjectError> {
        self.record(format!("click {button:?}"))
    }

    fn mouse_move(&self, dir: MoveDirection, px: i32) -> Result<(), InjectError> {
        self.record(format!("move {dir:?} {px}"))
    }

    fn scroll(&self, dir: ScrollDirection, lines: i32) -> Result<(), InjectError> {
        self.record(format!("scroll {dir:?} {lines}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_validation_rejects_bad_shapes() {
        assert_eq!(validate_chord(&[]), Err(InjectError::EmptyChord));
        assert_eq!(
            validate_chord(&[VKey::LWIN, VKey(0x4C), VKey(0x4C)]),
            Err(InjectError::DuplicateKey(VKey(0x4C)))
        );
        let long: Vec<VKey> = (0..=MAX_CHORD_KEYS as u16).map(VKey).collect();
        assert_eq!(
            validate_chord(&long),
            Err(InjectError::ChordTooLong(long.len()))
        );
        assert!(validate_chord(&[VKey::LWIN, VKey(0x4C)]).is_ok());
    }
}
