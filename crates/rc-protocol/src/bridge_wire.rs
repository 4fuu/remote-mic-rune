//! ASCII line protocol between the elevated helper (client) and the app
//! (server) over [`crate::consts::BRIDGE_PIPE_NAME`]. One command per
//! `\n`-terminated line; usages are lowercase hex without prefix, comma
//! separated; `-` means the empty set.
//!
//! helper -> app:
//!   `HELLO <version> <token> <pid>`
//!   `E <unix_ms> <usages|->`        absolute pressed set; `-` = all released
//!   `P`                             ping (every 1s)
//!   `R <0|1>`                       device-node recovery in progress
//!   `O <generation> <usages|->`     agent acked owning this generation
//!   `A <hex|->`                     synthesis applied / off (absolute)
//!   `F <ok|skip> <reason>`          one synthesized voice frame result
//!   `BYE <reason>`
//! app -> helper:
//!   `OK <version> <generation> <usages|->`  auth ok + initial targets
//!   `T <generation> <usages|->`             retarget (absolute)
//!   `S <hex4|->`                            synthesis on/off (absolute)
//!   `W 1`                                   gate capability declaration
//!   `DENY <reason>`

use crate::buttons::RemoteButton;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeLine {
    Hello { version: u32, token: String, helper_pid: u32 },
    /// Absolute-state edges; empty vec = everything released.
    Edges { usages: Vec<u16> },
    Ping,
    Recovering { active: bool },
    Ownership { generation: u64, usages: Vec<u16> },
    SynthAck { to: Option<u16> },
    VoiceFrame { replaced: bool, reason: String },
    Ok { version: u32, generation: u64, usages: Vec<u16> },
    Targets { generation: u64, usages: Vec<u16> },
    Synth { to: Option<u16> },
    GateDeclare,
    Deny { reason: String },
    Bye { reason: String },
}

/// Parse one line (with or without trailing newline). Empty lines -> None.
pub fn parse_bridge_line(line: &str) -> Result<Option<BridgeLine>, String> {
    let line = line.trim_end_matches(['\r', '\n']).trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split(' ');
    let head = parts.next().unwrap_or_default();
    let usage_list = |payload: &str| -> Result<Vec<u16>, String> {
        parse_usage_payload(payload)
    };
    match head {
        "HELLO" => {
            let version = parse_num::<u32>(parts.next(), "HELLO version")?;
            let token = parts.next().ok_or("HELLO missing token")?.to_string();
            if token.is_empty() {
                return Err("HELLO empty token".into());
            }
            let helper_pid = parse_num::<u32>(parts.next(), "HELLO pid")?;
            Ok(Some(BridgeLine::Hello { version, token, helper_pid }))
        }
        "E" => {
            parse_num::<u64>(parts.next(), "E timestamp")?;
            Ok(Some(BridgeLine::Edges { usages: usage_list(parts.next().unwrap_or("-"))? }))
        }
        "P" => Ok(Some(BridgeLine::Ping)),
        "R" => match parts.next() {
            Some("1") | Some("on") => Ok(Some(BridgeLine::Recovering { active: true })),
            Some("0") | Some("off") => Ok(Some(BridgeLine::Recovering { active: false })),
            other => Err(format!("bad R state {other:?}")),
        },
        "O" => {
            let generation = parse_num::<u64>(parts.next(), "O generation")?;
            Ok(Some(BridgeLine::Ownership {
                generation,
                usages: usage_list(parts.next().unwrap_or("-"))?,
            }))
        }
        "A" => Ok(Some(BridgeLine::SynthAck { to: parse_opt_usage(parts.next().unwrap_or("-"))? })),
        "F" => {
            let replaced = match parts.next() {
                Some("ok") => true,
                Some("skip") => false,
                other => return Err(format!("bad F result {other:?}")),
            };
            let reason: String = parts.next().unwrap_or("unknown").chars().take(32).collect();
            Ok(Some(BridgeLine::VoiceFrame { replaced, reason }))
        }
        "OK" => {
            let version = parse_num::<u32>(parts.next(), "OK version")?;
            let generation = parse_num::<u64>(parts.next(), "OK generation")?;
            Ok(Some(BridgeLine::Ok {
                version,
                generation,
                usages: usage_list(parts.next().unwrap_or("-"))?,
            }))
        }
        "T" => {
            let generation = parse_num::<u64>(parts.next(), "T generation")?;
            Ok(Some(BridgeLine::Targets {
                generation,
                usages: usage_list(parts.next().unwrap_or("-"))?,
            }))
        }
        "S" => Ok(Some(BridgeLine::Synth { to: parse_opt_usage(parts.next().unwrap_or("-"))? })),
        "W" => {
            let cap = parse_num::<u32>(parts.next(), "W capability")?;
            if cap == 1 {
                Ok(Some(BridgeLine::GateDeclare))
            } else {
                Err(format!("unknown W capability {cap}"))
            }
        }
        "DENY" => Ok(Some(BridgeLine::Deny {
            reason: parts.collect::<Vec<_>>().join(" "),
        })),
        "BYE" => Ok(Some(BridgeLine::Bye {
            reason: parts.collect::<Vec<_>>().join(" "),
        })),
        other => Err(format!("unknown command {other:?}")),
    }
}

fn parse_num<T: std::str::FromStr>(part: Option<&str>, what: &str) -> Result<T, String> {
    part.and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("{what} is not an integer"))
}

fn parse_opt_usage(payload: &str) -> Result<Option<u16>, String> {
    if payload == "-" {
        return Ok(None);
    }
    let usage = u16::from_str_radix(payload.trim_start_matches("0x"), 16)
        .map_err(|_| format!("{payload:?} is not a hex usage"))?;
    Ok((usage != 0).then_some(usage))
}

fn parse_usage_payload(payload: &str) -> Result<Vec<u16>, String> {
    if payload == "-" {
        return Ok(Vec::new());
    }
    let mut usages = Vec::new();
    for raw in payload.split(',') {
        let token = raw.trim().trim_start_matches("0x");
        if token.is_empty() {
            continue;
        }
        let usage = u16::from_str_radix(token, 16)
            .map_err(|_| format!("{token:?} is not a hex usage"))?;
        if !usages.contains(&usage) {
            usages.push(usage);
        }
    }
    Ok(usages)
}

fn format_usage_payload(usages: &[u16]) -> String {
    if usages.is_empty() {
        return "-".to_string();
    }
    usages.iter().map(|u| format!("{u:02x}")).collect::<Vec<_>>().join(",")
}

// -- encode side (both directions) -----------------------------------------

pub fn format_hello(version: u32, token: &str, pid: u32) -> String {
    format!("HELLO {version} {token} {pid}")
}

pub fn format_edges(unix_ms: u64, usages: &[u16]) -> String {
    format!("E {unix_ms} {}", format_usage_payload(usages))
}

pub fn format_ping() -> String {
    "P".to_string()
}

pub fn format_recovering(active: bool) -> String {
    format!("R {}", if active { 1 } else { 0 })
}

pub fn format_ownership(generation: u64, usages: &[u16]) -> String {
    format!("O {generation} {}", format_usage_payload(usages))
}

pub fn format_synth_ack(to: Option<u16>) -> String {
    match to {
        Some(u) => format!("A {u:04X}"),
        None => "A -".to_string(),
    }
}

pub fn format_voice_frame(replaced: bool, reason: &str) -> String {
    format!("F {} {reason}", if replaced { "ok" } else { "skip" })
}

pub fn format_bye(reason: &str) -> String {
    format!("BYE {reason}")
}

pub fn format_ok(version: u32, generation: u64, usages: &[u16]) -> String {
    format!("OK {version} {generation} {}", format_usage_payload(usages))
}

pub fn format_targets(generation: u64, usages: &[u16]) -> String {
    format!("T {generation} {}", format_usage_payload(usages))
}

pub fn format_synth(to: Option<u16>) -> String {
    match to {
        Some(u) => format!("S {u:04X}"),
        None => "S -".to_string(),
    }
}

pub fn format_gate_declare() -> String {
    "W 1".to_string()
}

pub fn format_deny(reason: &str) -> String {
    format!("DENY {reason}")
}

/// Cross-check used by both endpoints: every usage in a T/OK line must be a
/// known button usage before it is forwarded to the agent.
pub fn all_known_buttons(usages: &[u16]) -> bool {
    usages.iter().all(|u| RemoteButton::from_usage(*u).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trip() {
        let line = format_hello(1, "deadbeef", 1234);
        assert_eq!(
            parse_bridge_line(&line).unwrap(),
            Some(BridgeLine::Hello { version: 1, token: "deadbeef".into(), helper_pid: 1234 })
        );
    }

    #[test]
    fn edges_use_lowercase_hex_and_dash_for_empty() {
        assert_eq!(format_edges(9, &[0x00F1, 0x0080, 0x0081]), "E 9 f1,80,81");
        assert_eq!(format_edges(9, &[]), "E 9 -");
        assert_eq!(
            parse_bridge_line("E 9 f1,80,81").unwrap(),
            Some(BridgeLine::Edges { usages: vec![0x00F1, 0x0080, 0x0081] })
        );
        assert_eq!(
            parse_bridge_line("E 9 -").unwrap(),
            Some(BridgeLine::Edges { usages: vec![] })
        );
    }

    #[test]
    fn ownership_and_targets_round_trip() {
        let line = format_ownership(7, &[0x004A]);
        assert_eq!(
            parse_bridge_line(&line).unwrap(),
            Some(BridgeLine::Ownership { generation: 7, usages: vec![0x004A] })
        );
        let line = format_targets(8, &[]);
        assert_eq!(
            parse_bridge_line(&line).unwrap(),
            Some(BridgeLine::Targets { generation: 8, usages: vec![] })
        );
    }

    #[test]
    fn synth_and_ack_forms() {
        assert_eq!(format_synth(Some(0x00E6)), "S 00E6");
        assert_eq!(format_synth(None), "S -");
        assert_eq!(format_synth_ack(Some(0x00E6)), "A 00E6");
        assert_eq!(parse_bridge_line("S -").unwrap(), Some(BridgeLine::Synth { to: None }));
        assert_eq!(
            parse_bridge_line("A 00E6").unwrap(),
            Some(BridgeLine::SynthAck { to: Some(0x00E6) })
        );
    }

    #[test]
    fn ok_line_round_trip() {
        let line = format_ok(1, 3, &[0x00F1, 0x0080, 0x0081]);
        assert_eq!(
            parse_bridge_line(&line).unwrap(),
            Some(BridgeLine::Ok { version: 1, generation: 3, usages: vec![0x00F1, 0x0080, 0x0081] })
        );
    }

    #[test]
    fn voice_frame_and_bye() {
        assert_eq!(
            parse_bridge_line("F skip lease").unwrap(),
            Some(BridgeLine::VoiceFrame { replaced: false, reason: "lease".into() })
        );
        assert_eq!(parse_bridge_line("BYE duration").unwrap(), Some(BridgeLine::Bye { reason: "duration".into() }));
    }

    #[test]
    fn empty_line_is_none_and_unknown_is_err() {
        assert_eq!(parse_bridge_line("  \r\n").unwrap(), None);
        assert!(parse_bridge_line("X 1").is_err());
        assert!(parse_bridge_line("HELLO 1").is_err()); // missing fields
    }

    #[test]
    fn known_button_check() {
        assert!(all_known_buttons(&[0x00F1, 0x0081]));
        assert!(!all_known_buttons(&[0x003E])); // voice key is not a button
        assert!(all_known_buttons(&[]));
    }
}
