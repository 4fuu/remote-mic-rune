//! Real implementations of the [`crate::bridge_api`] seam over the actual
//! `rc-bridge` and `rc-mapping` crates (the stubs in [`crate::service`] stay
//! for `--demo` mode and host tests).
//!
//! Three things need adapting:
//! - **Events**: `rc-bridge` delivers `rc_bridge::EngineEvent` on the channel
//!   handed to `RcBridge::start`; the app pump consumes the local
//!   [`EngineEvent`](crate::bridge_api::EngineEvent). A forwarder thread
//!   converts between the two (both carry `rc_protocol::RemoteButton`, so it
//!   is a pure variant remap).
//! - **Mappings**: the app's UI model ([`crate::mappings`]) predates seeing
//!   the real crate and differs in a few serde shapes (`Option` slots vs
//!   `Disabled`, `dx/dy` vs `dir/px`, `delta` vs `dir/lines`), so conversion
//!   is hand-written and unit-tested instead of a serde round-trip. The UI
//!   keeps editing its own model; the engine sees `rc_mapping` types.
//! - **Ownership startup**: `RcBridge::start` never fails — pipe creation
//!   happens on a worker and surfaces as `snapshot().already_running`; this
//!   module polls briefly and maps it to
//!   [`StartError::AnotherInstance`](crate::bridge_api::StartError::AnotherInstance).

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_mapping::actions as map_actions;
use rc_mapping::engine::MappingEngine;
use rc_mapping::inject::KeyInjector;

use crate::bridge_api::{
    BridgeCounters, BridgeHandle, BridgePhase, BridgeStatus, EngineEvent, MappingEngineHandle,
    StartError,
};
use crate::mappings;

/// Build the real service pair. `Err` only for another-instance / startup
/// failure; the caller falls back to nothing (the UI shows the error).
pub fn start(engine_tx: Sender<EngineEvent>) -> Result<(Arc<RealBridge>, Arc<RealEngine>), StartError> {
    let (real_tx, real_rx) = std::sync::mpsc::channel::<rc_bridge::EngineEvent>();
    let bridge = Arc::new(RealBridge {
        inner: rc_bridge::RcBridge::start(real_tx),
    });

    // The pipe worker decides asynchronously; give it a moment to surface
    // "another instance owns the pipe" before we declare success.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snap = bridge.inner.snapshot();
        if snap.already_running {
            bridge.inner.stop();
            return Err(StartError::AnotherInstance);
        }
        if !matches!(snap.phase, rc_bridge::BridgePhase::Stopped) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Bridge events -> app events.
    std::thread::spawn(move || {
        while let Ok(event) = real_rx.recv() {
            let converted = match event {
                rc_bridge::EngineEvent::Press(b) => EngineEvent::Press(b),
                rc_bridge::EngineEvent::Release(b) => EngineEvent::Release(b),
                rc_bridge::EngineEvent::ReleaseAll => EngineEvent::ReleaseAll,
            };
            if engine_tx.send(converted).is_err() {
                break; // app is shutting down
            }
        }
    });

    #[cfg(target_os = "windows")]
    let injector: Box<dyn KeyInjector> = Box::new(rc_mapping::SendInputInjector::new());
    #[cfg(not(target_os = "windows"))]
    let injector: Box<dyn KeyInjector> = NoopInjector::boxed();

    Ok((bridge, Arc::new(RealEngine { inner: Mutex::new(MappingEngine::new(injector)) })))
}

// ---------------------------------------------------------------- bridge ---

pub struct RealBridge {
    inner: Arc<rc_bridge::RcBridge>,
}

impl BridgeHandle for RealBridge {
    fn set_capture_targets(&self, enabled: bool, mapped_mask: u64) {
        self.inner.set_capture_targets(enabled, mapped_mask);
    }

    fn set_voice_synth(&self, to: Option<u16>) {
        self.inner.set_voice_synth(to);
    }

    fn snapshot(&self) -> BridgeStatus {
        let snap = self.inner.snapshot();
        BridgeStatus {
            phase: match snap.phase {
                rc_bridge::BridgePhase::Stopped => BridgePhase::Stopped,
                rc_bridge::BridgePhase::Failed => BridgePhase::Stopped,
                rc_bridge::BridgePhase::Listening => BridgePhase::Listening,
                rc_bridge::BridgePhase::Connected => BridgePhase::Connected,
            },
            helper_pid: (snap.helper_pid != 0).then_some(snap.helper_pid),
            generation: snap.generation,
            acked_usages: snap.owned_usages,
            ownership_age_ms: snap.last_ack_age_ms,
            recovering: snap.recovering,
            captured_usages: snap.target_usages,
            counters: BridgeCounters {
                edges: snap.counters.press_edges_total + snap.counters.release_edges_total,
                release_alls: snap.counters.release_all_total,
                retargets: snap.counters.retargets_total,
                pings: snap.counters.accepted_total,
                denies: snap.counters.denied_total,
            },
        }
    }

    fn stop(&self) {
        self.inner.stop();
    }
}

// ---------------------------------------------------------------- engine ---

pub struct RealEngine {
    inner: Mutex<MappingEngine>,
}

impl MappingEngineHandle for RealEngine {
    fn handle(&self, event: EngineEvent) {
        let mapped = match event {
            EngineEvent::Press(b) => rc_mapping::EngineEvent::Press(b),
            EngineEvent::Release(b) => rc_mapping::EngineEvent::Release(b),
            EngineEvent::ReleaseAll => rc_mapping::EngineEvent::ReleaseAll,
        };
        if let Ok(mut engine) = self.inner.lock() {
            engine.handle(mapped);
        }
    }

    fn tick(&self, _now_ms: u64) {
        if let Ok(mut engine) = self.inner.lock() {
            engine.tick(Instant::now());
        }
    }

    fn replace_mappings(&self, mappings: &mappings::ButtonMappings) {
        let converted = convert_mappings(mappings);
        if let Ok(mut engine) = self.inner.lock() {
            engine.reconfigure(&converted);
        }
    }
}

// ------------------------------------------------------------- conversion ---

fn convert_vkey(key: mappings::VKey) -> map_actions::VKey {
    map_actions::VKey(key.0)
}

fn convert_action(action: &mappings::ButtonAction) -> map_actions::ButtonAction {
    use mappings::ButtonAction as App;
    match action {
        App::Disabled => map_actions::ButtonAction::Disabled,
        App::Shortcut { chord } => map_actions::ButtonAction::Shortcut {
            chord: chord.iter().copied().map(convert_vkey).collect(),
        },
        App::MediaKey { key } => map_actions::ButtonAction::MediaKey {
            key: match key {
                mappings::MediaKey::Mute => map_actions::MediaKey::Mute,
                mappings::MediaKey::VolumeDown => map_actions::MediaKey::VolumeDown,
                mappings::MediaKey::VolumeUp => map_actions::MediaKey::VolumeUp,
                mappings::MediaKey::PlayPause => map_actions::MediaKey::PlayPause,
                mappings::MediaKey::Prev => map_actions::MediaKey::Prev,
                mappings::MediaKey::Next => map_actions::MediaKey::Next,
            },
        },
        App::MouseClick { button } => map_actions::ButtonAction::MouseClick {
            button: match button {
                mappings::MouseButton::Left => map_actions::MouseClickButton::Left,
                mappings::MouseButton::Right => map_actions::MouseClickButton::Right,
                mappings::MouseButton::Middle => map_actions::MouseClickButton::Middle,
            },
        },
        // The UI model stores a vector; the engine model stores a direction
        // plus magnitude. Dominant axis wins; diagonal moves are not
        // creatable from the V1 picker.
        App::MouseMove { dx, dy } => {
            let (dir, px) = if dx.abs() >= dy.abs() {
                (*dx >= 0, dx.abs())
            } else {
                (*dy >= 0, dy.abs())
            };
            map_actions::ButtonAction::MouseMove {
                dir: if dx.abs() >= dy.abs() {
                    if dir { map_actions::MoveDirection::Right } else { map_actions::MoveDirection::Left }
                } else if dir {
                    map_actions::MoveDirection::Down
                } else {
                    map_actions::MoveDirection::Up
                },
                px: px as i32,
            }
        }
        App::Scroll { delta } => map_actions::ButtonAction::Scroll {
            dir: if *delta >= 0 { map_actions::ScrollDirection::Up } else { map_actions::ScrollDirection::Down },
            lines: delta.abs(),
        },
    }
}

fn convert_actions(actions: &mappings::ButtonActions) -> map_actions::ButtonActions {
    let conv = |slot: &Option<mappings::ButtonAction>| {
        slot.as_ref().map(convert_action).unwrap_or(map_actions::ButtonAction::Disabled)
    };
    // The app model stores "which action repeats"; the engine model stores
    // "which column repeats". Repeat targets the single column when it is
    // configured, otherwise the long column.
    let repeat_column = actions.hold_repeat.as_ref().map(|_| {
        if actions.single.is_some() {
            map_actions::ButtonTrigger::Single
        } else {
            map_actions::ButtonTrigger::Long
        }
    });
    map_actions::ButtonActions {
        single: conv(&actions.single),
        double: conv(&actions.double),
        long: conv(&actions.long),
        hold_repeat: repeat_column,
    }
}

fn convert_mappings(mappings: &mappings::ButtonMappings) -> map_actions::ButtonMappings {
    let mut buttons = map_actions::ButtonMap::default();
    for (name, actions) in &mappings.buttons {
        // Keyed by the same stable lowercase names; unknown names cannot
        // occur because the app only writes RemoteButton::name() keys.
        if let Some(button) =
            rc_protocol::ALL_BUTTONS.iter().find(|b| b.name() == name).copied()
        {
            buttons.0.insert(button, convert_actions(actions));
        }
    }
    // Capture gating is the app's toggle (set_capture_targets); the engine
    // itself stays enabled so configured gestures fire whenever edges arrive.
    map_actions::ButtonMappings { enabled: true, buttons }
}

// ------------------------------------------------------------- non-windows --

/// Host/no-Windows injector: records nothing, injects nothing. The real app
/// on Windows uses `SendInputInjector`.
struct NoopInjector;

impl NoopInjector {
    /// Host/no-Windows builds only; the Windows build uses `SendInputInjector`.
    #[allow(dead_code)]
    fn boxed() -> Box<dyn KeyInjector> {
        Box::new(NoopInjector)
    }
}

impl KeyInjector for NoopInjector {
    fn send_chord(&self, _chord: &[map_actions::VKey]) -> Result<(), rc_mapping::InjectError> {
        Ok(())
    }
    fn media(&self, _key: map_actions::MediaKey) -> Result<(), rc_mapping::InjectError> {
        Ok(())
    }
    fn mouse_click(&self, _button: map_actions::MouseClickButton) -> Result<(), rc_mapping::InjectError> {
        Ok(())
    }
    fn mouse_move(&self, _dir: map_actions::MoveDirection, _px: i32) -> Result<(), rc_mapping::InjectError> {
        Ok(())
    }
    fn scroll(&self, _dir: map_actions::ScrollDirection, _lines: i32) -> Result<(), rc_mapping::InjectError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_protocol::RemoteButton;

    #[test]
    fn converts_every_action_kind() {
        let mut mappings = mappings::ButtonMappings::default();
        mappings.buttons.insert(
            "back".into(),
            mappings::ButtonActions {
                single: Some(mappings::ButtonAction::MediaKey { key: mappings::MediaKey::VolumeUp }),
                double: Some(mappings::ButtonAction::Shortcut {
                    chord: vec![mappings::vk::LCONTROL, mappings::vk::F5],
                }),
                long: Some(mappings::ButtonAction::MouseClick { button: mappings::MouseButton::Right }),
                hold_repeat: Some(mappings::ButtonAction::Disabled),
            },
        );
        mappings.buttons.insert(
            "up".into(),
            mappings::ButtonActions {
                single: Some(mappings::ButtonAction::MouseMove { dx: 0, dy: -40 }),
                double: Some(mappings::ButtonAction::Scroll { delta: 3 }),
                long: None,
                hold_repeat: None,
            },
        );
        let converted = convert_mappings(&mappings);
        let back = converted.actions(RemoteButton::Back);
        assert!(matches!(
            back.single,
            map_actions::ButtonAction::MediaKey { key: map_actions::MediaKey::VolumeUp }
        ));
        assert!(matches!(
            back.double,
            map_actions::ButtonAction::Shortcut { ref chord } if chord == &vec![
                map_actions::VKey(0xA2),
                map_actions::VKey(0x74)
            ]
        ));
        assert!(matches!(
            back.long,
            map_actions::ButtonAction::MouseClick { button: map_actions::MouseClickButton::Right }
        ));
        // hold_repeat set + single configured -> repeats the single column.
        assert_eq!(back.hold_repeat, Some(map_actions::ButtonTrigger::Single));

        let up = converted.actions(RemoteButton::Up);
        assert!(matches!(
            up.single,
            map_actions::ButtonAction::MouseMove { dir: map_actions::MoveDirection::Up, px: 40 }
        ));
        assert!(matches!(
            up.double,
            map_actions::ButtonAction::Scroll { dir: map_actions::ScrollDirection::Up, lines: 3 }
        ));
        assert!(matches!(up.long, map_actions::ButtonAction::Disabled));
        assert_eq!(up.hold_repeat, None);
        assert!(converted.enabled);
    }

    #[test]
    fn unknown_button_names_are_dropped_not_panicked() {
        let mut mappings = mappings::ButtonMappings::default();
        mappings.buttons.insert("not-a-button".into(), mappings::ButtonActions::default());
        let converted = convert_mappings(&mappings);
        assert!(converted.buttons.0.is_empty());
    }
}
