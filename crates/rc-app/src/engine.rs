//! The engine pump: one thread that owns the bridge's event channel, feeds
//! every event into the mapping engine, forwards snapshots and pressed-state
//! changes to the UI, and drains the channel on a short cadence.
//!
//! The loop's decisions are factored into pure helpers ([`apply_edge`],
//! [`due`]) so they are host-testable without a thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rc_protocol::ALL_BUTTONS;

use crate::bridge_api::{EngineEvent, SharedBridge, SharedEngine, UiNotice};
use crate::mappings::ButtonMappings;

/// How long one pump iteration drains bridge events before checking status.
pub const POLL_MS: u64 = 25;

/// How often the pump samples the bridge snapshot for the status screen.
pub const STATUS_PERIOD_MS: u64 = 500;

/// Loop timings, injectable so tests run fast.
#[derive(Debug, Clone, Copy)]
pub struct PumpTimings {
    pub poll: Duration,
    pub status_period: Duration,
}

impl Default for PumpTimings {
    fn default() -> Self {
        PumpTimings {
            poll: Duration::from_millis(POLL_MS),
            status_period: Duration::from_millis(STATUS_PERIOD_MS),
        }
    }
}

/// What the pump installs once, before the loop starts.
#[derive(Debug, Clone)]
pub struct PumpInit {
    pub mappings: ButtonMappings,
    pub capture_enabled: bool,
    pub voice_synth_to: Option<u16>,
    /// The UI language, for the one user-facing notice the pump can emit
    /// (the fatal overlay when the event channel dies).
    pub lang: crate::i18n::Lang,
}

/// Spawn the pump thread. `on_notice` is called from the pump thread; the UI
/// side forwards notices into app state (via `AppHandle::update`).
pub fn spawn_pump(
    rx: Receiver<EngineEvent>,
    bridge: SharedBridge,
    engine: SharedEngine,
    init: PumpInit,
    timings: PumpTimings,
    on_notice: Box<dyn Fn(UiNotice) + Send>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("rc-app-engine-pump".into())
        .spawn(move || pump_loop(rx, bridge, engine, init, timings, on_notice, stop))
        .expect("spawn the engine pump thread")
}

/// The pump body. Exits (and stops the bridge) when `stop` flips or the
/// event channel closes.
pub fn pump_loop(
    rx: Receiver<EngineEvent>,
    bridge: SharedBridge,
    engine: SharedEngine,
    init: PumpInit,
    timings: PumpTimings,
    on_notice: Box<dyn Fn(UiNotice) + Send>,
    stop: Arc<AtomicBool>,
) {
    // Install the initial state so capture and mappings are correct even if
    // the UI never touches anything.
    engine.replace_mappings(&init.mappings);
    bridge.set_capture_targets(init.capture_enabled, init.mappings.mapped_mask());
    bridge.set_voice_synth(init.voice_synth_to);
    on_notice(UiNotice::Status(bridge.snapshot()));

    let mut pressed: u64 = 0;
    let mut last_status = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Drain the channel into the engine for up to one poll period.
        let deadline = Instant::now() + timings.poll;
        let mut edges = 0usize;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(event) => {
                    if apply_edge(&mut pressed, event) {
                        edges += 1;
                    }
                    engine.handle(event);
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    on_notice(UiNotice::Fatal(
                        crate::i18n::tr(init.lang, "The bridge event channel closed").into(),
                    ));
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        if edges > 0 {
            on_notice(UiNotice::PressedChanged(pressed));
        }
        if due(last_status, timings.status_period) {
            on_notice(UiNotice::Status(bridge.snapshot()));
            last_status = Instant::now();
        }
    }
    bridge.stop();
}

/// Apply one engine event to the pressed mask (bit per button ordinal).
/// Returns true when the mask changed.
pub fn apply_edge(pressed: &mut u64, event: EngineEvent) -> bool {
    match event {
        EngineEvent::Press(button) => {
            let bit = 1u64 << button.ordinal();
            if *pressed & bit == 0 {
                *pressed |= bit;
                true
            } else {
                false
            }
        }
        EngineEvent::Release(button) => {
            let bit = 1u64 << button.ordinal();
            if *pressed & bit != 0 {
                *pressed &= !bit;
                true
            } else {
                false
            }
        }
        EngineEvent::ReleaseAll => {
            let changed = *pressed != 0;
            *pressed = 0;
            changed
        }
    }
}

/// Whether a periodic task is due (pure so tests can pin the clock).
pub fn due(last: Instant, period: Duration) -> bool {
    last.elapsed() >= period
}

/// The pressed mask as button names, for logs.
pub fn pressed_names(mask: u64) -> Vec<&'static str> {
    ALL_BUTTONS
        .iter()
        .filter(|b| mask & (1 << b.ordinal()) != 0)
        .map(|b| b.name())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_api::{BridgeHandle, BridgePhase};
    use crate::mappings::{ButtonAction, MediaKey};
    use crate::service::{NoticeSink, StubBridge, StubEngine};
    use std::sync::mpsc;

    fn init(mappings: ButtonMappings) -> PumpInit {
        PumpInit {
            mappings,
            capture_enabled: true,
            voice_synth_to: None,
            lang: crate::i18n::Lang::En,
        }
    }

    fn fast_timings() -> PumpTimings {
        PumpTimings { poll: Duration::from_millis(5), status_period: Duration::from_millis(30) }
    }

    fn mappings_with_home() -> ButtonMappings {
        let mut mappings = ButtonMappings::default();
        mappings.set_action(
            RemoteButton::Home,
            Some(ButtonAction::MediaKey { key: MediaKey::PlayPause }),
        );
        mappings
    }

    // Alias to keep the test imports tidy.
    use rc_protocol::RemoteButton;

    #[test]
    fn apply_edge_tracks_press_release_and_release_all() {
        let mut pressed = 0u64;
        let home = RemoteButton::Home;
        assert!(apply_edge(&mut pressed, EngineEvent::Press(home)));
        assert!(!apply_edge(&mut pressed, EngineEvent::Press(home)), "duplicate press is no change");
        assert_eq!(pressed, 1 << home.ordinal());
        assert!(apply_edge(&mut pressed, EngineEvent::Release(home)));
        assert!(!apply_edge(&mut pressed, EngineEvent::Release(home)));
        assert!(apply_edge(&mut pressed, EngineEvent::Press(home)));
        assert!(apply_edge(&mut pressed, EngineEvent::ReleaseAll));
        assert_eq!(pressed, 0);
    }

    #[test]
    fn pump_installs_initial_state_and_feeds_events() {
        let (tx, rx) = mpsc::channel();
        let bridge = StubBridge::start(tx.clone(), false).unwrap();
        let engine = StubEngine::new();
        let sink = NoticeSink::new();
        let stop = Arc::new(AtomicBool::new(false));

        let handle = spawn_pump(
            rx,
            bridge.clone(),
            engine.clone(),
            init(mappings_with_home()),
            fast_timings(),
            Box::new({
                let sink = sink.clone();
                move |notice| sink.push(notice)
            }),
            stop.clone(),
        );

        tx.send(EngineEvent::Press(RemoteButton::Home)).unwrap();
        tx.send(EngineEvent::Release(RemoteButton::Home)).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("pump thread must exit cleanly");

        let state = engine.state();
        assert_eq!(state.handled.len(), 2, "both events reached the engine");
        assert_eq!(
            state.mappings.as_ref().map(|m| m.mapped_mask()),
            Some(1 << RemoteButton::Home.ordinal()),
        );
        // Initial capture install happened exactly once, with the right mask.
        assert!(bridge.capture_requests().contains(&(true, 1 << RemoteButton::Home.ordinal())));
        let notices = sink.snapshot();
        assert!(
            notices.iter().any(|n| matches!(n, UiNotice::Status(_))),
            "status snapshots reached the UI sink"
        );
        assert!(
            notices.iter().any(|n| matches!(n, UiNotice::PressedChanged(_))),
            "pressed changes reached the UI sink"
        );
    }

    #[test]
    fn pump_stops_when_the_channel_closes() {
        let (tx, rx) = mpsc::channel();
        let bridge = StubBridge::start(tx, false).unwrap();
        let engine = StubEngine::new();
        let sink = NoticeSink::new();
        let stop = Arc::new(AtomicBool::new(false));
        // stop() drops the bridge's engine sender, closing the channel —
        // the same path a real RcBridge::stop takes on app quit.
        bridge.stop();

        let handle = spawn_pump(
            rx,
            bridge,
            engine,
            init(ButtonMappings::default()),
            fast_timings(),
            Box::new({
                let sink = sink.clone();
                move |notice| sink.push(notice)
            }),
            stop,
        );
        handle.join().expect("channel close must terminate the pump");
        assert!(sink.snapshot().iter().any(|n| matches!(n, UiNotice::Fatal(_))));
    }

    #[test]
    fn pressed_names_map_ords_to_labels() {
        let mask = (1 << RemoteButton::Back.ordinal()) | (1 << RemoteButton::Up.ordinal());
        assert_eq!(pressed_names(mask), vec!["back", "up"]);
        assert!(pressed_names(0).is_empty());
    }

    #[test]
    fn due_reports_elapsed_periods() {
        let start = Instant::now();
        assert!(!due(start, Duration::from_secs(10)));
        assert!(due(start - Duration::from_secs(11), Duration::from_secs(10)));
    }

    #[test]
    fn phase_default_is_stopped() {
        assert_eq!(BridgePhase::Stopped, BridgePhase::Stopped);
    }
}
