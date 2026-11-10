//! The mapping engine: bridge press edges in, the button's action injected.
//!
//! One action per button, fired on the press edge. There is no double-click
//! window, long-press threshold or hold-to-repeat: a second press is simply a
//! second action, and holding the button fires exactly once.
//!
//! Thread model (chosen design): the engine is a plain struct with **no
//! internal thread**. The owning application feeds every [`EngineEvent`] from
//! the bridge channel into [`MappingEngine::handle`] as it arrives.
//!
//! The one exception to press-edge firing is a workstation-lock shortcut
//! (Win+L): injecting it before the physical release can race the raw
//! DOWN/UP pair, so it is held until the button is released. That is a
//! transport detail, not a second gesture: the button still has one action
//! and fires it once.

use std::collections::BTreeMap;

use rc_protocol::RemoteButton;

use crate::actions::{ButtonAction, ButtonMappings};
use crate::inject::{InjectError, KeyInjector};

/// Input events the bridge delivers to the mapping engine. E lines carry
/// absolute pressed sets; the bridge derives these edges itself.
///
/// This mirrors `rc_bridge::EngineEvent` (same three variants): the crates
/// stay decoupled, and the application maps between the two when it pumps
/// the bridge channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    Press(RemoteButton),
    Release(RemoteButton),
    ReleaseAll,
}

/// Executes each button's action against the configured injection backend.
pub struct MappingEngine {
    mappings: ButtonMappings,
    injector: Box<dyn KeyInjector>,
    /// Lock-workstation actions waiting for their physical release.
    deferred: BTreeMap<RemoteButton, ButtonAction>,
    last_error: Option<InjectError>,
    injection_errors: u64,
    injected_actions: u64,
}

impl std::fmt::Debug for MappingEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappingEngine")
            .field("mappings", &self.mappings)
            .field("injection_errors", &self.injection_errors)
            .field("injected_actions", &self.injected_actions)
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl MappingEngine {
    /// Create an engine around an injector, starting from the default
    /// (disabled) mapping: nothing is injected until
    /// [`MappingEngine::reconfigure`] installs a real configuration.
    pub fn new(injector: Box<dyn KeyInjector>) -> Self {
        Self {
            mappings: ButtonMappings::default(),
            injector,
            deferred: BTreeMap::new(),
            last_error: None,
            injection_errors: 0,
            injected_actions: 0,
        }
    }

    /// Install a new mapping configuration. A pending deferred action is
    /// dropped without firing.
    pub fn reconfigure(&mut self, mappings: &ButtonMappings) {
        self.mappings = mappings.clone();
        self.deferred.clear();
    }

    pub fn mappings(&self) -> &ButtonMappings {
        &self.mappings
    }

    /// Drop pending deferred actions without firing (bridge teardown /
    /// watchdog events).
    pub fn release_all(&mut self) {
        self.deferred.clear();
    }

    /// Feed one edge event.
    pub fn handle(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Press(button) => {
                let Some(action) = self.mappings.action(button).cloned() else {
                    return;
                };
                if matches!(&action, ButtonAction::Shortcut { chord }
                    if ButtonAction::shortcut_locks_workstation(chord))
                {
                    self.deferred.entry(button).or_insert(action);
                } else {
                    self.execute(&action);
                }
            }
            EngineEvent::Release(button) => {
                if let Some(action) = self.deferred.remove(&button) {
                    self.execute(&action);
                }
            }
            EngineEvent::ReleaseAll => {
                // Ownership loss / bridge teardown is not a physical release:
                // drop a pending lock action instead of firing it.
                self.deferred.clear();
            }
        }
    }

    /// Last injection failure, if any (surfaced for diagnostics).
    pub fn last_error(&self) -> Option<&InjectError> {
        self.last_error.as_ref()
    }

    pub fn injection_errors(&self) -> u64 {
        self.injection_errors
    }

    pub fn injected_actions(&self) -> u64 {
        self.injected_actions
    }

    fn execute(&mut self, action: &ButtonAction) {
        let result = match action {
            ButtonAction::Disabled => return,
            ButtonAction::Shortcut { chord } => self.injector.send_chord(chord),
            ButtonAction::MediaKey { key } => self.injector.media(*key),
            ButtonAction::MouseClick { button } => self.injector.mouse_click(*button),
            ButtonAction::MouseMove { dir, px } => self.injector.mouse_move(*dir, *px),
            ButtonAction::Scroll { dir, lines } => self.injector.scroll(*dir, *lines),
        };
        self.injected_actions += 1;
        if let Err(error) = result {
            self.injection_errors += 1;
            self.last_error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ButtonActions, ButtonMap, MediaKey, MouseClickButton, VKey};
    use crate::inject::RecordingInjector;

    fn chord(key: VKey) -> ButtonAction {
        ButtonAction::Shortcut { chord: vec![key] }
    }

    fn mapping(button: RemoteButton, action: ButtonAction) -> ButtonMappings {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings
            .buttons
            .0
            .insert(button, ButtonActions { single: action });
        mappings
    }

    #[test]
    fn press_injects_once_and_release_adds_nothing() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        engine.handle(EngineEvent::Release(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 1);
        assert_eq!(engine.injection_errors(), 0);
    }

    #[test]
    fn double_press_fires_the_same_action_twice() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        for _ in 0..2 {
            engine.handle(EngineEvent::Press(RemoteButton::Ok));
            engine.handle(EngineEvent::Release(RemoteButton::Ok));
        }
        assert_eq!(engine.injected_actions(), 2);
    }

    #[test]
    fn holding_fires_once_and_never_a_long_action() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        // No timers exist in the engine: nothing can fire while held.
        engine.handle(EngineEvent::Release(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 1);
    }

    #[test]
    fn lock_workstation_waits_for_release_and_fires_once() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(
            RemoteButton::Power,
            ButtonAction::Shortcut {
                chord: vec![VKey::LWIN, VKey(0x4C)],
            },
        ));
        engine.handle(EngineEvent::Press(RemoteButton::Power));
        assert_eq!(engine.injected_actions(), 0, "Win+L waits for the release");
        engine.handle(EngineEvent::Release(RemoteButton::Power));
        assert_eq!(engine.injected_actions(), 1);

        // A release-all edge (ownership loss / bridge teardown) also drops the
        // pending action without firing it.
        engine.handle(EngineEvent::Press(RemoteButton::Power));
        assert_eq!(engine.injected_actions(), 1);
        engine.handle(EngineEvent::ReleaseAll);
        assert_eq!(engine.injected_actions(), 1);
    }

    #[test]
    fn teardown_release_all_drops_pending_without_firing() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(
            RemoteButton::Power,
            ButtonAction::Shortcut {
                chord: vec![VKey::LWIN, VKey(0x4C)],
            },
        ));
        engine.handle(EngineEvent::Press(RemoteButton::Power));
        engine.release_all();
        assert_eq!(engine.injected_actions(), 0);
    }

    #[test]
    fn disabled_and_unmapped_buttons_do_nothing() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 0);

        engine.reconfigure(&mapping(RemoteButton::Ok, ButtonAction::Disabled));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        engine.handle(EngineEvent::Release(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 0);
    }

    #[test]
    fn reconfigure_swaps_behavior_mid_flight() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 1);

        engine.reconfigure(&mapping(
            RemoteButton::Ok,
            ButtonAction::MouseClick {
                button: MouseClickButton::Left,
            },
        ));
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 2);

        engine.reconfigure(&ButtonMappings::default());
        engine.handle(EngineEvent::Press(RemoteButton::Ok));
        engine.handle(EngineEvent::Release(RemoteButton::Ok));
        assert_eq!(engine.injected_actions(), 2);
    }

    #[test]
    fn media_actions_are_injected() {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(&mapping(
            RemoteButton::Home,
            ButtonAction::MediaKey {
                key: MediaKey::PlayPause,
            },
        ));
        engine.handle(EngineEvent::Press(RemoteButton::Home));
        assert_eq!(engine.injected_actions(), 1);
    }

    #[test]
    fn injection_failures_are_surfaced() {
        struct FailingInjector;
        impl KeyInjector for FailingInjector {
            fn send_chord(&self, _chord: &[VKey]) -> Result<(), InjectError> {
                Err(InjectError::PartialDelivery {
                    sent: 1,
                    expected: 2,
                })
            }
            fn media(&self, _key: MediaKey) -> Result<(), InjectError> {
                Ok(())
            }
            fn mouse_click(&self, _button: MouseClickButton) -> Result<(), InjectError> {
                Ok(())
            }
            fn mouse_move(&self, _dir: crate::actions::MoveDirection, _px: i32) -> Result<(), InjectError> {
                Ok(())
            }
            fn scroll(&self, _dir: crate::actions::ScrollDirection, _lines: i32) -> Result<(), InjectError> {
                Ok(())
            }
        }
        let mut engine = MappingEngine::new(Box::new(FailingInjector));
        engine.reconfigure(&mapping(RemoteButton::Back, chord(VKey::ESCAPE)));
        engine.handle(EngineEvent::Press(RemoteButton::Back));
        assert_eq!(engine.injection_errors(), 1);
        assert_eq!(
            engine.last_error(),
            Some(&InjectError::PartialDelivery {
                sent: 1,
                expected: 2
            })
        );
    }
}
