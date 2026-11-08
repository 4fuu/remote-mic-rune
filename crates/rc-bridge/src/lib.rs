//! rc-bridge — the app side of the remote-mic-rune helper link.
//!
//! The elevated helper (separate process) connects to this app's named pipe
//! (`rc_protocol::consts::BRIDGE_PIPE_NAME`) and speaks the ASCII line
//! protocol from `rc_protocol::bridge_wire`: `HELLO`/`OK` auth, absolute
//! pressed sets (`E` lines), heartbeats (`P`), ownership acks (`O`), synth
//! acks (`A`), recovery state (`R`) and goodbye (`BYE`).
//!
//! Safety model (ported from the reference product's semantics):
//! - **Absolute-state edges**: `E` lines carry the full pressed set; this
//!   crate derives press/release transitions itself, so a lost line
//!   self-heals on the next one.
//! - **Per-generation ownership**: the app sends `T <generation> <usages>`
//!   (resend until acked at 0.5/1.5/3 s then every 5 s). Only an `O` ack of
//!   the current generation inside `OWNERSHIP_TIMEOUT_MS` (1.5 s — before
//!   the agent's 2 s lease, so the native keyboard path recovers first)
//!   counts as "keys owned". Losing ownership releases everything in the
//!   engine but keeps the link.
//! - **Silence watchdog**: no line from the helper for 3 s means the link
//!   is dead: release everything, keep listening for a new connection.
//! - **Takeover**: a new helper connection always wins; the old one gets a
//!   `BYE` and is disconnected without releasing (the winner re-states the
//!   absolute pressed set).
//! - **Single instance**: creating the first pipe instance with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` fails when the name is taken; the
//!   snapshot then reports `already_running` so the app can surface it.
//!
//! The transport, descriptor file and token generation are Windows-only
//! (`pipe_windows.rs`); all protocol/state logic lives in the pure
//! [`state`] module and is tested on any host.

pub mod bridge;
pub mod state;

#[cfg(target_os = "windows")]
pub mod pipe_windows;

use rc_protocol::RemoteButton;

/// Events the bridge delivers to the mapping engine.
///
/// `E` lines carry absolute pressed sets; the bridge derives these edges
/// itself. `ReleaseAll` is used when the whole pressed state must be
/// dropped (ownership loss, teardown paths). This mirrors
/// `rc_mapping::EngineEvent` (same three variants); the app maps between
/// the two when it pumps the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    Press(RemoteButton),
    Release(RemoteButton),
    ReleaseAll,
}

pub use bridge::{BridgeSnapshot, RcBridge};
pub use state::{BridgeCounters, BridgePhase, CaptureTargets};
