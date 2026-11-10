//! rc-mapping — one action per button, plus injection for remote-mic-rune.
//!
//! Layers, bottom up:
//! - [`actions`]: pure serde model of what a button does (JSON config).
//! - [`inject`]: the [`KeyInjector`](inject::KeyInjector) seam; the Windows
//!   [`SendInputInjector`] drives `SendInput`.
//! - [`engine`]: [`MappingEngine`] wiring press edges -> injector.
//!
//! Driver model: the engine has no internal thread and no timers. The
//! application calls [`MappingEngine::handle`] for every bridge edge event;
//! a press fires the button's one action immediately. See `engine.rs` for the
//! workstation-lock release deferral.

pub mod actions;
pub mod engine;
pub mod inject;

#[cfg(target_os = "windows")]
pub mod win_inject;

pub use actions::{
    ButtonAction, ButtonActions, ButtonMap, ButtonMappings, MediaKey, MouseClickButton,
    MoveDirection, ScrollDirection, VKey,
};
pub use engine::{EngineEvent, MappingEngine};
pub use inject::{InjectError, KeyInjector, MAX_CHORD_KEYS};

#[cfg(target_os = "windows")]
pub use win_inject::SendInputInjector;
