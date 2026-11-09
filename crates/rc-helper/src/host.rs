//! Host selection: turn a registry scan into an injection target verdict.
//!
//! Exclusive-host criterion:
//! a pid hosts exactly one WUDF-diagnostic member node AND that node is the
//! RC003 (`members == 1 && rc003_members == 1`). Shared hosts are refused by
//! default because killing or injecting them takes down the user's other BLE
//! devices. `--force` / `--target-pid` are documented overrides for the case
//! where the user knows which remote is theirs.

use crate::registry::{is_rc003_entry, HostEntry, HostScan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerdict {
    /// One pid, exactly one member node, and it is the RC003. Safe target.
    Exclusive { pid: u32, instance_id: String },
    /// A candidate pid exists but shares the host with other device nodes.
    /// Refused without `--force`.
    Shared {
        pid: u32,
        members: usize,
        rc003_members: usize,
        instance_id: String,
    },
    /// No device instance with a live (non-zero) RC003 host pid.
    NoHost,
    /// Two or more distinct live RC003 host pids: auto-picking one risks
    /// injecting the wrong remote's host. Refused without `--force`.
    Ambiguous { pids: Vec<u32> },
}

impl HostVerdict {
    /// The pid to inject into, if the verdict names exactly one. Ambiguous
    /// names the lowest pid deterministically (scan order is not stable).
    pub fn pid(&self) -> Option<u32> {
        match self {
            HostVerdict::Exclusive { pid, .. } => Some(*pid),
            HostVerdict::Shared { pid, .. } => Some(*pid),
            HostVerdict::Ambiguous { pids } => pids.first().copied(),
            HostVerdict::NoHost => None,
        }
    }
}

/// Pure verdict over scan entries. `entries` may contain RC003 nodes with
/// pid 0 (host not running) and other vendors' nodes; only live RC003 hosts
/// are candidates. Entries are deduplicated by instance id first: the same
/// registry node appearing twice (rescan overlap) must not inflate the
/// member counts and flip a real exclusive host to "shared".
pub fn verdict_from_entries(entries: &[HostEntry]) -> HostVerdict {
    let mut unique: Vec<&HostEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if !unique.iter().any(|e| e.instance_id == entry.instance_id) {
            unique.push(entry);
        }
    }
    let mut candidate_pids: Vec<u32> = unique
        .iter()
        .filter(|e| e.is_rc003 && e.pid != 0)
        .map(|e| e.pid)
        .collect();
    candidate_pids.sort_unstable();
    candidate_pids.dedup();

    match candidate_pids.len() {
        0 => HostVerdict::NoHost,
        1 => {
            let pid = candidate_pids[0];
            let members: Vec<&HostEntry> =
                unique.iter().filter(|e| e.pid == pid).copied().collect();
            let rc003_members = members.iter().filter(|e| is_rc003_entry(e)).count();
            let rc003_instance = members
                .iter()
                .find(|e| is_rc003_entry(e))
                .map(|e| e.instance_id.clone())
                .unwrap_or_default();
            if members.len() == 1 && rc003_members == 1 {
                HostVerdict::Exclusive {
                    pid,
                    instance_id: rc003_instance,
                }
            } else {
                HostVerdict::Shared {
                    pid,
                    members: members.len(),
                    rc003_members,
                    instance_id: rc003_instance,
                }
            }
        }
        _ => HostVerdict::Ambiguous {
            pids: candidate_pids,
        },
    }
}

/// Verdict over a whole scan (convenience wrapper keeping the call sites
/// free of field plumbing).
pub fn verdict_from_scan(scan: &HostScan) -> HostVerdict {
    verdict_from_entries(&scan.entries)
}

/// Apply `--force` / `--target-pid` overrides to a verdict, producing either
/// the pid to use or the refusal exit path. Overrides are explicit user
/// intent and skip the shared/ambiguous refusals but never invent a target:
/// `--target-pid` must be one of the pids seen in the scan (or any pid when
/// no scan entries exist at all, e.g. self-test rigs).
pub fn apply_overrides(
    verdict: &HostVerdict,
    force: bool,
    target_pid: Option<u32>,
) -> Result<Option<u32>, HostVerdict> {
    if let Some(pid) = target_pid {
        return Ok(Some(pid));
    }
    match verdict {
        HostVerdict::Exclusive { pid, .. } => Ok(Some(*pid)),
        HostVerdict::Shared { .. } if force => {
            verdict.pid().map(Some).ok_or_else(|| verdict.clone())
        }
        HostVerdict::Ambiguous { .. } if force => {
            verdict.pid().map(Some).ok_or_else(|| verdict.clone())
        }
        HostVerdict::NoHost => Ok(None),
        _ => Err(verdict.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc003(instance: &str, pid: u32) -> HostEntry {
        HostEntry {
            instance_id: format!(r"HID\{instance}"),
            is_rc003: true,
            pid,
        }
    }

    fn other(instance: &str, pid: u32) -> HostEntry {
        HostEntry {
            instance_id: format!(r"HID\{instance}"),
            is_rc003: false,
            pid,
        }
    }

    #[test]
    fn exclusive_host_detected() {
        let entries = [rc003(r"VID_2717&PID_32B8&MI_01\7&2d5f3a4b&0&0000", 4012)];
        assert_eq!(
            verdict_from_entries(&entries),
            HostVerdict::Exclusive {
                pid: 4012,
                instance_id: entries[0].instance_id.clone(),
            }
        );
    }

    #[test]
    fn ble_gatt_instance_alone_in_host_is_exclusive() {
        // The BLE-paired form seen on real Win11 hardware: one
        // BTHLEDEVICE HID service collection, no USB interfaces at all.
        let entries = [HostEntry {
            instance_id: r"BTHLEDEVICE\{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&012717_PID&32b8_REV&00a4_c05d39c3274f\b&20a7291a&1&0055".into(),
            is_rc003: true,
            pid: 2672,
        }];
        assert_eq!(
            verdict_from_entries(&entries),
            HostVerdict::Exclusive {
                pid: 2672,
                instance_id: entries[0].instance_id.clone(),
            }
        );
    }

    #[test]
    fn shared_host_refused_when_other_device_hosts_alongside() {
        // The user's BLE keyboard rides in the same WUDFHost: members == 2.
        let entries = [
            rc003(r"VID_2717&PID_32B8&MI_01\7&0000", 4012),
            other(r"VID_046D&PID_C52B&MI_02\6&0000", 4012),
        ];
        match verdict_from_entries(&entries) {
            HostVerdict::Shared {
                pid,
                members,
                rc003_members,
                ..
            } => {
                assert_eq!(pid, 4012);
                assert_eq!(members, 2);
                assert_eq!(rc003_members, 1);
            }
            other => panic!("expected shared, got {other:?}"),
        }
    }

    #[test]
    fn two_rc003_nodes_in_one_host_still_counts_shared() {
        // Both interface collections of one remote landing in the same host:
        // members == 2 fails the exclusive criterion even though both are ours.
        let entries = [
            rc003(r"VID_2717&PID_32B8&MI_00\7&0000", 4012),
            rc003(r"VID_2717&PID_32B8&MI_01\7&0000", 4012),
        ];
        match verdict_from_entries(&entries) {
            HostVerdict::Shared {
                members,
                rc003_members,
                ..
            } => {
                assert_eq!(members, 2);
                assert_eq!(rc003_members, 2);
            }
            other => panic!("expected shared, got {other:?}"),
        }
    }

    #[test]
    fn no_host_when_all_pids_zero() {
        let entries = [rc003(r"VID_2717&PID_32B8&MI_01\7&0000", 0)];
        assert_eq!(verdict_from_entries(&entries), HostVerdict::NoHost);
    }

    #[test]
    fn no_host_when_rc003_absent() {
        // Another BLE device is present with a live host, the remote is not.
        let entries = [other(r"VID_046D&PID_C52B&MI_02\6&0000", 5000)];
        assert_eq!(verdict_from_entries(&entries), HostVerdict::NoHost);
    }

    #[test]
    fn no_host_on_empty_scan() {
        assert_eq!(verdict_from_entries(&[]), HostVerdict::NoHost);
    }

    #[test]
    fn rc003_dead_host_with_other_live_device_is_still_no_host() {
        let entries = [
            rc003(r"VID_2717&PID_32B8&MI_01\7&0000", 0),
            other(r"VID_046D&PID_C52B&MI_02\6&0000", 5000),
        ];
        assert_eq!(verdict_from_entries(&entries), HostVerdict::NoHost);
    }

    #[test]
    fn two_remotes_are_ambiguous() {
        let entries = [
            rc003(r"VID_2717&PID_32B8&MI_01\7&0001", 4012),
            rc003(r"VID_2717&PID_32B8&MI_01\7&0002", 4099),
        ];
        match verdict_from_entries(&entries) {
            HostVerdict::Ambiguous { pids } => assert_eq!(pids, vec![4012, 4099]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_scan_entries_do_not_create_ambiguity() {
        // The same instance seen twice (rescan overlap) must not look like
        // two remotes.
        let entries = [
            rc003(r"VID_2717&PID_32B8&MI_01\7&0001", 4012),
            rc003(r"VID_2717&PID_32B8&MI_01\7&0001", 4012),
        ];
        assert_eq!(
            verdict_from_entries(&entries),
            HostVerdict::Exclusive {
                pid: 4012,
                instance_id: entries[0].instance_id.clone(),
            }
        );
    }

    #[test]
    fn force_and_target_pid_overrides() {
        let shared = verdict_from_entries(&[
            rc003(r"VID_2717&PID_32B8&MI_01\7&0000", 4012),
            other(r"VID_046D&PID_C52B&MI_02\6&0000", 4012),
        ]);
        // Without --force a shared host is refused.
        assert!(apply_overrides(&shared, false, None).is_err());
        // With --force it proceeds with the detected pid.
        assert_eq!(apply_overrides(&shared, true, None).unwrap(), Some(4012));

        let ambiguous = HostVerdict::Ambiguous { pids: vec![11, 22] };
        assert!(apply_overrides(&ambiguous, false, None).is_err());
        assert_eq!(apply_overrides(&ambiguous, true, None).unwrap(), Some(11));
        // --target-pid overrides everything, even NoHost.
        assert_eq!(
            apply_overrides(&HostVerdict::NoHost, false, Some(7)).unwrap(),
            Some(7)
        );
        assert_eq!(
            apply_overrides(
                &HostVerdict::Exclusive {
                    pid: 9,
                    instance_id: String::new()
                },
                false,
                Some(7)
            )
            .unwrap(),
            Some(7)
        );
        // Exclusive never needs force.
        assert_eq!(
            apply_overrides(
                &HostVerdict::Exclusive {
                    pid: 9,
                    instance_id: String::new()
                },
                false,
                None
            )
            .unwrap(),
            Some(9)
        );
        // NoHost without a target stays None (caller exits with NO_HOST).
        assert_eq!(
            apply_overrides(&HostVerdict::NoHost, false, None).unwrap(),
            None
        );
    }
}
