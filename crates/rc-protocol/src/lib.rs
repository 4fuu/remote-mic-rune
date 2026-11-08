//! rc-protocol — the single source of truth shared by every crate in
//! remote-mic-rune: button usage table, wire formats for both links, and all
//! timing/safety constants.
//!
//! Rules for this crate:
//! - No Windows APIs, no unsafe: it must build and test on any host.
//! - Both endpoints of every link use these types/functions, never hand-rolled
//!   copies of the wire format.
//!
//! Links:
//! - agent  <-> helper : JSON lines over a loopback named pipe
//!   (`\\.\pipe\RemoteMicRune.Agent.<helper-pid>`), see [`agent_wire`].
//! - helper <-> app    : ASCII line protocol over the fixed bridge pipe
//!   (`\\.\pipe\RemoteMicRune.Bridge.v1`), see [`bridge_wire`].

pub mod agent_wire;
pub mod bridge_wire;
pub mod buttons;
pub mod consts;
pub mod handoff;

pub use buttons::{RemoteButton, ALL_BUTTONS, ENHANCED_CAPTURE_BUTTON_USAGES};
