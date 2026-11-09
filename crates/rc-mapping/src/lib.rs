//! rc-mapping — gesture recognizer, button mapping model, and injection for
//! remote-mic-rune.
//!
//! Layers, bottom up:
//! - [`actions`]: pure serde model of what a gesture does (JSON config).
//! - [`gestures`]: pure per-button state machine turning press/release edges
//!   into single/double/long gestures (timings in `rc_protocol::consts`).
//! - [`inject`]: the [`KeyInjector`](inject::KeyInjector) seam; the Windows
//!   [`SendInputInjector`] drives `SendInput`.
//! - [`engine`]: [`MappingEngine`] wiring edges -> gestures -> injector.
//!
//! Driver model: the engine has no internal thread. The application calls
//! [`MappingEngine::handle`] for every bridge edge event and
//! [`MappingEngine::tick`] on a fixed cadence (25 ms in the current driver
//! model). See `engine.rs` for details.

pub mod actions;
pub mod engine;
pub mod gestures;
pub mod inject;

#[cfg(target_os = "windows")]
pub mod win_inject;

pub use actions::{
    ButtonAction, ButtonActions, ButtonMap, ButtonMappings, ButtonTrigger, MediaKey,
    MouseClickButton, MoveDirection, ScrollDirection, VKey,
};
pub use engine::{EngineEvent, MappingEngine};
pub use gestures::{
    configs_from_mappings, FiredGesture, GestureConfig, GestureConfigs, GestureKind,
    GestureRecognizer, DOUBLE_CLICK_WINDOW, LONG_PRESS_THRESHOLD, REPEAT_START_DELAY,
};
pub use inject::{InjectError, KeyInjector, MAX_CHORD_KEYS};

#[cfg(target_os = "windows")]
pub use win_inject::SendInputInjector;
