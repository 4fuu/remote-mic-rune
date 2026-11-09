//! The integration seam between rc-app and the sibling crates `rc-bridge`
//! (bridge/pipe server) and `rc-mapping` (gesture engine + injector).
//!
//! The sibling crates are written in parallel threads, so rc-app codes against
//! their documented API shapes through the two traits below. Today
//! [`crate::service`] provides stub implementations so the app compiles,
//! runs and is testable on any host. When the real crates land, the swap is
//! confined to `main.rs` and a ~30-line adapter:
//!
//! 1. Add the path dependencies (`rc-bridge`, `rc-mapping`) to rc-app's
//!    Cargo.toml.
//! 2. Replace `service::StubBridge::start` in `main.rs` with
//!    `RcBridge::start(engine_tx)`, wrapped in a `RealBridge(Arc<RcBridge>)`
//!    newtype implementing [`BridgeHandle`] by delegating to
//!    `set_capture_targets`, `set_voice_synth`, `snapshot` and `stop`, and
//!    converting `BridgeSnapshot` into [`BridgeStatus`].
//!    `StartError::AnotherInstance` comes from pipe creation failing
//!    (`%LOCALAPPDATA%\RemoteMicRune\bridge.ini` cannot be created => another
//!    instance owns the bridge pipe).
//! 3. Replace `service::StubEngine` with `MappingEngine`, wrapped in a
//!    `RealEngine(Arc<MappingEngine>)` implementing [`MappingEngineHandle`]
//!    (`handle` = `MappingEngine::handle`, `tick` = the engine's 25 ms tick,
//!    `replace_mappings` = the engine's mappings setter), and re-export the
//!    real `EngineEvent` instead of the local mirror below.
//!
//! Nothing else in rc-app touches rc-bridge/rc-mapping types: ui.rs, engine.rs
//! and main.rs only ever use [`BridgeHandle`], [`MappingEngineHandle`],
//! [`EngineEvent`], [`BridgeStatus`], [`StartError`] and [`UiNotice`].

use std::sync::Arc;

use rc_protocol::RemoteButton;

use crate::mappings::ButtonMappings;

/// Events rc-bridge delivers on the channel handed to `RcBridge::start`.
/// Mirrors `rc_mapping::EngineEvent`; replace this definition with
/// `pub use rc_mapping::EngineEvent` when the crate lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    Press(RemoteButton),
    Release(RemoteButton),
    /// Sent when the recognizer loses focus or the injector resets; the
    /// engine clears its pressed set. Produced by rc-bridge at runtime and
    /// constructed in engine tests on the host.
    #[allow(dead_code)]
    ReleaseAll,
}

/// UI-facing mirror of `rc_bridge::BridgeSnapshot` (phase, helper pid,
/// generation, acked usages, ownership age, recovering flag, counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgePhase {
    Stopped,
    Listening,
    Connected,
}

/// The bridge counters the status screen shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeCounters {
    pub edges: u64,
    pub release_alls: u64,
    pub retargets: u64,
    pub pings: u64,
    pub denies: u64,
}

/// One snapshot of bridge state for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    pub phase: BridgePhase,
    pub helper_pid: Option<u32>,
    /// Current capture-target generation handed to the helper/agent.
    pub generation: u64,
    /// Usages the agent last ACKed owning (empty until the first ack).
    pub acked_usages: Vec<u16>,
    /// Age of that ack; None = never acked.
    pub ownership_age_ms: Option<u64>,
    /// Device-node recovery in progress (helper's `R 1`).
    pub recovering: bool,
    /// Usages currently requested for capture.
    pub captured_usages: Vec<u16>,
    pub counters: BridgeCounters,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        BridgeStatus {
            phase: BridgePhase::Stopped,
            helper_pid: None,
            generation: 0,
            acked_usages: Vec::new(),
            ownership_age_ms: None,
            recovering: false,
            captured_usages: Vec::new(),
            counters: BridgeCounters::default(),
        }
    }
}

/// Why `start` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// The bridge pipe is owned by another app instance.
    #[allow(dead_code)] // produced by rc-bridge at runtime; tests construct it on the host
    AnotherInstance,
    #[allow(dead_code)] // produced by rc-bridge at runtime; tests construct it on the host
    Failed(String),
}

impl StartError {
    /// The user-facing message in the UI's language; the single-instance
    /// case is its own screen.
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::tr_fmt;
        match self {
            StartError::AnotherInstance => tr_fmt!(
                lang,
                "Another instance of Remote Mic Rune is already running. \
                 Only one app can own the bridge pipe at a time."
            ),
            StartError::Failed(why) => tr_fmt!(lang, "The bridge could not start: {}", why),
        }
    }
}

/// What rc-app needs from the running bridge (rc-bridge's `RcBridge`).
pub trait BridgeHandle: Send + Sync + 'static {
    /// Update the dynamic capture scope: master switch plus the mask of
    /// buttons with configured mappings. Recomputed on every mappings change.
    fn set_capture_targets(&self, enabled: bool, mapped_mask: u64);
    /// Voice-key report-layer synthesis target usage, or None for off.
    fn set_voice_synth(&self, to: Option<u16>);
    /// One consistent snapshot of bridge state.
    fn snapshot(&self) -> BridgeStatus;
    /// Ordered teardown of the pipe server (app exit).
    fn stop(&self);
}

/// What rc-app needs from the gesture engine (rc-mapping's `MappingEngine`).
pub trait MappingEngineHandle: Send + Sync + 'static {
    /// Feed one bridge event (button edge or release-all) into the recognizer.
    fn handle(&self, event: EngineEvent);
    /// Advance gesture timers (double-click windows, long-press, repeat).
    /// The pump calls this every [`crate::engine::TICK_MS`].
    fn tick(&self, now_ms: u64);
    /// Swap in a new mapping table (capturing recompute is separate, via the
    /// bridge handle).
    fn replace_mappings(&self, mappings: &ButtonMappings);
}

/// Updates the pump thread forwards to the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiNotice {
    /// A fresh [`BridgeStatus`] snapshot.
    Status(BridgeStatus),
    /// Pressed-state mask by button ordinal, for the per-row indicators.
    PressedChanged(u64),
    /// The pump gave up (channel closed, engine poisoned); app should show it.
    Fatal(String),
}

/// Shared handle types the UI and pump hold.
pub type SharedBridge = Arc<dyn BridgeHandle>;
pub type SharedEngine = Arc<dyn MappingEngineHandle>;
