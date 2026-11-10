//! Stub implementations of the rc-bridge / rc-mapping seam so rc-app
//! compiles, runs and is testable before the sibling crates land.
//!
//! `StubBridge` mirrors `RcBridge::start(engine_tx) -> Arc<RcBridge>`'s shape:
//! it hands back an [`BridgeHandle`], owns the receiver side of the event
//! channel, and (with `--demo`) simulates a helper connecting so the UI can
//! be exercised on a development host. The real crate replaces both the
//! constructor call in `main.rs` and this file (see `bridge_api.rs`).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rc_protocol::{RemoteButton, ALL_BUTTONS};

use crate::bridge_api::{
    BridgeCounters, BridgeHandle, BridgePhase, BridgeStatus, EngineEvent, MappingEngineHandle,
    StartError,
};

#[cfg(test)]
use crate::bridge_api::UiNotice;
use crate::mappings::ButtonMappings;

/// In-memory bridge state the UI snapshot call reads.
#[derive(Debug)]
struct StubBridgeState {
    status: BridgeStatus,
    capture_requests: Vec<(bool, u64)>,
    synth_requests: Vec<Option<u16>>,
    stopped: bool,
    /// Kept alive so the engine channel stays open for the bridge's
    /// lifetime, exactly like the real `RcBridge` holds its sender.
    engine_tx: Option<std::sync::mpsc::Sender<EngineEvent>>,
}

/// Stand-in for `RcBridge` until the real crate lands.
pub struct StubBridge {
    state: Mutex<StubBridgeState>,
}

impl StubBridge {
    /// Mirrors `RcBridge::start`: creates the "pipe" (always succeeds here)
    /// and returns the handle. With `demo`, a simulated helper connects and
    /// starts pressing buttons so host-side UI runs have something to show.
    pub fn start(engine_tx: std::sync::mpsc::Sender<EngineEvent>, demo: bool) -> Result<Arc<StubBridge>, StartError> {
        let bridge = Arc::new(StubBridge {
            state: Mutex::new(StubBridgeState {
                status: BridgeStatus {
                    phase: BridgePhase::Listening,
                    ..Default::default()
                },
                capture_requests: Vec::new(),
                synth_requests: Vec::new(),
                stopped: false,
                engine_tx: Some(engine_tx.clone()),
            }),
        });
        if demo {
            spawn_demo_events(engine_tx, bridge.clone());
        }
        Ok(bridge)
    }

    /// The single-instance failure path, for tests of the startup decision.
    #[cfg(test)]
    pub fn start_conflicted() -> Result<Arc<StubBridge>, StartError> {
        Err(StartError::AnotherInstance)
    }

    /// Every `set_capture_targets` request, oldest first.
    #[cfg(test)]
    pub fn capture_requests(&self) -> Vec<(bool, u64)> {
        self.state.lock().unwrap().capture_requests.clone()
    }

    #[cfg(test)]
    pub fn synth_requests(&self) -> Vec<Option<u16>> {
        self.state.lock().unwrap().synth_requests.clone()
    }
}

impl BridgeHandle for StubBridge {
    fn set_capture_targets(&self, enabled: bool, mapped_mask: u64) {
        let mut state = self.state.lock().unwrap();
        state.capture_requests.push((enabled, mapped_mask));
        state.status.generation += 1;
        state.status.captured_usages = rc_protocol::buttons::capture_set(mapped_mask)
            .into_iter()
            .filter(|_| enabled)
            .collect();
    }

    fn set_voice_synth(&self, to: Option<u16>) {
        self.state.lock().unwrap().synth_requests.push(to);
    }

    fn snapshot(&self) -> BridgeStatus {
        self.state.lock().unwrap().status.clone()
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        // Dropping the sender closes the engine channel, ending the pump.
        state.engine_tx = None;
    }
}

/// Simulated helper: connects, presses `Back` in a loop, bumps counters.
/// Debug tool only; never compiled into what a user runs.
fn spawn_demo_events(engine_tx: std::sync::mpsc::Sender<EngineEvent>, bridge: Arc<StubBridge>) {
    thread::spawn(move || {
        let mut step: u64 = 0;
        loop {
            thread::sleep(Duration::from_millis(1_500));
            let Ok(mut state) = bridge.state.lock() else { return };
            if state.stopped {
                return;
            }
            step += 1;
            state.status = BridgeStatus {
                phase: if step % 6 == 0 { BridgePhase::Listening } else { BridgePhase::Connected },
                helper_pid: Some(4242),
                generation: state.status.generation.max(1),
                acked_usages: rc_protocol::buttons::capture_set(1 << RemoteButton::Back.ordinal()),
                ownership_age_ms: Some(step * 400 % 4_000),
                recovering: step % 17 == 0,
                captured_usages: state.status.captured_usages.clone(),
                counters: BridgeCounters { edges: step, pings: step * 2, ..Default::default() },
            };
            drop(state);

            // Press and release Back so the row indicator moves.
            let back = ALL_BUTTONS[0];
            let _ = engine_tx.send(EngineEvent::Press(back));
            let _ = engine_tx.send(EngineEvent::Release(back));
        }
    });
}

/// Stub engine state tests can inspect.
#[derive(Debug, Clone, Default)]
pub struct StubEngineState {
    pub handled: Vec<EngineEvent>,
    pub mappings: Option<ButtonMappings>,
}

/// Stand-in for rc-mapping's `MappingEngine`: records what the pump feeds it.
pub struct StubEngine {
    state: Mutex<StubEngineState>,
}

impl Default for StubEngine {
    fn default() -> Self {
        StubEngine { state: Mutex::new(StubEngineState::default()) }
    }
}

impl StubEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Clone of the stub engine state, for tests.
    #[cfg(test)]
    pub fn state(&self) -> StubEngineState {
        self.state.lock().unwrap().clone()
    }
}

impl MappingEngineHandle for StubEngine {
    fn handle(&self, event: EngineEvent) {
        self.state.lock().unwrap().handled.push(event);
    }

    fn replace_mappings(&self, mappings: &ButtonMappings) {
        self.state.lock().unwrap().mappings = Some(mappings.clone());
    }
}

/// Test helper: a UI notice collector. Test-only: production notices go to
/// the real UI through `UiNotice` senders.
#[cfg(test)]
#[derive(Default)]
pub struct NoticeSink {
    pub notices: Mutex<Vec<UiNotice>>,
}

#[cfg(test)]
impl NoticeSink {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn push(&self, notice: UiNotice) {
        self.notices.lock().unwrap().push(notice);
    }

    pub fn snapshot(&self) -> Vec<UiNotice> {
        self.notices.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_bridge_records_capture_and_synth_requests() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = StubBridge::start(tx, false).unwrap();
        bridge.set_capture_targets(true, 1 << RemoteButton::Home.ordinal());
        bridge.set_capture_targets(false, 0);
        bridge.set_voice_synth(Some(0x00E2));
        bridge.set_voice_synth(None);

        assert_eq!(
            bridge.capture_requests(),
            vec![(true, 1 << RemoteButton::Home.ordinal()), (false, 0)]
        );
        assert_eq!(bridge.synth_requests(), vec![Some(0x00E2), None]);
        assert_eq!(bridge.snapshot().phase, BridgePhase::Listening);
    }

    #[test]
    fn capture_requests_advance_the_generation() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = StubBridge::start(tx, false).unwrap();
        assert_eq!(bridge.snapshot().generation, 0);
        bridge.set_capture_targets(true, 0);
        assert_eq!(bridge.snapshot().generation, 1);
    }

    #[test]
    fn disabled_capture_leaves_an_empty_captured_set() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = StubBridge::start(tx, false).unwrap();
        bridge.set_capture_targets(false, u64::MAX);
        assert!(bridge.snapshot().captured_usages.is_empty());
    }

    #[test]
    fn conflicted_start_reports_another_instance() {
        assert!(
            matches!(StubBridge::start_conflicted(), Err(StartError::AnotherInstance)),
            "the conflicted stub reports the single-instance error"
        );
    }

    #[test]
    fn stub_engine_records_everything_the_pump_feeds_it() {
        let engine = StubEngine::new();
        let mut mappings = ButtonMappings::default();
        mappings.set_action(
            RemoteButton::Back,
            Some(crate::mappings::ButtonAction::Disabled),
        );
        engine.replace_mappings(&mappings);
        engine.handle(EngineEvent::Press(RemoteButton::Back));
        engine.handle(EngineEvent::Release(RemoteButton::Back));
        engine.handle(EngineEvent::ReleaseAll);
        let state = engine.state();
        assert_eq!(state.handled.len(), 3);
        assert!(state.mappings.is_some());
    }
}
