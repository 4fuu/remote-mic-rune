//! JSON-lines protocol between the injected agent (inside WUDFHost.exe) and
//! the elevated helper. Transport: loopback named pipe, one JSON object per
//! `\n`-terminated line, both directions. Every helper->agent command carries
//! the token; the agent silently drops commands with a wrong token (a forged
//! renew must never arm the agent).
//!
//! Absolute-state edges: an `Edge` reports the FULL sorted set of currently
//! pressed target usages, not deltas. `usages: []` with reason != "state"
//! means a forced release (disarm / bye / lease expiry / dispose). The
//! receiver derives up/down transitions itself, which self-heals lost lines.

use serde::{Deserialize, Serialize};

use crate::consts::LEASE_MS;

/// Helper -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperToAgent {
    /// Lease renew. The ONLY message required for the agent to stay armed.
    Renew { token: String },
    Arm { token: String },
    Disarm { token: String },
    /// clear=false switches to observe mode: the agent reports edges but
    /// never writes a single byte into reports.
    Mode { token: String, clear: bool },
    /// restore=false: don't restore original report bytes after the call.
    Restore { token: String, on: bool },
    /// Dynamic capture scope. Guards enforced by the agent:
    /// `report` subset of the semantic button whitelist, `clear` covers
    /// `report`, valid generation. Empty sets = full fail-open.
    Targets {
        token: String,
        report: Vec<u16>,
        clear: Vec<u16>,
        generation: u64,
    },
    /// Report-layer synthesis config. `from` must be the voice key usage,
    /// `to` must be in [`crate::buttons::VOICE_SYNTHABLE_USAGES`].
    Synth { token: String, from: u16, to: u16 },
    SynthOff { token: String },
    /// Press-frame presentation delay (gate). on=false disables.
    Gate { token: String, on: bool, delay_ms: Option<u32> },
    /// Ordered teardown: disarm, release edges, ack nothing, close.
    Bye { token: String },
}

/// Agent -> helper, first line after connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentHello {
    pub token: String,
    /// Agent build id, e.g. "rc-agent/0.1.0+<short-hash>". The helper logs a
    /// mismatch against its embedded build (stale-agent detection).
    pub build: String,
    pub pid: u32,
    /// "x64" | "arm64".
    pub arch: String,
    pub mode_clear: bool,
    pub restore: bool,
    pub lease_ms: u32,
    pub report_usages: Vec<u16>,
    pub target_generation: u64,
}

/// Periodic agent heartbeat (every [`crate::consts::AGENT_HB_MS`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentStats {
    pub ioctl_calls: u64,
    pub target_hits: u64,
    /// Reports touched only because of extra clear keys (canary), never
    /// reported as edges. Kept separate so cross-checks stay readable.
    pub canary_hits: u64,
    pub clears_ok: u64,
    pub clears_fail: u64,
    pub restores_ok: u64,
    pub kernel_changed: u64,
    pub edges_sent: u64,
    pub synth_hits: u64,
    pub gate_delays: u64,
    pub lease_expired: u64,
    pub rx_timeouts: u64,
    pub read_errors: u64,
    pub auth_rejected: u64,
    pub cmd_rejected: u64,
    pub send_dropped: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentHeartbeat {
    pub connected: bool,
    pub mode_clear: bool,
    pub report_usages: Vec<u16>,
    pub clear_usages: Vec<u16>,
    pub synth_from: u16,
    pub synth_to: u16,
    pub gate_delay_ms: u32,
    pub target_generation: u64,
    pub disarmed: bool,
    pub lease_ok: bool,
    /// -1 = never renewed.
    pub since_renew_ms: i64,
    pub stats: AgentStats,
}

/// Agent -> helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToHelper {
    Hello {
        t: u64,
        #[serde(flatten)]
        hello: AgentHello,
    },
    Hb {
        t: u64,
        #[serde(flatten)]
        hb: AgentHeartbeat,
    },
    /// Absolute-state edge: `usages` is the full sorted pressed set.
    /// Empty + reason "disarm"|"bye"|"lease_expired"|"dispose" = forced
    /// release of everything.
    Edge {
        t: u64,
        usages: Vec<u16>,
        reason: String,
    },
    /// Agent applied a targets command for `generation`.
    TargetsAck { t: u64, generation: u64, usages: Vec<u16> },
    /// Agent applied synthesis config (absolute state; off acks from=to=0).
    SynthAck { t: u64, from: u16, to: u16 },
    /// Rate-limited diagnostics line (already truncated by the agent).
    Log { t: u64, msg: String },
    /// One synthesized voice frame result: replaced=true means the report
    /// really was rewritten this press.
    VoiceFrame { t: u64, replaced: bool, reason: String },
    Bye { t: u64 },
}

/// Default lease advertised in hello (kept in sync with consts by test).
pub fn default_lease_ms() -> u32 {
    LEASE_MS
}

/// Encode one message as a protocol line (no trailing newline).
pub fn encode_agent_line(msg: &AgentToHelper) -> String {
    serde_json::to_string(msg).expect("agent messages serialize")
}

pub fn encode_helper_line(msg: &HelperToAgent) -> String {
    serde_json::to_string(msg).expect("helper messages serialize")
}

pub fn decode_agent_line(line: &str) -> Option<AgentToHelper> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n'])).ok()
}

pub fn decode_helper_line(line: &str) -> Option<HelperToAgent> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n'])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_round_trip_preserves_absolute_state() {
        let msg = AgentToHelper::Edge {
            t: 12345,
            usages: vec![0x0028, 0x00F1],
            reason: "state".into(),
        };
        let line = encode_agent_line(&msg);
        assert_eq!(decode_agent_line(&line), Some(msg));
    }

    #[test]
    fn targets_command_round_trip() {
        let msg = HelperToAgent::Targets {
            token: "abcdef0123456789".into(),
            report: vec![0x00F1, 0x0080, 0x0081],
            clear: vec![0x00F1, 0x004A, 0x0080, 0x0081],
            generation: 7,
        };
        let line = encode_helper_line(&msg);
        assert!(line.contains(r#""type":"targets""#));
        assert_eq!(decode_helper_line(&line), Some(msg));
    }

    #[test]
    fn hello_flattens_fields() {
        let msg = AgentToHelper::Hello {
            t: 1,
            hello: AgentHello {
                token: "tok".into(),
                build: "rc-agent/0.1.0".into(),
                pid: 4242,
                arch: "x64".into(),
                mode_clear: true,
                restore: true,
                lease_ms: default_lease_ms(),
                report_usages: vec![],
                target_generation: 0,
            },
        };
        let line = encode_agent_line(&msg);
        assert!(line.contains(r#""type":"hello""#));
        assert!(line.contains(r#""pid":4242"#));
        assert_eq!(decode_agent_line(&line), Some(msg));
    }

    #[test]
    fn bad_json_rejected_not_panicked() {
        assert!(decode_agent_line("not json").is_none());
        assert!(decode_helper_line(r#"{"type":"unknown_kind"}"#).is_none());
    }
}
