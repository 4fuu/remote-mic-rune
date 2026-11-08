//! Pure protocol/state logic of the bridge link: capture-set computation,
//! edge derivation from absolute pressed sets, auth decisions, ownership
//! validity tracking, and resend scheduling. No Windows APIs, no locks, no
//! IO — everything is host-testable with injected time.

use std::collections::BTreeSet;
use std::time::Duration;

use rc_protocol::consts;
use rc_protocol::RemoteButton;

use crate::EngineEvent;

/// Helper must authenticate within this window after connecting or the
/// connection is dropped.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// No line from the helper for this long means the link is dead: release
/// every pressed button, keep listening.
pub const SILENCE_TIMEOUT: Duration =
    Duration::from_millis(consts::BRIDGE_SILENCE_TIMEOUT_MS as u64);
/// Ownership is only trusted while the current generation was acked within
/// this window. Deliberately expires BEFORE the agent's 2 s lease so keys
/// fall back to the native path first.
pub const OWNERSHIP_TIMEOUT: Duration =
    Duration::from_millis(consts::OWNERSHIP_TIMEOUT_MS as u64);
/// Sleep between polls of a nowait-mode pipe with no data (a missing backoff
/// busy-looped a full core on real hardware).
pub const IDLE_BACKOFF: Duration = Duration::from_millis(consts::PIPE_IDLE_BACKOFF_MS);

/// The capture set the app requests while enhanced capture is enabled:
/// always-captured buttons (Back/VolumeUp/VolumeDown, which have no native
/// path) plus every currently mapped button. Disabled = empty set (full
/// fail-open: the helper stops clearing anything).
pub fn targets_for(enabled: bool, mapped_mask: u64) -> BTreeSet<u16> {
    if enabled {
        rc_protocol::buttons::capture_set(mapped_mask).into_iter().collect()
    } else {
        BTreeSet::new()
    }
}

/// Next target generation; 0 is reserved ("never sent"), so wrap to 1.
pub fn next_generation(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

/// Bridge lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BridgePhase {
    /// Not running (never started, or startup failed).
    #[default]
    Stopped,
    /// Pipe server alive, no authenticated helper yet.
    Listening,
    /// Helper connected and authenticated.
    Connected,
    /// Startup failure other than "already running" (e.g. pipe creation
    /// failed for an unexpected reason).
    Failed,
}

impl BridgePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            BridgePhase::Stopped => "stopped",
            BridgePhase::Listening => "listening",
            BridgePhase::Connected => "connected",
            BridgePhase::Failed => "failed",
        }
    }
}

/// Lifetime counters, exposed for the UI snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BridgeCounters {
    /// Helper connections accepted (including takeovers).
    pub accepted_total: u64,
    /// Auth rejections (per attempt, not per connection).
    pub denied_total: u64,
    /// Connections taken over by a newer helper.
    pub replaced_total: u64,
    /// Press edges delivered to the engine channel.
    pub press_edges_total: u64,
    /// Release edges delivered to the engine channel.
    pub release_edges_total: u64,
    /// `ReleaseAll` events delivered to the engine channel.
    pub release_all_total: u64,
    /// Usages dropped because they were outside the current capture set or
    /// not a known button (expected to stay 0).
    pub filtered_usages_total: u64,
    /// Lines that failed to parse (expected to stay 0).
    pub malformed_total: u64,
    /// Generation bumps from capture-target changes.
    pub retargets_total: u64,
    /// Full releases caused by teardown (silence, error, BYE, stop).
    pub teardown_releases_total: u64,
    /// Times ownership transitioned valid -> lost (each releases the engine).
    pub ownership_losses_total: u64,
    /// `O` lines that did not match the current generation/set.
    pub ownership_rejected_total: u64,
    /// Voice-frame (`F`) results received.
    pub voice_frames_total: u64,
}

/// Current capture targets: generation plus the absolute usage set. The
/// generation increments on every change; only an `O` ack for the current
/// generation with the exact set counts as ownership.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureTargets {
    pub generation: u64,
    pub usages: BTreeSet<u16>,
}

impl CaptureTargets {
    /// Install a new target set, assigning the next generation. Returns the
    /// new generation. The caller compares sets first and skips the call
    /// when nothing changed.
    pub fn install(&mut self, usages: BTreeSet<u16>) -> u64 {
        self.generation = next_generation(self.generation);
        self.usages = usages;
        self.generation
    }
}

/// Auth decision for a `HELLO` line: version must match the protocol
/// version and the token must match the descriptor token (belt and
/// suspenders on the pipe transport — the descriptor file itself is
/// user-readable, so the token guards against confused-process mixups, not
/// against the same user).
pub fn auth_decide(version: u32, token: &str, expected_token: &str) -> Result<(), &'static str> {
    if version != consts::BRIDGE_PROTOCOL_VERSION {
        return Err("version_mismatch");
    }
    if token != expected_token {
        return Err("token_mismatch");
    }
    Ok(())
}

/// Result of applying one absolute pressed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeDiff {
    /// The accepted pressed set (becomes the new "currently pressed").
    pub accepted: BTreeSet<u16>,
    /// Events in delivery order: releases first, then presses.
    pub events: Vec<EngineEvent>,
    /// Usages rejected because they are not a known button usage or outside
    /// the current capture set.
    pub rejected: u64,
}

/// Derive per-button edges from two absolute pressed sets. Usages are
/// filtered against the current capture set (`allowed`) and the semantic
/// button table; unknown or out-of-set usages are counted, never delivered.
/// This self-heals lost lines: each `E` line fully re-states the truth.
pub fn derive_edges(
    before: &BTreeSet<u16>,
    wanted: &[u16],
    allowed: &BTreeSet<u16>,
) -> EdgeDiff {
    let mut accepted = BTreeSet::new();
    let mut rejected = 0u64;
    for usage in wanted {
        if RemoteButton::from_usage(*usage).is_some() && allowed.contains(usage) {
            accepted.insert(*usage);
        } else {
            rejected += 1;
        }
    }
    let mut events = Vec::new();
    for usage in before.difference(&accepted) {
        if let Some(button) = RemoteButton::from_usage(*usage) {
            events.push(EngineEvent::Release(button));
        }
    }
    for usage in accepted.difference(before) {
        if let Some(button) = RemoteButton::from_usage(*usage) {
            events.push(EngineEvent::Press(button));
        }
    }
    EdgeDiff {
        accepted,
        events,
        rejected,
    }
}

/// Tracks whether the helper's agent currently owns the keys for the
/// current capture generation. Validity requires BOTH an ack of the current
/// generation and freshness within [`OWNERSHIP_TIMEOUT`]; validity
/// transitions are reported exactly once so each loss releases the engine
/// exactly once.
#[derive(Debug, Clone, Default)]
pub struct OwnershipTracker {
    acked_generation: Option<u64>,
    acked_usages: BTreeSet<u16>,
    last_ack: Option<std::time::Instant>,
    valid: bool,
}

impl OwnershipTracker {
    /// Record an `O` line. Only an ack of the current generation with the
    /// exact current usage set counts; anything else is rejected (counter
    /// in the caller).
    pub fn record(
        &mut self,
        generation: u64,
        usages: &BTreeSet<u16>,
        targets: &CaptureTargets,
        now: std::time::Instant,
    ) -> bool {
        if generation == targets.generation && usages == &targets.usages {
            self.acked_generation = Some(generation);
            self.acked_usages = usages.clone();
            self.last_ack = Some(now);
            true
        } else {
            false
        }
    }

    /// Re-evaluate validity for the given current generation at `now`.
    /// An ack is fresh while its age is strictly below [`OWNERSHIP_TIMEOUT`]
    /// (at exactly the timeout ownership is lost — before the agent's 2 s
    /// lease). Returns `Some(new_validity)` exactly when the validity
    /// flipped.
    pub fn sync_validity(
        &mut self,
        current_generation: u64,
        now: std::time::Instant,
    ) -> Option<bool> {
        let fresh = self
            .last_ack
            .map(|at| now.duration_since(at) < OWNERSHIP_TIMEOUT)
            .unwrap_or(false);
        let valid_now = fresh && self.acked_generation == Some(current_generation);
        if valid_now != self.valid {
            self.valid = valid_now;
            Some(valid_now)
        } else {
            None
        }
    }

    /// Forget any ack (generation bump, takeover, teardown). The next
    /// `sync_validity` reports the loss transition if ownership was valid.
    pub fn clear(&mut self) {
        self.acked_generation = None;
        self.acked_usages.clear();
        self.last_ack = None;
    }

    pub fn valid(&self) -> bool {
        self.valid
    }

    pub fn acked_generation(&self) -> Option<u64> {
        self.acked_generation
    }

    pub fn acked_usages(&self) -> &BTreeSet<u16> {
        &self.acked_usages
    }

    pub fn last_ack(&self) -> Option<std::time::Instant> {
        self.last_ack
    }
}

/// Whether the link went silent: no line from the helper for at least
/// [`SILENCE_TIMEOUT`] (the window is inclusive — at exactly 3 s the link
/// is declared dead).
pub fn silence_expired(last_rx: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last_rx
        .map(|at| now.duration_since(at) >= SILENCE_TIMEOUT)
        .unwrap_or(false)
}

/// Whether an unauthenticated connection overstayed its welcome
/// (inclusive window, same boundary rule as the other timeouts).
pub fn hello_expired(started: std::time::Instant, now: std::time::Instant) -> bool {
    now.duration_since(started) >= HELLO_TIMEOUT
}

/// Resend schedule for lines awaiting an ack (`T` until `O`, `S` until
/// `A`): 0.5 s / 1.5 s / 3 s, then every 5 s. `None` elapsed (last write
/// failed or never sent) means retry immediately. The lines are absolute
/// state and idempotent, so resending is always safe.
pub fn resend_due(elapsed: Option<Duration>, attempts: u64) -> bool {
    let Some(elapsed) = elapsed else {
        return true;
    };
    let wait = match attempts {
        0 => Duration::from_millis(500),
        1 => Duration::from_millis(1_500),
        2 => Duration::from_millis(3_000),
        _ => Duration::from_millis(5_000),
    };
    elapsed >= wait
}

/// Shared bridge state guarded by one mutex (poison-free locking in the
/// caller). Windows threads mutate this; the UI reads it via `snapshot`.
#[derive(Debug, Default)]
pub struct BridgeShared {
    pub phase: BridgePhase,
    /// Pipe creation failed because another instance owns the name: the
    /// app shows "already running" and stays disabled.
    pub already_running: bool,
    pub helper_pid: u32,
    pub transport: &'static str,
    pub counters: BridgeCounters,
    /// Usages currently believed pressed (absolute, from `E` lines).
    pub pressed: BTreeSet<u16>,
    pub last_rx: Option<std::time::Instant>,
    /// Helper announced device-node recovery (`R` line): suppress
    /// capture-failure restarts until it reports done.
    pub recovering: bool,
    /// The agent confirmed the current synth config with an `A` ack.
    pub synth_confirmed: bool,
    pub ownership: OwnershipTracker,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_protocol::buttons::ALWAYS_CAPTURED_BUTTONS;

    fn usage_of(button: RemoteButton) -> u16 {
        button.usage()
    }

    #[test]
    fn targets_for_matches_capture_set_and_disable_is_empty() {
        let home_bit = 1u64 << RemoteButton::Home.ordinal();
        let ok_bit = 1u64 << RemoteButton::Ok.ordinal();
        let targets = targets_for(true, home_bit | ok_bit);
        let expected: BTreeSet<u16> = rc_protocol::buttons::capture_set(home_bit | ok_bit)
            .into_iter()
            .collect();
        assert_eq!(targets, expected);
        for button in ALWAYS_CAPTURED_BUTTONS {
            assert!(targets.contains(&usage_of(button)));
        }
        assert!(targets.contains(&usage_of(RemoteButton::Home)));
        assert!(targets.contains(&usage_of(RemoteButton::Ok)));
        // Unmapped buttons stay native.
        assert!(!targets.contains(&usage_of(RemoteButton::Menu)));
        // Disabled: nothing captured at all (full fail-open).
        assert!(targets_for(false, home_bit | ok_bit).is_empty());
    }

    #[test]
    fn generation_wraps_to_one_not_zero() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(41), 42);
        assert_eq!(next_generation(u64::MAX), 1);
    }

    #[test]
    fn auth_decide_checks_version_then_token() {
        assert_eq!(auth_decide(consts::BRIDGE_PROTOCOL_VERSION, "tok", "tok"), Ok(()));
        assert_eq!(
            auth_decide(consts::BRIDGE_PROTOCOL_VERSION + 1, "tok", "tok"),
            Err("version_mismatch")
        );
        assert_eq!(
            auth_decide(consts::BRIDGE_PROTOCOL_VERSION, "wrong", "tok"),
            Err("token_mismatch")
        );
    }

    #[test]
    fn derive_edges_reports_releases_first_presses_second() {
        let back = usage_of(RemoteButton::Back);
        let ok = usage_of(RemoteButton::Ok);
        let home = usage_of(RemoteButton::Home);
        let allowed: BTreeSet<u16> = [back, ok, home].into_iter().collect();

        // Empty -> all pressed: presses only, in usage-ascending order
        // (the set is a BTreeSet keyed by usage).
        let diff = derive_edges(&BTreeSet::new(), &[back, ok], &allowed);
        assert_eq!(diff.accepted, [back, ok].into_iter().collect());
        assert_eq!(
            diff.events,
            vec![EngineEvent::Press(RemoteButton::Ok), EngineEvent::Press(RemoteButton::Back)]
        );
        assert_eq!(diff.rejected, 0);

        // Back released, Home pressed: release before press.
        let diff = derive_edges(&diff.accepted, &[ok, home], &allowed);
        assert_eq!(diff.accepted, [ok, home].into_iter().collect());
        assert_eq!(
            diff.events,
            vec![
                EngineEvent::Release(RemoteButton::Back),
                EngineEvent::Press(RemoteButton::Home)
            ]
        );

        // Empty set = all released (absolute-state self-healing).
        let diff = derive_edges(&diff.accepted, &[], &allowed);
        assert!(diff.accepted.is_empty());
        assert_eq!(
            diff.events,
            vec![
                EngineEvent::Release(RemoteButton::Ok),
                EngineEvent::Release(RemoteButton::Home)
            ]
        );
    }

    #[test]
    fn derive_edges_filters_unknown_and_out_of_set_usages() {
        let back = usage_of(RemoteButton::Back);
        let allowed: BTreeSet<u16> = [back].into_iter().collect();
        // Voice key and an unknown usage are never buttons; a known button
        // outside the current capture set is also rejected.
        let diff = derive_edges(
            &BTreeSet::new(),
            &[back, rc_protocol::buttons::VOICE_KEY_HID_USAGE, 0x1234, usage_of(RemoteButton::Menu)],
            &allowed,
        );
        assert_eq!(diff.accepted, [back].into_iter().collect());
        assert_eq!(diff.events, vec![EngineEvent::Press(RemoteButton::Back)]);
        assert_eq!(diff.rejected, 3);
    }

    #[test]
    fn ownership_requires_current_generation_and_exact_set() {
        let t0 = std::time::Instant::now();
        let mut targets = CaptureTargets {
            generation: 7,
            usages: [usage_of(RemoteButton::Back)].into_iter().collect(),
        };
        targets.install([usage_of(RemoteButton::Ok)].into_iter().collect());
        let mut tracker = OwnershipTracker::default();

        // Stale generation or wrong set: rejected, no ack recorded.
        assert!(!tracker.record(7, &[usage_of(RemoteButton::Ok)].into_iter().collect(), &targets, t0));
        assert!(!tracker.record(targets.generation, &BTreeSet::new(), &targets, t0));
        assert_eq!(tracker.acked_generation(), None);
        assert!(!tracker.valid());
        assert_eq!(tracker.sync_validity(targets.generation, t0), None, "never valid, no transition");

        // Correct ack becomes valid exactly once.
        assert!(tracker.record(
            targets.generation,
            &[usage_of(RemoteButton::Ok)].into_iter().collect(),
            &targets,
            t0
        ));
        assert_eq!(tracker.acked_usages(), &[usage_of(RemoteButton::Ok)].into_iter().collect());
        assert_eq!(tracker.sync_validity(targets.generation, t0), Some(true));
        assert_eq!(tracker.sync_validity(targets.generation, t0), None);
    }

    #[test]
    fn ownership_expires_before_the_agent_lease_window() {
        let t0 = std::time::Instant::now();
        let mut targets = CaptureTargets::default();
        targets.install([usage_of(RemoteButton::Back)].into_iter().collect());
        let mut tracker = OwnershipTracker::default();
        tracker.record(
            targets.generation,
            &targets.usages,
            &targets,
            t0,
        );
        tracker.sync_validity(targets.generation, t0);

        // 1499 ms: still owned (and the agent's 2 s lease would too).
        let just_before = t0 + OWNERSHIP_TIMEOUT - Duration::from_millis(1);
        assert_eq!(tracker.sync_validity(targets.generation, just_before), None);
        assert!(tracker.valid());

        // At 1500 ms ownership is lost, well before the agent's 2 s lease,
        // so the native path recovers first.
        let at_timeout = t0 + OWNERSHIP_TIMEOUT;
        assert_eq!(tracker.sync_validity(targets.generation, at_timeout), Some(false));
        assert!(!tracker.valid());

        // A fresh ack for the current generation re-arms ownership.
        tracker.record(targets.generation, &targets.usages, &targets, at_timeout);
        assert_eq!(tracker.sync_validity(targets.generation, at_timeout), Some(true));
    }

    #[test]
    fn ownership_clear_then_sync_reports_loss_once() {
        let t0 = std::time::Instant::now();
        let mut targets = CaptureTargets::default();
        targets.install([usage_of(RemoteButton::Back)].into_iter().collect());
        let mut tracker = OwnershipTracker::default();
        tracker.record(targets.generation, &targets.usages, &targets, t0);
        assert_eq!(tracker.sync_validity(targets.generation, t0), Some(true));

        // Generation bump path: clear + sync reports the loss transition.
        tracker.clear();
        let new_gen = targets.install(BTreeSet::new());
        assert_eq!(tracker.sync_validity(new_gen, t0), Some(false));
        assert_eq!(tracker.sync_validity(new_gen, t0), None);
    }

    #[test]
    fn silence_and_hello_windows_fire_at_exact_boundaries() {
        let t0 = std::time::Instant::now();
        assert!(!silence_expired(None, t0));
        let at = t0 + SILENCE_TIMEOUT - Duration::from_millis(1);
        assert!(!silence_expired(Some(t0), at));
        assert!(silence_expired(Some(t0), t0 + SILENCE_TIMEOUT));

        assert!(!hello_expired(t0, t0 + HELLO_TIMEOUT - Duration::from_millis(1)));
        assert!(hello_expired(t0, t0 + HELLO_TIMEOUT));
    }

    #[test]
    fn resend_schedule_is_fast_then_steady() {
        // Never sent / write failed: retry immediately.
        assert!(resend_due(None, 0));
        assert!(!resend_due(Some(Duration::from_millis(499)), 0));
        assert!(resend_due(Some(Duration::from_millis(500)), 0));
        assert!(!resend_due(Some(Duration::from_millis(1_499)), 1));
        assert!(resend_due(Some(Duration::from_millis(1_500)), 1));
        assert!(!resend_due(Some(Duration::from_millis(2_999)), 2));
        assert!(resend_due(Some(Duration::from_millis(3_000)), 2));
        assert!(!resend_due(Some(Duration::from_millis(4_999)), 3));
        assert!(resend_due(Some(Duration::from_millis(5_000)), 3));
        assert!(resend_due(Some(Duration::from_millis(5_000)), 50));
    }
}
