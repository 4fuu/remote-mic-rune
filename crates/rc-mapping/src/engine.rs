//! The mapping engine: bridge events in, gestures recognized, actions
//! injected.
//!
//! Thread model (chosen design): the engine is a plain struct with **no
//! internal thread**. The owning application drives it:
//! - feed every [`EngineEvent`] from the bridge channel into
//!   [`MappingEngine::handle`] as it arrives, and
//! - call [`MappingEngine::tick`] on a fixed cadence — the current driver
//!   contract is every 25 ms (`rc-protocol::PIPE_IDLE_BACKOFF`-style polling
//!   is not needed here; 25 ms keeps repeat beats within ~2% of their
//!   interval).
//! - For deterministic tests, [`MappingEngine::handle_at`] and `tick` accept
//!   an explicit timestamp instead of `Instant::now()`.

use std::time::Instant;

use rc_protocol::RemoteButton;

use crate::actions::{ButtonAction, ButtonMappings};
use crate::gestures::{
    configs_from_mappings, FiredGesture, GestureConfigs, GestureKind, GestureRecognizer,
};
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

/// Executes a fired gesture against the configured action.
pub struct MappingEngine {
    mappings: ButtonMappings,
    configs: GestureConfigs,
    recognizer: GestureRecognizer,
    injector: Box<dyn KeyInjector>,
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
    /// (disabled) mapping: no gestures are recognized until
    /// [`MappingEngine::reconfigure`] installs a real configuration.
    pub fn new(injector: Box<dyn KeyInjector>) -> Self {
        let mappings = ButtonMappings::default();
        let configs = configs_from_mappings(&mappings);
        let recognizer = GestureRecognizer::new(&configs);
        Self {
            mappings,
            configs,
            recognizer,
            injector,
            last_error: None,
            injection_errors: 0,
            injected_actions: 0,
        }
    }

    /// Install a new mapping configuration. All in-flight gesture state is
    /// reset (pending windows and repeats are dropped, nothing fires).
    pub fn reconfigure(&mut self, mappings: &ButtonMappings) {
        self.mappings = mappings.clone();
        self.configs = configs_from_mappings(mappings);
        self.recognizer.reconfigure(&self.configs);
    }

    pub fn mappings(&self) -> &ButtonMappings {
        &self.mappings
    }

    /// Release every held state without firing gestures (bridge teardown /
    /// watchdog events).
    pub fn release_all(&mut self) {
        self.recognizer.release_all();
    }

    /// Feed one edge event, timestamped now.
    pub fn handle(&mut self, event: EngineEvent) {
        self.handle_at(event, Instant::now());
    }

    /// Feed one edge event with an explicit timestamp (test seam).
    pub fn handle_at(&mut self, event: EngineEvent, now: Instant) {
        let fired = match event {
            EngineEvent::Press(button) => self.recognizer.press(button, now),
            EngineEvent::Release(button) => self.recognizer.release(button, now),
            EngineEvent::ReleaseAll => {
                self.recognizer.release_all();
                Vec::new()
            }
        };
        for gesture in fired {
            self.execute(gesture);
        }
    }

    /// Advance all recognizer deadlines up to `now` and execute what fired.
    pub fn tick(&mut self, now: Instant) {
        for gesture in self.recognizer.advance(now) {
            self.execute(gesture);
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

    fn execute(&mut self, gesture: FiredGesture) {
        let actions = self.mappings.actions(gesture.button);
        let action = match gesture.kind {
            GestureKind::Single => &actions.single,
            GestureKind::Double => &actions.double,
            GestureKind::Long => &actions.long,
        };
        let result = match action {
            ButtonAction::Disabled => Ok(()),
            ButtonAction::Shortcut { chord } => self.injector.send_chord(chord),
            ButtonAction::MediaKey { key } => self.injector.media(*key),
            ButtonAction::MouseClick { button } => self.injector.mouse_click(*button),
            ButtonAction::MouseMove { dir, px } => self.injector.mouse_move(*dir, *px),
            ButtonAction::Scroll { dir, lines } => self.injector.scroll(*dir, *lines),
        };
        if action.is_configured() {
            self.injected_actions += 1;
        }
        if let Err(error) = result {
            self.injection_errors += 1;
            self.last_error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{
        ButtonActions, ButtonMap, ButtonTrigger, MediaKey, MouseClickButton, MoveDirection,
        ScrollDirection, VKey,
    };
    use crate::gestures::{DOUBLE_CLICK_WINDOW, LONG_PRESS_THRESHOLD, REPEAT_START_DELAY};
    use crate::inject::RecordingInjector;
    use std::time::Duration;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    fn engine_with(mappings: &ButtonMappings) -> MappingEngine {
        let mut engine = MappingEngine::new(Box::new(RecordingInjector::new()));
        engine.reconfigure(mappings);
        engine
    }

    fn chord(key: VKey) -> ButtonAction {
        ButtonAction::Shortcut { chord: vec![key] }
    }

    fn single_mapping(button: RemoteButton, action: ButtonAction) -> ButtonMappings {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            button,
            ButtonActions {
                single: action,
                ..ButtonActions::default()
            },
        );
        mappings
    }

    #[test]
    fn immediate_single_injects_chord_on_press() {
        let mut engine = engine_with(&single_mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(0));
        engine.handle_at(EngineEvent::Release(RemoteButton::Ok), t(10));
        assert_eq!(engine.injected_actions(), 1);
        assert_eq!(engine.injection_errors(), 0);
    }

    #[test]
    fn double_and_long_route_to_their_columns() {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            RemoteButton::Ok,
            ButtonActions {
                single: chord(VKey::RETURN),
                double: chord(VKey::SPACE),
                long: ButtonAction::MediaKey {
                    key: MediaKey::PlayPause,
                },
                hold_repeat: None,
            },
        );
        let mut engine = engine_with(&mappings);

        // Double: press, release, press within window, release -> Space.
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(0));
        engine.handle_at(EngineEvent::Release(RemoteButton::Ok), t(50));
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(150));
        engine.handle_at(EngineEvent::Release(RemoteButton::Ok), t(200));
        // Long: hold past threshold -> PlayPause.
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(1000));
        engine.tick(t(1000) + LONG_PRESS_THRESHOLD);
        assert_eq!(engine.injected_actions(), 2);
    }

    #[test]
    fn tick_drives_repeat_stream() {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            RemoteButton::Up,
            ButtonActions {
                single: chord(VKey::UP),
                double: ButtonAction::Disabled,
                long: ButtonAction::Disabled,
                hold_repeat: Some(ButtonTrigger::Single),
            },
        );
        let mut engine = engine_with(&mappings);
        let start = t(0);
        engine.handle_at(EngineEvent::Press(RemoteButton::Up), start);
        // Coarse 25 ms ticks over one second of holding.
        for ms in 1..=1000usize {
            engine.tick(start + Duration::from_millis(ms as u64 * 25));
        }
        engine.handle_at(
            EngineEvent::Release(RemoteButton::Up),
            start + Duration::from_millis(25_000),
        );
        // Press shot fires immediately; repeat beats start at the start
        // delay and then land on every interval, on 25 ms tick boundaries.
        let start_delay_ms = REPEAT_START_DELAY.as_millis() as u64;
        let interval_ms = RemoteButton::Up.repeat_interval().as_millis() as u64;
        let last_tick_ms = 25_000;
        let expected_beats = (last_tick_ms - start_delay_ms) / interval_ms + 1;
        assert_eq!(
            engine.injected_actions(),
            1 + expected_beats,
            "repeat stream must fire the immediate shot plus the delayed beats"
        );
    }

    #[test]
    fn release_all_drops_pending_gestures_without_injecting() {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            RemoteButton::Ok,
            ButtonActions {
                single: chord(VKey::RETURN),
                double: chord(VKey::SPACE),
                long: ButtonAction::Disabled,
                hold_repeat: None,
            },
        );
        let mut engine = engine_with(&mappings);
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(0));
        engine.handle_at(EngineEvent::Release(RemoteButton::Ok), t(40));
        engine.handle_at(EngineEvent::ReleaseAll, t(50));
        // The pending double window must not fire Space.
        engine.tick(t(0) + Duration::from_millis(40) + DOUBLE_CLICK_WINDOW);
        assert_eq!(engine.injected_actions(), 0);
    }

    #[test]
    fn reconfigure_swaps_behavior_mid_flight() {
        let mut engine =
            engine_with(&single_mapping(RemoteButton::Ok, chord(VKey::RETURN)));
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(0));
        assert_eq!(engine.injected_actions(), 1);

        // Reconfigure to a mouse click: next press clicks instead.
        engine.reconfigure(&single_mapping(
            RemoteButton::Ok,
            ButtonAction::MouseClick {
                button: MouseClickButton::Left,
            },
        ));
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(1000));
        assert_eq!(engine.injected_actions(), 2);

        // Reconfigure to disabled: nothing further.
        engine.reconfigure(&ButtonMappings::default());
        engine.handle_at(EngineEvent::Press(RemoteButton::Ok), t(2000));
        engine.handle_at(EngineEvent::Release(RemoteButton::Ok), t(2100));
        assert_eq!(engine.injected_actions(), 2);
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
            fn mouse_move(&self, _dir: MoveDirection, _px: i32) -> Result<(), InjectError> {
                Ok(())
            }
            fn scroll(&self, _dir: ScrollDirection, _lines: i32) -> Result<(), InjectError> {
                Ok(())
            }
        }
        let mut engine = MappingEngine::new(Box::new(FailingInjector));
        engine.reconfigure(&single_mapping(RemoteButton::Back, chord(VKey::ESCAPE)));
        engine.handle_at(EngineEvent::Press(RemoteButton::Back), t(0));
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
