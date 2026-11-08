//! Pure formatting helpers turning a [`BridgeStatus`] into the status
//! screen's lines. Host-tested; the view only places the strings.

use rc_protocol::RemoteButton;

use crate::bridge_api::{BridgeCounters, BridgePhase, BridgeStatus};

/// One-line phase label for the header badge.
pub fn phase_label(phase: BridgePhase) -> &'static str {
    match phase {
        BridgePhase::Stopped => "Stopped",
        BridgePhase::Listening => "Listening",
        BridgePhase::Connected => "Connected",
    }
}

/// "1.2 s" / "850 ms" for ages in the status list.
pub fn age_label(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1_000.0)
    } else {
        format!("{:.0} min", ms as f64 / 60_000.0)
    }
}

/// The ownership generation line: which capture-target generation is in
/// effect and how fresh the agent's ack of it is. Ownership is only trusted
/// while the ack is fresher than `rc-protocol::OWNERSHIP_TIMEOUT_MS`.
pub fn ownership_line(status: &BridgeStatus) -> String {
    match (status.generation, status.ownership_age_ms) {
        (0, _) => "No capture generation issued yet".to_string(),
        (gen, Some(age)) => {
            let fresh = age <= rc_protocol::consts::OWNERSHIP_TIMEOUT_MS as u64;
            format!(
                "Generation {}, acked {} ago{}",
                gen,
                age_label(age),
                if fresh { "" } else { " (stale)" }
            )
        }
        (gen, None) => format!("Generation {}, not acked yet", gen),
    }
}

/// Buttons whose usages the agent last ACKed owning, by name.
pub fn acked_buttons_line(usages: &[u16]) -> String {
    if usages.is_empty() {
        return "None".to_string();
    }
    let mut names: Vec<&str> = usages
        .iter()
        .filter_map(|u| RemoteButton::from_usage(*u))
        .map(|b| b.name())
        .collect();
    if names.len() != usages.len() {
        return format!("{} usages (unrecognized mix)", usages.len());
    }
    names.sort_unstable();
    names.join(", ")
}

/// Counter line: edges / release-alls / retargets / pings / denies.
pub fn counters_line(c: &BridgeCounters) -> String {
    format!(
        "Edges {} · ReleaseAll {} · Retargets {} · Pings {} · Denies {}",
        c.edges, c.release_alls, c.retargets, c.pings, c.denies
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(gen: u64, age: Option<u64>) -> BridgeStatus {
        BridgeStatus { generation: gen, ownership_age_ms: age, ..Default::default() }
    }

    #[test]
    fn phases_have_short_labels() {
        assert_eq!(phase_label(BridgePhase::Stopped), "Stopped");
        assert_eq!(phase_label(BridgePhase::Listening), "Listening");
        assert_eq!(phase_label(BridgePhase::Connected), "Connected");
    }

    #[test]
    fn ages_render_in_sensible_units() {
        assert_eq!(age_label(0), "0 ms");
        assert_eq!(age_label(850), "850 ms");
        assert_eq!(age_label(1_200), "1.2 s");
        assert_eq!(age_label(75_000), "1 min");
    }

    #[test]
    fn ownership_line_reports_generation_and_ack_freshness() {
        assert_eq!(ownership_line(&status(0, None)), "No capture generation issued yet");
        assert_eq!(ownership_line(&status(3, None)), "Generation 3, not acked yet");
        assert_eq!(ownership_line(&status(3, Some(120))), "Generation 3, acked 120 ms ago");
        // Just past OWNERSHIP_TIMEOUT_MS the line calls the ack stale.
        let stale = status(3, Some(rc_protocol::consts::OWNERSHIP_TIMEOUT_MS as u64 + 1));
        assert!(ownership_line(&stale).contains("(stale)"), "{}", ownership_line(&stale));
    }

    #[test]
    fn acked_buttons_render_as_sorted_names() {
        assert_eq!(acked_buttons_line(&[]), "None");
        let home = RemoteButton::Home.usage();
        let back = RemoteButton::Back.usage();
        assert_eq!(acked_buttons_line(&[home, back]), "back, home");
        assert_eq!(acked_buttons_line(&[0x1234]), "1 usages (unrecognized mix)");
    }

    #[test]
    fn counters_render_on_one_line() {
        let line = counters_line(&BridgeCounters { edges: 12, pings: 3, ..Default::default() });
        assert!(line.contains("Edges 12"));
        assert!(line.contains("Pings 3"));
        assert!(line.contains("Denies 0"));
    }
}
