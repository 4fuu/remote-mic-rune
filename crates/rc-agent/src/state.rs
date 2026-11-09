//! Pure agent state machine: lease, command handling with all guardrails,
//! absolute-state edges, and heartbeat rendering. Platform-independent; the
//! transport feeds it lines and ships back the produced messages.
//!
//! Guardrails enforced end to end:
//! report ⊆ semantic button whitelist, clear ⊇ report, usage 0 rejected,
//! duplicates rejected, token checked on every command before dispatch (a
//! forged renew must never arm the agent), synth targets whitelist-checked,
//! monotonic generation, and fail-open everywhere.

use rc_protocol::agent_wire::{
    decode_helper_line, AgentHeartbeat, AgentHello, AgentStats, AgentToHelper, HelperToAgent,
};
use rc_protocol::buttons::{
    ENHANCED_CAPTURE_BUTTON_USAGES, VOICE_KEY_HID_USAGE, VOICE_SYNTHABLE_USAGES,
};
use rc_protocol::consts::{
    AGENT_AUTH_TIMEOUT_MS, AGENT_RECONNECT_MS, AGENT_RX_TIMEOUT_MS, GATE_DELAY_MS,
};

/// Progressive post-auth-failure reconnect backoff.
/// Not a tunable protocol constant: re-arming too fast only floods the peer
/// that rejected us.
pub const AUTH_BACKOFF_STEPS_MS: [u64; 4] = [2_000, 4_000, 8_000, 15_000];

/// `failures` is the count of consecutive auth failures (>= 1 after a drop).
pub fn auth_backoff_ms(failures: u32) -> u64 {
    if failures == 0 {
        return AUTH_BACKOFF_STEPS_MS[0];
    }
    AUTH_BACKOFF_STEPS_MS[((failures - 1) as usize).min(AUTH_BACKOFF_STEPS_MS.len() - 1)]
}

/// Everything the agent tracks across commands and frames.
#[derive(Debug, Clone)]
pub struct AgentCore {
    // Identity (from the boot config).
    pub token: String,
    pub lease_ms: u32,
    pub hb_ms: u32,
    pub build: String,

    // Capture configuration.
    pub mode_clear: bool,
    pub restore_on_leave: bool,
    pub disarmed: bool,
    pub report_usages: Vec<u16>,
    pub clear_usages: Vec<u16>,
    pub target_generation: u64,

    // Report-layer synthesis.
    pub synth_from: u16,
    pub synth_to: u16,
    /// The OS still believes the synthesized key is down (last frame was
    /// really overwritten); the next frame without the voice key is a release.
    pub synth_held: bool,
    /// Press-frame presentation delay; 0 = gate disabled.
    pub gate_delay_ms: u32,

    // Absolute pressed set (sorted) for edge reporting.
    pub pressed: Vec<u16>,

    // Link state.
    pub connected: bool,
    pub ever_renewed: bool,
    /// Monotonic ms of the last accepted renew; None = never renewed.
    pub last_renew_ms: Option<u64>,
    pub t_connect_ms: Option<u64>,
    pub t_last_rx_ms: Option<u64>,
    pub auth_failures: u32,
    /// Earliest monotonic ms at which a reconnect may be attempted.
    pub retry_not_before_ms: u64,

    pub stats: AgentStats,
}

impl AgentCore {
    pub fn new(token: String, lease_ms: u32, hb_ms: u32, build: String) -> Self {
        AgentCore {
            token,
            lease_ms,
            hb_ms,
            build,
            mode_clear: true,
            restore_on_leave: true,
            disarmed: false,
            report_usages: Vec::new(),
            clear_usages: Vec::new(),
            target_generation: 0,
            synth_from: 0,
            synth_to: 0,
            synth_held: false,
            gate_delay_ms: 0,
            pressed: Vec::new(),
            connected: false,
            ever_renewed: false,
            last_renew_ms: None,
            t_connect_ms: None,
            t_last_rx_ms: None,
            auth_failures: 0,
            retry_not_before_ms: 0,
            stats: AgentStats::default(),
        }
    }

    // ---------------------------------------------------------------- lease

    /// The write license: renewed within the lease window, connected, armed.
    /// Any doubt => false => the hook writes nothing.
    pub fn lease_ok(&self, now_ms: u64) -> bool {
        if self.disarmed || !self.connected {
            return false;
        }
        match self.last_renew_ms {
            None => false,
            Some(t) => now_ms.saturating_sub(t) <= u64::from(self.lease_ms),
        }
    }

    // ---------------------------------------------------------- link state

    pub fn mark_rx(&mut self, now_ms: u64) {
        self.t_last_rx_ms = Some(now_ms);
    }

    pub fn on_connected(&mut self, now_ms: u64) {
        self.connected = true;
        // A new connection starts a fresh auth judgment: only a renew that
        // actually arrives on THIS connection may later classify silence as
        // a link problem instead of a token problem.
        self.ever_renewed = false;
        self.t_connect_ms = Some(now_ms);
        self.t_last_rx_ms = Some(now_ms);
    }

    /// Watchdogs, in a fixed order: auth first ("the peer does not
    /// recognize us"), then rx silence ("the peer is gone").
    pub fn watchdog_hit(&self, now_ms: u64) -> Option<(&'static str, bool)> {
        if self.connected {
            if let Some(tc) = self.t_connect_ms {
                if !self.ever_renewed
                    && now_ms.saturating_sub(tc) > u64::from(AGENT_AUTH_TIMEOUT_MS)
                {
                    return Some(("auth_mismatch", true));
                }
            }
            if let Some(tr) = self.t_last_rx_ms {
                if now_ms.saturating_sub(tr) > u64::from(AGENT_RX_TIMEOUT_MS) {
                    return Some(("rx_silence", false));
                }
            }
        }
        None
    }

    /// Apply a connection drop: backoff bookkeeping and flag reset.
    /// `slow` = auth-flavored drop (progressive backoff); otherwise the
    /// plain reconnect interval applies and the auth failure count resets.
    /// Returns the `dropped:` log line.
    pub fn drop_state(&mut self, reason: &str, slow: bool, now_ms: u64) -> String {
        self.connected = false;
        if slow {
            self.auth_failures += 1;
            self.retry_not_before_ms = now_ms + auth_backoff_ms(self.auth_failures);
        } else {
            self.auth_failures = 0;
            self.retry_not_before_ms = now_ms + u64::from(AGENT_RECONNECT_MS);
        }
        self.ever_renewed = false;
        self.t_connect_ms = None;
        self.t_last_rx_ms = None;
        format!("dropped:{reason}")
    }

    pub fn can_retry_connect(&self, now_ms: u64) -> bool {
        !self.connected && now_ms >= self.retry_not_before_ms
    }

    // --------------------------------------------------------------- edges

    fn same_sorted_set(a: &[u16], b: &[u16]) -> bool {
        a == b
    }

    /// Absolute-state edge on change. `found` must be sorted. Only state
    /// changes count toward `edges_sent`.
    pub fn emit_edges_if_changed(&mut self, found: &[u16], unix_ms: u64) -> Option<AgentToHelper> {
        if Self::same_sorted_set(&self.pressed, found) {
            return None;
        }
        self.pressed = found.to_vec();
        self.stats.edges_sent += 1;
        Some(AgentToHelper::Edge {
            t: unix_ms,
            usages: self.pressed.clone(),
            reason: "state".into(),
        })
    }

    /// Forced full-release message (empty set + reason) when anything was
    /// pressed; nothing sent when already empty.
    pub fn release_edges(&mut self, reason: &str, unix_ms: u64) -> Option<AgentToHelper> {
        if self.pressed.is_empty() {
            return None;
        }
        self.pressed.clear();
        Some(AgentToHelper::Edge {
            t: unix_ms,
            usages: Vec::new(),
            reason: reason.to_string(),
        })
    }

    /// Heartbeat-time lease expiry: stop clears, release pressed state.
    pub fn lease_tick(&mut self, now_ms: u64, unix_ms: u64) -> Option<AgentToHelper> {
        if !self.lease_ok(now_ms)
            && self.last_renew_ms.is_some()
            && self.connected
            && !self.disarmed
        {
            let msg = self.release_edges("lease_expired", unix_ms);
            self.last_renew_ms = None;
            return msg;
        }
        None
    }

    // ------------------------------------------------------------ messages

    pub fn log_msg(msg: impl Into<String>, unix_ms: u64) -> AgentToHelper {
        AgentToHelper::Log {
            t: unix_ms,
            msg: msg.into(),
        }
    }

    pub fn hello_msg(&self, pid: u32, arch: &str, unix_ms: u64) -> AgentToHelper {
        AgentToHelper::Hello {
            t: unix_ms,
            hello: AgentHello {
                token: self.token.clone(),
                build: self.build.clone(),
                pid,
                arch: arch.to_string(),
                mode_clear: self.mode_clear,
                restore: self.restore_on_leave,
                lease_ms: self.lease_ms,
                report_usages: self.report_usages.clone(),
                target_generation: self.target_generation,
            },
        }
    }

    pub fn heartbeat_msg(&self, now_ms: u64, unix_ms: u64) -> AgentToHelper {
        AgentToHelper::Hb {
            t: unix_ms,
            hb: AgentHeartbeat {
                connected: self.connected,
                mode_clear: self.mode_clear,
                report_usages: self.report_usages.clone(),
                clear_usages: self.clear_usages.clone(),
                synth_from: self.synth_from,
                synth_to: self.synth_to,
                gate_delay_ms: self.gate_delay_ms,
                target_generation: self.target_generation,
                disarmed: self.disarmed,
                lease_ok: self.lease_ok(now_ms),
                since_renew_ms: match self.last_renew_ms {
                    None => -1,
                    Some(t) => now_ms.saturating_sub(t) as i64,
                },
                stats: self.stats.clone(),
            },
        }
    }

    // ------------------------------------------------------------- command

    /// Decode and dispatch one helper line. Returns the outgoing messages in
    /// order (logs, acks, edges). A token mismatch rejects the command
    /// entirely: the counter is the only trace.
    pub fn handle_line(&mut self, line: &str, now_ms: u64, unix_ms: u64) -> Vec<AgentToHelper> {
        self.mark_rx(now_ms);
        let Some(cmd) = decode_helper_line(line) else {
            self.stats.cmd_rejected += 1;
            let head: String = line.chars().take(32).collect();
            return vec![Self::log_msg(
                format!("cmd:parse_failed len={} head={head}", line.len()),
                unix_ms,
            )];
        };
        if !self.token.is_empty() && command_token(&cmd) != Some(self.token.as_str()) {
            self.stats.cmd_rejected += 1;
            return Vec::new();
        }
        self.dispatch(cmd, now_ms, unix_ms)
    }

    fn dispatch(&mut self, cmd: HelperToAgent, now_ms: u64, unix_ms: u64) -> Vec<AgentToHelper> {
        let mut out = Vec::new();
        // Trace every non-renew command; renew is 2/s and would flood the log.
        if !matches!(cmd, HelperToAgent::Renew { .. }) {
            out.push(Self::log_msg(format!("cmd:{}", command_kind(&cmd)), unix_ms));
        }
        match cmd {
            HelperToAgent::Renew { .. } => {
                if !self.ever_renewed {
                    out.push(Self::log_msg("renew:first", unix_ms));
                }
                self.last_renew_ms = Some(now_ms);
                self.ever_renewed = true;
                self.auth_failures = 0;
            }
            HelperToAgent::Arm { .. } => {
                self.disarmed = false;
            }
            HelperToAgent::Disarm { .. } => {
                self.disarmed = true;
                self.last_renew_ms = None;
                if let Some(m) = self.release_edges("disarm", unix_ms) {
                    out.push(m);
                }
            }
            HelperToAgent::Mode { clear, .. } => {
                self.mode_clear = clear;
                out.push(Self::log_msg(
                    if clear { "mode:clear" } else { "mode:observe" },
                    unix_ms,
                ));
            }
            HelperToAgent::Restore { on, .. } => {
                self.restore_on_leave = on;
            }
            HelperToAgent::Targets {
                report,
                clear,
                generation,
                ..
            } => {
                self.apply_targets(report, clear, generation, unix_ms, &mut out);
            }
            HelperToAgent::Synth { from, to, .. } => {
                self.apply_synth(from, to, unix_ms, &mut out);
            }
            HelperToAgent::SynthOff { .. } => {
                self.synth_from = 0;
                self.synth_to = 0;
                self.synth_held = false;
                out.push(AgentToHelper::SynthAck {
                    t: unix_ms,
                    from: 0,
                    to: 0,
                });
                out.push(Self::log_msg("synth:off", unix_ms));
            }
            HelperToAgent::Gate { on, delay_ms, .. } => {
                // Idempotent; delay_ms may override the constant for tuning,
                // anything out of range falls back to the default.
                let want = match delay_ms {
                    Some(d) if d > 0 && d <= 2000 => d,
                    _ => GATE_DELAY_MS,
                };
                self.gate_delay_ms = if on { want } else { 0 };
                out.push(Self::log_msg(
                    if self.gate_delay_ms > 0 {
                        format!("gate:on delay_ms={}", self.gate_delay_ms)
                    } else {
                        "gate:off".to_string()
                    },
                    unix_ms,
                ));
            }
            HelperToAgent::Bye { .. } => {
                self.disarmed = true;
                self.last_renew_ms = None;
                if let Some(m) = self.release_edges("bye", unix_ms) {
                    out.push(m);
                }
            }
        }
        out
    }

    fn apply_targets(
        &mut self,
        report: Vec<u16>,
        clear: Vec<u16>,
        generation: u64,
        unix_ms: u64,
        out: &mut Vec<AgentToHelper>,
    ) {
        if let Err(reason) = validate_targets(&report, &clear) {
            self.stats.cmd_rejected += 1;
            out.push(Self::log_msg(reason, unix_ms));
            return;
        }
        // Keys leaving the report set must release before the swap, so the
        // receiver never keeps a stale DOWN across a remap.
        let mut report = report;
        let mut clear = clear;
        report.sort_unstable();
        clear.sort_unstable();
        let retained: Vec<u16> = self
            .pressed
            .iter()
            .copied()
            .filter(|u| report.contains(u))
            .collect();
        self.report_usages = report.clone();
        self.clear_usages = clear;
        self.target_generation = generation;
        if let Some(m) = self.emit_edges_if_changed(&retained, unix_ms) {
            out.push(m);
        }
        out.push(AgentToHelper::TargetsAck {
            t: unix_ms,
            generation,
            usages: self.report_usages.clone(),
        });
        out.push(Self::log_msg(
            format!(
                "targets:applied generation={} report={} clear={}",
                generation,
                usages_hex(&self.report_usages),
                usages_hex(&self.clear_usages)
            ),
            unix_ms,
        ));
    }

    fn apply_synth(&mut self, from: u16, to: u16, unix_ms: u64, out: &mut Vec<AgentToHelper>) {
        let ok = from == VOICE_KEY_HID_USAGE
            && VOICE_SYNTHABLE_USAGES.contains(&to)
            && from != to;
        if !ok {
            out.push(Self::log_msg(
                format!("synth:rejected from={} to={}", usages_hex(&[from]), usages_hex(&[to])),
                unix_ms,
            ));
            return;
        }
        self.synth_from = from;
        self.synth_to = to;
        out.push(AgentToHelper::SynthAck {
            t: unix_ms,
            from,
            to,
        });
        out.push(Self::log_msg(
            format!(
                "synth:applied from={} to={}",
                usages_hex(&[from]),
                usages_hex(&[to])
            ),
            unix_ms,
        ));
    }
}

fn command_kind(cmd: &HelperToAgent) -> &'static str {
    match cmd {
        HelperToAgent::Renew { .. } => "renew",
        HelperToAgent::Arm { .. } => "arm",
        HelperToAgent::Disarm { .. } => "disarm",
        HelperToAgent::Mode { .. } => "mode",
        HelperToAgent::Restore { .. } => "restore",
        HelperToAgent::Targets { .. } => "targets",
        HelperToAgent::Synth { .. } => "synth",
        HelperToAgent::SynthOff { .. } => "synth_off",
        HelperToAgent::Gate { .. } => "gate",
        HelperToAgent::Bye { .. } => "bye",
    }
}

fn command_token(cmd: &HelperToAgent) -> Option<&str> {
    match cmd {
        HelperToAgent::Renew { token }
        | HelperToAgent::Arm { token }
        | HelperToAgent::Disarm { token }
        | HelperToAgent::Mode { token, .. }
        | HelperToAgent::Restore { token, .. }
        | HelperToAgent::Targets { token, .. }
        | HelperToAgent::Synth { token, .. }
        | HelperToAgent::SynthOff { token }
        | HelperToAgent::Gate { token, .. }
        | HelperToAgent::Bye { token } => Some(token.as_str()),
    }
}

/// Elements must be 1..=0xFFFF (0 is the empty-slot marker, never a key) and
/// duplicate-free. Empty lists are valid (fail-open configuration).
fn valid_usage_list(v: &[u16]) -> bool {
    let mut sorted = v.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len() == v.len() && v.iter().all(|&u| u != 0)
}

fn is_whitelisted(u: u16) -> bool {
    ENHANCED_CAPTURE_BUTTON_USAGES.iter().any(|&(_, w)| w == u)
}

fn validate_targets(report: &[u16], clear: &[u16]) -> Result<(), &'static str> {
    if !valid_usage_list(report) || !valid_usage_list(clear) {
        return Err("targets:rejected_bad_array");
    }
    if report.iter().any(|&u| !is_whitelisted(u)) {
        return Err("targets:rejected_outside_whitelist");
    }
    if report.iter().any(|u| !clear.contains(u)) {
        return Err("targets:rejected_clear_lacks_report");
    }
    Ok(())
}

/// `0x00f1,0x0080` style rendering for logs and ack payloads.
pub fn usages_hex(us: &[u16]) -> String {
    us.iter()
        .map(|u| format!("0x{u:04x}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_protocol::agent_wire::encode_helper_line;

    const TOKEN: &str = "0123456789abcdef";

    fn core() -> AgentCore {
        AgentCore::new(TOKEN.to_string(), 2000, 500, "rc-agent/test".into())
    }

    fn connect(core: &mut AgentCore, now: u64) {
        core.on_connected(now);
    }

    fn send(core: &mut AgentCore, msg: &HelperToAgent, now: u64, unix: u64) -> Vec<AgentToHelper> {
        core.handle_line(&encode_helper_line(msg), now, unix)
    }

    fn edge_usages(msgs: &[AgentToHelper]) -> Vec<&Vec<u16>> {
        msgs.iter()
            .filter_map(|m| match m {
                AgentToHelper::Edge { usages, .. } => Some(usages),
                _ => None,
            })
            .collect()
    }

    // ------------------------------------------------------------- targets

    #[test]
    fn targets_apply_acks_and_swaps_sets() {
        let mut c = core();
        connect(&mut c, 0);
        let msgs = send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![0x0080, 0x00F1],
                clear: vec![0x00F1, 0x0080, 0x004A],
                generation: 7,
            },
            10,
            100,
        );
        assert_eq!(c.report_usages, vec![0x0080, 0x00F1]); // sorted on store
        assert_eq!(c.clear_usages, vec![0x004A, 0x0080, 0x00F1]);
        assert_eq!(c.target_generation, 7);
        assert!(matches!(
            msgs.iter().find(|m| matches!(m, AgentToHelper::TargetsAck { .. })),
            Some(AgentToHelper::TargetsAck { generation: 7, usages, .. }) if usages == &vec![0x0080, 0x00F1]
        ));
    }

    #[test]
    fn targets_reject_usage_outside_whitelist() {
        let mut c = core();
        connect(&mut c, 0);
        let msgs = send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![0x00F1, 0x1234],
                clear: vec![0x00F1, 0x1234],
                generation: 1,
            },
            0,
            0,
        );
        assert_eq!(c.stats.cmd_rejected, 1);
        assert_eq!(c.target_generation, 0);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("outside_whitelist"))));
    }

    #[test]
    fn targets_reject_clear_missing_report_usage() {
        let mut c = core();
        connect(&mut c, 0);
        let msgs = send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![0x00F1, 0x0080],
                clear: vec![0x00F1],
                generation: 1,
            },
            0,
            0,
        );
        assert_eq!(c.stats.cmd_rejected, 1);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("clear_lacks_report"))));
    }

    #[test]
    fn targets_reject_zero_and_duplicate_usages() {
        let mut c = core();
        connect(&mut c, 0);
        for report in [&[0u16][..], &[0x00F1, 0x00F1][..]] {
            let msgs = send(
                &mut c,
                &HelperToAgent::Targets {
                    token: TOKEN.into(),
                    report: report.to_vec(),
                    clear: report.to_vec(),
                    generation: 1,
                },
                0,
                0,
            );
            assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("bad_array"))));
        }
        assert_eq!(c.stats.cmd_rejected, 2);
    }

    #[test]
    fn targets_removal_releases_dropped_keys_first() {
        let mut c = core();
        connect(&mut c, 0);
        send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![0x00F1, 0x0080],
                clear: vec![0x00F1, 0x0080],
                generation: 1,
            },
            0,
            0,
        );
        // both pressed
        let line = encode_helper_line(&HelperToAgent::Renew { token: TOKEN.into() });
        c.handle_line(&line, 10, 10);
        let pressed_after = c.emit_edges_if_changed(&[0x00F1, 0x0080], 20);
        assert!(pressed_after.is_some());

        // retarget to back only: volume_up must appear as released in the
        // edge that precedes the ack
        let msgs = send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![0x00F1],
                clear: vec![0x00F1],
                generation: 2,
            },
            30,
            40,
        );
        let edges = edge_usages(&msgs);
        assert_eq!(edges, vec![&vec![0x00F1]]);
        assert_eq!(c.pressed, vec![0x00F1]);
        // ack comes after the edge
        let ack_pos = msgs
            .iter()
            .position(|m| matches!(m, AgentToHelper::TargetsAck { generation: 2, .. }))
            .unwrap();
        let edge_pos = msgs
            .iter()
            .position(|m| matches!(m, AgentToHelper::Edge { .. }))
            .unwrap();
        assert!(edge_pos < ack_pos);
    }

    #[test]
    fn empty_targets_is_valid_fail_open_config() {
        let mut c = core();
        connect(&mut c, 0);
        let msgs = send(
            &mut c,
            &HelperToAgent::Targets {
                token: TOKEN.into(),
                report: vec![],
                clear: vec![],
                generation: 3,
            },
            0,
            0,
        );
        assert!(c.report_usages.is_empty() && c.clear_usages.is_empty());
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::TargetsAck { generation: 3, .. })));
    }

    // --------------------------------------------------------------- token

    #[test]
    fn forged_renew_never_arms_the_agent() {
        let mut c = core();
        connect(&mut c, 0);
        let before = c.last_renew_ms;
        let msgs = send(&mut c, &HelperToAgent::Renew { token: "wrong".into() }, 100, 100);
        assert!(msgs.is_empty());
        assert_eq!(c.stats.cmd_rejected, 1);
        assert_eq!(c.last_renew_ms, before);
        assert!(!c.ever_renewed);
        assert!(!c.lease_ok(150));
    }

    #[test]
    fn every_command_kind_checks_the_token() {
        let mut c = core();
        connect(&mut c, 0);
        let bad = [
            HelperToAgent::Arm { token: "x".into() },
            HelperToAgent::Disarm { token: "x".into() },
            HelperToAgent::Mode { token: "x".into(), clear: true },
            HelperToAgent::Restore { token: "x".into(), on: true },
            HelperToAgent::Targets { token: "x".into(), report: vec![], clear: vec![], generation: 1 },
            HelperToAgent::Synth { token: "x".into(), from: 0x3E, to: 0xE6 },
            HelperToAgent::SynthOff { token: "x".into() },
            HelperToAgent::Gate { token: "x".into(), on: true, delay_ms: None },
            HelperToAgent::Bye { token: "x".into() },
        ];
        for cmd in &bad {
            send(&mut c, &cmd, 0, 0);
        }
        assert_eq!(c.stats.cmd_rejected, bad.len() as u64);
        // nothing was applied
        assert!(c.report_usages.is_empty() && c.synth_from == 0 && !c.disarmed);
    }

    #[test]
    fn malformed_line_counts_and_logs_without_panicking() {
        let mut c = core();
        connect(&mut c, 0);
        let msgs = c.handle_line("this is not json", 0, 0);
        assert_eq!(c.stats.cmd_rejected, 1);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("parse_failed"))));
    }

    // ---------------------------------------------------------------- lease

    #[test]
    fn lease_requires_renew_connection_and_arm() {
        let mut c = core();
        // never connected, never renewed
        assert!(!c.lease_ok(0));

        connect(&mut c, 100);
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 150, 0);
        assert!(c.lease_ok(150));
        assert!(c.lease_ok(150 + 2000)); // exactly LEASE_MS is still ok
        assert!(!c.lease_ok(150 + 2001)); // one ms past: expired

        // disconnected kills the lease even with a fresh renew
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 1000, 0);
        c.connected = false;
        assert!(!c.lease_ok(1001));

        // disarmed kills the lease
        c.connected = true;
        send(&mut c, &HelperToAgent::Disarm { token: TOKEN.into() }, 1002, 0);
        assert!(c.disarmed);
        assert!(!c.lease_ok(1003));
    }

    #[test]
    fn disarm_releases_pressed_keys_with_reason() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 0, 0);
        c.emit_edges_if_changed(&[0x00F1], 1);
        let msgs = send(&mut c, &HelperToAgent::Disarm { token: TOKEN.into() }, 2, 3);
        let edges = edge_usages(&msgs);
        assert_eq!(edges.len(), 1);
        assert!(edges[0].is_empty());
        assert!(matches!(
            msgs.iter().find(|m| matches!(m, AgentToHelper::Edge { reason, .. } if reason == "disarm")),
            Some(_)
        ));
        assert!(c.pressed.is_empty());
        assert_eq!(c.last_renew_ms, None);
    }

    #[test]
    fn bye_disarms_and_releases() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 0, 0);
        c.emit_edges_if_changed(&[0x0080], 1);
        let msgs = send(&mut c, &HelperToAgent::Bye { token: TOKEN.into() }, 2, 3);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Edge { reason, usages, .. } if reason == "bye" && usages.is_empty())));
        assert!(c.disarmed);
        assert!(!c.lease_ok(4));
    }

    #[test]
    fn lease_tick_fires_once_and_releases_with_reason() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 0, 0);
        c.emit_edges_if_changed(&[0x00F1], 1);

        let msg = c.lease_tick(3000, 4); // 3000 - 0 > 2000
        assert!(msg.is_some());
        assert!(matches!(
            msg,
            Some(AgentToHelper::Edge { reason, usages, .. }) if reason == "lease_expired" && usages.is_empty()
        ));
        assert_eq!(c.last_renew_ms, None);

        // renew cleared: a second tick does nothing (no double fire)
        assert!(c.lease_tick(5000, 6).is_none());
    }

    #[test]
    fn arm_recovers_after_disarm() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Disarm { token: TOKEN.into() }, 0, 0);
        assert!(!c.lease_ok(1));
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 2, 0);
        assert!(!c.lease_ok(3), "disarm must survive renew until arm");
        send(&mut c, &HelperToAgent::Arm { token: TOKEN.into() }, 4, 0);
        assert!(c.lease_ok(5), "arm restores the license within the lease");
    }

    // ---------------------------------------------------------------- synth

    #[test]
    fn synth_whitelist_guards() {
        let mut c = core();
        connect(&mut c, 0);

        // from must be the voice key
        let msgs = send(&mut c, &HelperToAgent::Synth { token: TOKEN.into(), from: 0x00F1, to: 0x00E6 }, 0, 0);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("synth:rejected"))));
        assert_eq!(c.synth_from, 0);

        // to must be on the measured whitelist
        let msgs = send(&mut c, &HelperToAgent::Synth { token: TOKEN.into(), from: 0x003E, to: 0x1234 }, 0, 0);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("synth:rejected"))));

        // from == to is nonsense, reject
        let msgs = send(&mut c, &HelperToAgent::Synth { token: TOKEN.into(), from: 0x003E, to: 0x003E }, 0, 0);
        assert!(msgs.iter().any(|m| matches!(m, AgentToHelper::Log { msg, .. } if msg.contains("synth:rejected"))));

        // good pair
        let msgs = send(&mut c, &HelperToAgent::Synth { token: TOKEN.into(), from: 0x003E, to: 0x00E6 }, 0, 0);
        assert!(matches!(
            msgs.iter().find(|m| matches!(m, AgentToHelper::SynthAck { .. })),
            Some(AgentToHelper::SynthAck { from: 0x003E, to: 0x00E6, .. })
        ));
        assert_eq!(c.synth_from, 0x003E);
        assert_eq!(c.synth_to, 0x00E6);
    }

    #[test]
    fn synth_off_resets_held_state_and_acks_zero() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Synth { token: TOKEN.into(), from: 0x003E, to: 0x00E2 }, 0, 0);
        c.synth_held = true;
        let msgs = send(&mut c, &HelperToAgent::SynthOff { token: TOKEN.into() }, 0, 0);
        assert_eq!(c.synth_from, 0);
        assert_eq!(c.synth_to, 0);
        assert!(!c.synth_held);
        assert!(matches!(
            msgs.iter().find(|m| matches!(m, AgentToHelper::SynthAck { .. })),
            Some(AgentToHelper::SynthAck { from: 0, to: 0, .. })
        ));
    }

    // ----------------------------------------------------------------- gate

    #[test]
    fn gate_command_sets_and_clamps_delay() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Gate { token: TOKEN.into(), on: true, delay_ms: None }, 0, 0);
        assert_eq!(c.gate_delay_ms, GATE_DELAY_MS);
        send(&mut c, &HelperToAgent::Gate { token: TOKEN.into(), on: true, delay_ms: Some(2000) }, 0, 0);
        assert_eq!(c.gate_delay_ms, 2000);
        send(&mut c, &HelperToAgent::Gate { token: TOKEN.into(), on: true, delay_ms: Some(0) }, 0, 0);
        assert_eq!(c.gate_delay_ms, GATE_DELAY_MS, "non-positive override falls back");
        send(&mut c, &HelperToAgent::Gate { token: TOKEN.into(), on: true, delay_ms: Some(3000) }, 0, 0);
        assert_eq!(c.gate_delay_ms, GATE_DELAY_MS, "over-range override falls back");
        send(&mut c, &HelperToAgent::Gate { token: TOKEN.into(), on: false, delay_ms: None }, 0, 0);
        assert_eq!(c.gate_delay_ms, 0);
    }

    #[test]
    fn mode_and_restore_switch_state() {
        let mut c = core();
        connect(&mut c, 0);
        assert!(c.mode_clear && c.restore_on_leave);
        send(&mut c, &HelperToAgent::Mode { token: TOKEN.into(), clear: false }, 0, 0);
        assert!(!c.mode_clear);
        send(&mut c, &HelperToAgent::Mode { token: TOKEN.into(), clear: true }, 0, 0);
        assert!(c.mode_clear);
        send(&mut c, &HelperToAgent::Restore { token: TOKEN.into(), on: false }, 0, 0);
        assert!(!c.restore_on_leave);
        send(&mut c, &HelperToAgent::Restore { token: TOKEN.into(), on: true }, 0, 0);
        assert!(c.restore_on_leave);
    }

    // ------------------------------------------------------- link lifecycle

    #[test]
    fn auth_watchdog_fires_before_rx_watchdog() {
        let mut c = core();
        c.on_connected(0);
        // at 4001ms with no renew ever: auth mismatch (slow path)
        assert_eq!(c.watchdog_hit(4001), Some(("auth_mismatch", true)));
        // renew stops the auth watchdog
        c.handle_line(&encode_helper_line(&HelperToAgent::Renew { token: TOKEN.into() }), 100, 0);
        assert!(c.watchdog_hit(2000).is_none());
        assert_eq!(c.watchdog_hit(4000), Some(("rx_silence", false)));
    }

    #[test]
    fn lease_silence_is_not_misjudged_as_auth_failure() {
        // The subtle bug this test pins: using lastRenewAt == 0 to mean
        // "never renewed" made a long-lived connection's ordinary stall look
        // like an auth failure. ever_renewed must gate the auth watchdog
        // instead.
        let mut c = core();
        c.on_connected(0);
        c.handle_line(&encode_helper_line(&HelperToAgent::Renew { token: TOKEN.into() }), 100, 0);
        c.last_renew_ms = None; // lease expired long ago, renew count reset
        assert_eq!(c.watchdog_hit(6000), Some(("rx_silence", false)));
    }

    #[test]
    fn drop_backoff_progresses_and_plain_drops_reset_it() {
        let mut c = core();
        c.on_connected(0);
        c.drop_state("auth_mismatch", true, 0);
        assert_eq!(c.retry_not_before_ms, 2000);
        c.on_connected(2000);
        c.drop_state("auth_mismatch", true, 2000);
        assert_eq!(c.retry_not_before_ms, 6000);
        c.on_connected(6000);
        c.drop_state("auth_mismatch", true, 6000);
        assert_eq!(c.retry_not_before_ms, 14000);
        // caps at the last step
        c.on_connected(14000);
        c.drop_state("auth_mismatch", true, 14000);
        c.on_connected(29000);
        c.drop_state("auth_mismatch", true, 29000);
        assert_eq!(c.retry_not_before_ms, 29000 + 15000);

        // plain drop resets the failure count to the 1s reconnect interval
        c.on_connected(44000);
        c.drop_state("rx_silence", false, 44000);
        assert_eq!(c.retry_not_before_ms, 45000);
        assert_eq!(c.auth_failures, 0);
        assert!(c.can_retry_connect(45000));
        assert!(!c.can_retry_connect(44999));
    }

    #[test]
    fn hello_carries_full_current_state() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Targets { token: TOKEN.into(), report: vec![0x00F1], clear: vec![0x00F1], generation: 5 }, 0, 0);
        let msg = c.hello_msg(4242, "x64", 77);
        let AgentToHelper::Hello { t, hello } = msg else {
            panic!("expected hello");
        };
        assert_eq!(t, 77);
        assert_eq!(hello.pid, 4242);
        assert_eq!(hello.arch, "x64");
        assert_eq!(hello.token, TOKEN);
        assert_eq!(hello.report_usages, vec![0x00F1]);
        assert_eq!(hello.target_generation, 5);
        assert!(hello.mode_clear && hello.restore);
        assert_eq!(hello.lease_ms, 2000);
    }

    #[test]
    fn heartbeat_renders_full_state_and_since_renew() {
        let mut c = core();
        connect(&mut c, 0);
        send(&mut c, &HelperToAgent::Renew { token: TOKEN.into() }, 100, 0);
        let msg = c.heartbeat_msg(500, 900);
        let AgentToHelper::Hb { t, hb } = msg else {
            panic!("expected hb");
        };
        assert_eq!(t, 900);
        assert!(hb.connected && hb.lease_ok);
        assert_eq!(hb.since_renew_ms, 400);
        assert_eq!(hb.stats.ioctl_calls, 0);
        // never-renewed renders -1
        let mut c2 = core();
        c2.on_connected(0);
        let AgentToHelper::Hb { hb, .. } = c2.heartbeat_msg(100, 100) else {
            panic!("expected hb");
        };
        assert_eq!(hb.since_renew_ms, -1);
        assert!(!hb.lease_ok);
    }

    #[test]
    fn release_edges_is_silent_when_nothing_pressed() {
        let mut c = core();
        assert!(c.release_edges("dispose", 0).is_none());
    }

    #[test]
    fn auth_backoff_shape() {
        assert_eq!(auth_backoff_ms(0), 2000);
        assert_eq!(auth_backoff_ms(1), 2000);
        assert_eq!(auth_backoff_ms(2), 4000);
        assert_eq!(auth_backoff_ms(3), 8000);
        assert_eq!(auth_backoff_ms(4), 15000);
        assert_eq!(auth_backoff_ms(99), 15000);
    }

    #[test]
    fn usages_hex_format_is_stable() {
        assert_eq!(usages_hex(&[0x00F1, 0x0080]), "0x00f1,0x0080");
        assert_eq!(usages_hex(&[]), "");
    }
}
