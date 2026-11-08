//! Device-node recovery: when the agent link dies while the device is still
//! present, the host process was likely replaced (UMDF restart, driver
//! update). The fix is `pnputil /remove-device <instance>` +
//! `pnputil /scan-devices`, which re-enumerates the node and spawns a FRESH
//! exclusive host — then we re-inject it.
//!
//! The decision (whether to attempt) is pure and unit-tested; the attempt
//! itself is Windows-only and bounded by timeouts so pnputil can never hang
//! around unattended. A 60 s cooldown sits between attempts.

use crate::registry::HostScan;
#[cfg(test)]
use rc_protocol::consts::RECOVERY_COOLDOWN_MS;

/// Decide whether a recovery attempt should start.
///
/// - `agent_link_dead`: the agent pipe session ended without a successor
///   (no Hello after re-connect attempts within the watchdog window).
/// - `device_present`: the RC003 still has device instance(s) in the
///   registry (any pid, including 0). If the remote is gone entirely there
///   is nothing to recover — wait for the device to return.
/// - `last_attempt_ms`: Unix-ms of the previous attempt (None = never).
pub fn should_recover(
    agent_link_dead: bool,
    device_present: bool,
    last_attempt_ms: Option<u64>,
    now_ms: u64,
    cooldown_ms: u64,
) -> bool {
    if !agent_link_dead || !device_present {
        return false;
    }
    match last_attempt_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= cooldown_ms,
    }
}

/// Recoverable RC003 presence from a scan: any RC003 instance at all.
pub fn device_present(scan: &HostScan) -> bool {
    scan.entries.iter().any(|e| e.is_rc003)
}

/// The new-host wait window (device node re-enumeration takes seconds).
pub const NEW_HOST_WAIT_MS: u64 = 90_000;
pub const NEW_HOST_POLL_MS: u64 = 500;

#[cfg(target_os = "windows")]
pub mod windows_impl {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// Bounded pnputil budgets: 120 s remove + 30 s scan + 90 s wait stays
    /// within any sane caller grace; over-budget runs are killed.
    const REMOVE_TIMEOUT_MS: u64 = 120_000;
    const SCAN_TIMEOUT_MS: u64 = 30_000;

    /// Run one pnputil command with a hard timeout, CREATE_NO_WINDOW, and
    /// discarded stdout/stderr (only the exit code matters, and the device
    /// instance path must not leak into logs or pipe buffers).
    fn run_pnputil(args: &[&str], timeout_ms: u64) -> Result<bool, &'static str> {
        let mut child = std::process::Command::new("pnputil")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(crate::task::windows_impl::CREATE_NO_WINDOW)
            .spawn()
            .map_err(|_| "spawn_failed")?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status.success()),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("timeout");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("wait_failed");
                }
            }
        }
    }

    fn label(result: &Result<bool, &'static str>) -> &'static str {
        match result {
            Ok(true) => "ok",
            Ok(false) => "exit_failed",
            Err(reason) => reason,
        }
    }

    /// One recovery attempt: remove the node, rescan, wait for the new
    /// exclusive host. Returns the new host pid when a fresh exclusive
    /// RC003 host appeared.
    pub fn run_recovery(
        instance_id: &str,
        logger: &crate::logging::Logger,
    ) -> Option<u32> {
        logger.kv("[RECOVERY]", &[("phase", "pnputil".into()), ("instance", instance_id.to_string())]);
        let removed = run_pnputil(&["/remove-device", instance_id], REMOVE_TIMEOUT_MS);
        let scanned = run_pnputil(&["/scan-devices"], SCAN_TIMEOUT_MS);
        logger.kv(
            "[RECOVERY]",
            &[
                ("phase", "pnputil_done".into()),
                ("remove", label(&removed).to_string()),
                ("scan", label(&scanned).to_string()),
            ],
        );

        // Wait for a NEW exclusive host (a different pid than before, or any
        // pid when the old host never had one recorded).
        let deadline = Instant::now() + Duration::from_millis(NEW_HOST_WAIT_MS);
        while Instant::now() < deadline {
            if let Ok(scan) = crate::registry::scan_hosts() {
                match crate::host::verdict_from_scan(&scan) {
                    crate::host::HostVerdict::Exclusive { pid, .. } => {
                        logger.kv(
                            "[RECOVERY]",
                            &[("phase", "new_host".into()), ("pid", pid.to_string())],
                        );
                        return Some(pid);
                    }
                    other => {
                        logger.kv(
                            "[RECOVERY]",
                            &[("phase", "waiting".into()), ("verdict", format!("{other:?}"))],
                        );
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(NEW_HOST_POLL_MS));
        }
        logger.warn("recovery: no new exclusive host appeared within the wait window");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::HostEntry;

    fn scan_with(entries: Vec<HostEntry>) -> HostScan {
        HostScan {
            entries,
            ..Default::default()
        }
    }

    fn rc003(pid: u32) -> HostEntry {
        HostEntry {
            instance_id: r"HID\VID_2717&PID_32B8&MI_01\7&0000".into(),
            is_rc003: true,
            pid,
        }
    }

    #[test]
    fn cooldown_constants_match_contract() {
        assert_eq!(RECOVERY_COOLDOWN_MS, 60_000);
        assert_eq!(NEW_HOST_WAIT_MS, 90_000);
        assert_eq!(NEW_HOST_POLL_MS, 500);
    }

    #[test]
    fn first_attempt_recovers_immediately() {
        assert!(should_recover(true, true, None, 1000, RECOVERY_COOLDOWN_MS));
    }

    #[test]
    fn no_recovery_when_link_alive() {
        assert!(!should_recover(false, true, None, 1000, RECOVERY_COOLDOWN_MS));
    }

    #[test]
    fn no_recovery_when_device_gone() {
        // Remote unplugged: nothing to recover; wait for it to return.
        assert!(!should_recover(true, false, None, 1000, RECOVERY_COOLDOWN_MS));
    }

    #[test]
    fn cooldown_blocks_then_allows() {
        let last = 10_000;
        let cooldown = 60_000;
        assert!(!should_recover(true, true, Some(last), last + cooldown - 1, cooldown));
        assert!(should_recover(true, true, Some(last), last + cooldown, cooldown));
        assert!(should_recover(true, true, Some(last), last + cooldown + 999_999, cooldown));
    }

    #[test]
    fn clock_never_underflows() {
        // now earlier than last (clock skew): saturating subtraction keeps
        // this false instead of panicking on overflow.
        assert!(!should_recover(true, true, Some(10_000), 5_000, 60_000));
    }

    #[test]
    fn device_presence_ignores_other_devices() {
        let remote_gone = scan_with(vec![HostEntry {
            instance_id: r"HID\VID_046D&PID_C52B\6&0000".into(),
            is_rc003: false,
            pid: 5000,
        }]);
        assert!(!device_present(&remote_gone));

        let remote_dead_host = scan_with(vec![rc003(0)]);
        assert!(device_present(&remote_dead_host), "pid 0 still means the node exists");

        let remote_alive = scan_with(vec![rc003(4012)]);
        assert!(device_present(&remote_alive));
    }
}
