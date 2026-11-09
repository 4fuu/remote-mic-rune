//! Pure formatting helpers turning a [`BridgeStatus`] into the status
//! screen's lines, in the UI's language. Host-tested; the view only places
//! the strings.

use rc_protocol::RemoteButton;

use crate::bridge_api::{BridgeCounters, BridgePhase, BridgeStatus};
use crate::i18n::{tr, tr_fmt, Lang};

/// One-line phase label for the header badge.
pub fn phase_label(lang: Lang, phase: BridgePhase) -> &'static str {
    match phase {
        BridgePhase::Stopped => tr(lang, "Stopped"),
        BridgePhase::Listening => tr(lang, "Listening"),
        BridgePhase::Connected => tr(lang, "Connected"),
    }
}

/// "1.2 s" / "850 ms" for ages in the status list.
pub fn age_label(lang: Lang, ms: u64) -> String {
    if ms < 1_000 {
        tr_fmt!(lang, "{} ms", ms)
    } else if ms < 60_000 {
        tr_fmt!(lang, "{} s", format!("{:.1}", ms as f64 / 1_000.0))
    } else {
        tr_fmt!(lang, "{} min", format!("{:.0}", ms as f64 / 60_000.0))
    }
}

/// The ownership generation line: which capture-target generation is in
/// effect and how fresh the agent's ack of it is. Ownership is only trusted
/// while the ack is fresher than `rc-protocol::OWNERSHIP_TIMEOUT_MS`.
pub fn ownership_line(lang: Lang, status: &BridgeStatus) -> String {
    match (status.generation, status.ownership_age_ms) {
        (0, _) => tr(lang, "No capture generation issued yet").to_string(),
        (gen, Some(age)) => {
            let fresh = age <= rc_protocol::consts::OWNERSHIP_TIMEOUT_MS as u64;
            tr_fmt!(
                lang,
                "Generation {}, acked {} ago{}",
                gen,
                age_label(lang, age),
                if fresh { "" } else { tr(lang, " (stale)") }
            )
        }
        (gen, None) => tr_fmt!(lang, "Generation {}, not acked yet", gen),
    }
}

/// Buttons whose usages the agent last ACKed owning, by name. The names are
/// the protocol's stable identifiers (`back`, `home`, …) and stay English
/// like the rest of the protocol surface.
pub fn acked_buttons_line(lang: Lang, usages: &[u16]) -> String {
    if usages.is_empty() {
        return tr(lang, "None").to_string();
    }
    let mut names: Vec<&str> = usages
        .iter()
        .filter_map(|u| RemoteButton::from_usage(*u))
        .map(|b| b.name())
        .collect();
    if names.len() != usages.len() {
        return tr_fmt!(lang, "{} usages (unrecognized mix)", usages.len());
    }
    names.sort_unstable();
    names.join(", ")
}

/// Counter line: edges / release-alls / retargets / pings / denies.
pub fn counters_line(lang: Lang, c: &BridgeCounters) -> String {
    tr_fmt!(
        lang,
        "Edges {} · ReleaseAll {} · Retargets {} · Pings {} · Denies {}",
        c.edges,
        c.release_alls,
        c.retargets,
        c.pings,
        c.denies
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
        assert_eq!(phase_label(Lang::En, BridgePhase::Stopped), "Stopped");
        assert_eq!(phase_label(Lang::En, BridgePhase::Listening), "Listening");
        assert_eq!(phase_label(Lang::En, BridgePhase::Connected), "Connected");
        assert_eq!(phase_label(Lang::Zh, BridgePhase::Stopped), "已停止");
        assert_eq!(phase_label(Lang::Zh, BridgePhase::Connected), "已连接");
    }

    #[test]
    fn ages_render_in_sensible_units() {
        assert_eq!(age_label(Lang::En, 0), "0 ms");
        assert_eq!(age_label(Lang::En, 850), "850 ms");
        assert_eq!(age_label(Lang::En, 1_200), "1.2 s");
        assert_eq!(age_label(Lang::En, 75_000), "1 min");
        assert_eq!(age_label(Lang::Zh, 850), "850 毫秒");
        assert_eq!(age_label(Lang::Zh, 1_200), "1.2 秒");
    }

    #[test]
    fn ownership_line_reports_generation_and_ack_freshness() {
        assert_eq!(
            ownership_line(Lang::En, &status(0, None)),
            "No capture generation issued yet"
        );
        assert_eq!(
            ownership_line(Lang::En, &status(3, None)),
            "Generation 3, not acked yet"
        );
        assert_eq!(
            ownership_line(Lang::En, &status(3, Some(120))),
            "Generation 3, acked 120 ms ago"
        );
        // Just past OWNERSHIP_TIMEOUT_MS the line calls the ack stale.
        let stale = status(
            3,
            Some(rc_protocol::consts::OWNERSHIP_TIMEOUT_MS as u64 + 1),
        );
        assert!(
            ownership_line(Lang::En, &stale).contains("(stale)"),
            "{}",
            ownership_line(Lang::En, &stale)
        );
        // The Chinese line reads as one sentence, stale mark included.
        assert_eq!(
            ownership_line(Lang::Zh, &status(3, Some(120))),
            "代次 3，120 毫秒前已确认"
        );
        assert!(
            ownership_line(Lang::Zh, &stale).ends_with("（已过期）"),
            "{}",
            ownership_line(Lang::Zh, &stale)
        );
    }

    #[test]
    fn acked_buttons_render_as_sorted_names() {
        assert_eq!(acked_buttons_line(Lang::En, &[]), "None");
        let home = RemoteButton::Home.usage();
        let back = RemoteButton::Back.usage();
        assert_eq!(acked_buttons_line(Lang::En, &[home, back]), "back, home");
        assert_eq!(
            acked_buttons_line(Lang::En, &[0x1234]),
            "1 usages (unrecognized mix)"
        );
        assert_eq!(acked_buttons_line(Lang::Zh, &[]), "无");
        assert_eq!(
            acked_buttons_line(Lang::Zh, &[0x1234]),
            "1 个用法（无法识别的组合）"
        );
    }

    #[test]
    fn counters_render_on_one_line() {
        let c = BridgeCounters {
            edges: 12,
            pings: 3,
            ..Default::default()
        };
        let line = counters_line(Lang::En, &c);
        assert!(line.contains("Edges 12"));
        assert!(line.contains("Pings 3"));
        assert!(line.contains("Denies 0"));
        let zh = counters_line(Lang::Zh, &c);
        assert!(zh.contains("边沿 12"));
        assert!(zh.contains("心跳 3"));
        assert!(zh.contains("拒绝 0"));
    }
}
