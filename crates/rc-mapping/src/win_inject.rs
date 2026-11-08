//! Windows `SendInput` implementation of [`KeyInjector`].
//!
//! Chords are submitted as one batch: all key-downs in order (modifiers
//! first, action key last), then key-ups in reverse order, so the OS sees a
//! well-formed press/release pair. `SendInput`'s return value is the number
//! of events it consumed — anything short of the full batch is surfaced as
//! [`InjectError::PartialDelivery`].
//!
//! Injected events carry the OS injected flag (`LLKHF_INJECTED`) by design:
//! the remote's target apps are meant to receive these synthetic shortcuts.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};

use crate::actions::{MediaKey, MouseClickButton, MoveDirection, ScrollDirection, VKey};
use crate::inject::{validate_chord, InjectError, KeyInjector};

/// One wheel notch in Windows mouse-wheel units.
pub const WHEEL_DELTA: i32 = 120;

/// Production injector. Stateless: every call is an independent
/// `SendInput` submission.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendInputInjector;

impl SendInputInjector {
    pub fn new() -> Self {
        Self
    }

    fn submit(inputs: &[INPUT]) -> Result<(), InjectError> {
        let expected = inputs.len();
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) } as usize;
        if sent == expected {
            Ok(())
        } else {
            Err(InjectError::PartialDelivery { sent, expected })
        }
    }
}

fn key_input(key: VKey, is_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key.code()),
                wScan: 0,
                dwFlags: if is_up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn tap_key(key: VKey) -> [INPUT; 2] {
    [key_input(key, false), key_input(key, true)]
}

fn click_flags(button: MouseClickButton) -> [(windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS, bool); 2] {
    match button {
        MouseClickButton::Left => [
            (MOUSEEVENTF_LEFTDOWN, false),
            (MOUSEEVENTF_LEFTUP, false),
        ],
        MouseClickButton::Right => [
            (MOUSEEVENTF_RIGHTDOWN, false),
            (MOUSEEVENTF_RIGHTUP, false),
        ],
        MouseClickButton::Middle => [
            (MOUSEEVENTF_MIDDLEDOWN, false),
            (MOUSEEVENTF_MIDDLEUP, false),
        ],
    }
}

fn mouse_input(dx: i32, dy: i32, flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl KeyInjector for SendInputInjector {
    fn send_chord(&self, chord: &[VKey]) -> Result<(), InjectError> {
        validate_chord(chord)?;
        let mut inputs = Vec::with_capacity(chord.len() * 2);
        for key in chord {
            inputs.push(key_input(*key, false));
        }
        // Release in reverse order so modifiers lift after the action key.
        for key in chord.iter().rev() {
            inputs.push(key_input(*key, true));
        }
        Self::submit(&inputs)
    }

    fn media(&self, key: MediaKey) -> Result<(), InjectError> {
        Self::submit(&tap_key(key.vk()))
    }

    fn mouse_click(&self, button: MouseClickButton) -> Result<(), InjectError> {
        let [down, up] = click_flags(button);
        Self::submit(&[
            mouse_input(0, 0, down.0, 0),
            mouse_input(0, 0, up.0, 0),
        ])
    }

    fn mouse_move(&self, dir: MoveDirection, px: i32) -> Result<(), InjectError> {
        let (dx, dy) = match dir {
            MoveDirection::Up => (0, -px),
            MoveDirection::Down => (0, px),
            MoveDirection::Left => (-px, 0),
            MoveDirection::Right => (px, 0),
        };
        Self::submit(&[mouse_input(dx, dy, MOUSEEVENTF_MOVE, 0)])
    }

    fn scroll(&self, dir: ScrollDirection, lines: i32) -> Result<(), InjectError> {
        let delta = match dir {
            ScrollDirection::Up => WHEEL_DELTA * lines,
            ScrollDirection::Down => -(WHEEL_DELTA * lines),
        };
        Self::submit(&[mouse_input(0, 0, MOUSEEVENTF_WHEEL, delta as u32)])
    }
}
