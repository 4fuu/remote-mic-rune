//! Clock helpers shared by the pure state machine and the Windows glue.
//!
//! Two time bases are used:
//! - monotonic milliseconds since process start for every internal timing
//!   decision (lease, watchdogs, backoff) — immune to wall-clock changes;
//! - UNIX milliseconds for wire message `t` fields.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static MONO_ANCHOR: OnceLock<Instant> = OnceLock::new();

/// Monotonic milliseconds since the first call (process-lifetime anchor).
pub fn mono_ms() -> u64 {
    let anchor = MONO_ANCHOR.get_or_init(Instant::now);
    anchor.elapsed().as_millis() as u64
}

/// UNIX epoch milliseconds for wire timestamps; 0 if the clock is pre-epoch.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
