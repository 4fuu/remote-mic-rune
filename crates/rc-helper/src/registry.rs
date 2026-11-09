//! Registry scan: find every device instance under the PnP `Enum` tree
//! whose `Device Parameters\WUDFDiagnosticInfo` carries a `HostPid` value.
//!
//! Why a FULL walk and not an HID-only or RC003-only search:
//! - A BLE-paired RC003 never appears under `Enum\HID`. Windows enumerates
//!   its HID-over-GATT service under `Enum\BTHLEDEVICE\{00001812-...}` with
//!   ids encoded `VID&012717_PID&32B8`; an HID-only walk reports "no host"
//!   on exactly the machine the product targets.
//! - The exclusive-host verdict needs to know which OTHER devices share the
//!   same WUDFHost — killing or injecting a shared host takes the user's
//!   other UMDF-hosted devices down with it. Only the whole tree gives the
//!   true member count.
//!
//! The `HostPid` read is type- and width-aware (query type + required size
//! first, then read by the actual width). On current Windows the value is a
//! REG_QWORD (8 bytes); a reader hardcoded to 4 bytes fails with
//! ERROR_MORE_DATA (234) and — if that failure is swallowed — produces a
//! "0 instances" scan that masquerades as "device not connected". That
//! false negative is indistinguishable from a disconnected device unless
//! every failure mode is kept distinct, so both widths (and REG_BINARY with
//! matching width) are decoded here and every failure mode is counted.

use rc_protocol::consts::{DEVICE_PID, DEVICE_PID_BT, DEVICE_VID, DEVICE_VID_BT};

/// One WUDF-diagnostic member node: a device instance with a HostPid value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Full device instance id as PnP knows it, e.g.
    /// `HID\VID_2717&PID_32B8&MI_01\7&2d5f3a&0&0000` (USB form) or
    /// `BTHLEDEVICE\{00001812-...}_Dev_VID&012717_PID&32b8_REV&00a4_...`
    /// (BLE HID-over-GATT form). `pnputil /remove-device` accepts exactly
    /// this form.
    pub instance_id: String,
    /// The instance id identifies the RC003 remote (USB or Bluetooth id
    /// encoding).
    pub is_rc003: bool,
    /// WUDFHost.exe pid hosting the device; 0 = host not running.
    pub pid: u32,
}

/// Full-scan result with diagnostic counters. Without these, "0 entries"
/// cannot be attributed to anything — a bare "device not connected" report
/// hides exactly that failure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostScan {
    pub entries: Vec<HostEntry>,
    /// Device instance keys visited during the `Enum` tree walk.
    pub nodes_seen: usize,
    /// `Device Parameters\WUDFDiagnosticInfo` keys successfully opened
    /// (counted regardless of whether HostPid was then readable).
    pub diag_keys_opened: usize,
    /// Nodes whose HostPid decoded to 0 (host not running).
    pub no_host_count: usize,
    /// Nodes where the read failed; (instance id, sanitized reason).
    pub read_failures: Vec<(String, String)>,
}

/// Registry value types (subset used here).
pub const REG_NONE: u32 = 0;
pub const REG_SZ: u32 = 1;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;
pub const REG_QWORD: u32 = 11;

/// Upper bound for one registry value read; HostPid is 8 bytes in practice.
pub const MAX_VALUE_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidDecode {
    /// Decoded, non-zero pid (already sanity-bounded to u32).
    Pid(u32),
    /// Value present and zero: the device's UMDF host is not running.
    Zero,
    /// Width/type mismatch: type is not DWORD/QWORD/BINARY, or the byte
    /// count does not match the type, or the value exceeds a pid range.
    BadSize,
    /// Unsupported registry value type.
    BadType,
}

/// Decode `HostPid` from raw bytes. Little-endian on both widths; REG_BINARY
/// is accepted at 4 or 8 bytes because some Windows builds store it that way.
/// Pids are bounded to u32: a wider value is malformed, not a big pid.
pub fn decode_host_pid(value_type: u32, raw: &[u8]) -> PidDecode {
    let le = |raw: &[u8], n: usize| -> Option<u64> {
        if raw.len() < n {
            return None;
        }
        let mut v: u64 = 0;
        for (i, b) in raw[..n].iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        Some(v)
    };
    let decoded = match value_type {
        REG_DWORD => le(raw, 4),
        REG_QWORD => le(raw, 8),
        REG_BINARY => match raw.len() {
            4 => le(raw, 4),
            8 => le(raw, 8),
            _ => None,
        },
        // String-typed HostPid is never valid; named explicitly so the
        // constant documents the type set this reader understands.
        REG_SZ => return PidDecode::BadType,
        _ => return PidDecode::BadType,
    };
    match decoded {
        None => PidDecode::BadSize,
        Some(0) => PidDecode::Zero,
        Some(v) if v <= u32::MAX as u64 => PidDecode::Pid(v as u32),
        Some(_) => PidDecode::BadSize,
    }
}

/// True when a registry key path under `Enum` belongs to an RC003 instance.
/// Two encodings exist for the same hardware id pair:
/// - USB/HID form: contains `VID_2717` and `PID_32B8` (underscore ids).
/// - Bluetooth LE form: contains `VID&012717_PID&32B8` — the GATT device id
///   with the `01` Bluetooth-SIG vendor source prefix baked in.
/// Matched case-insensitively: the registry is case-preserving but not
/// case-enforcing (BLE instances arrive lowercase in practice).
pub fn is_rc003_key_path(key_path: &str) -> bool {
    let path = key_path.to_ascii_uppercase();
    let usb = path.contains(DEVICE_VID) && path.contains(DEVICE_PID);
    let ble = path.contains(DEVICE_VID_BT) && path.contains(DEVICE_PID_BT);
    usb || ble
}

/// The device instance id for a key path relative to
/// `SYSTEM\CurrentControlSet\Enum`. The walk runs from the `Enum` root, so
/// the relative path already carries its real enumerator prefix
/// (`HID\...`, `BTHLEDEVICE\...`) — PnP and `pnputil` accept it verbatim.
pub fn instance_id_for_key_path(key_path: &str) -> String {
    key_path.to_string()
}

pub fn is_rc003_entry(entry: &HostEntry) -> bool {
    entry.is_rc003 && is_rc003_key_path(&entry.instance_id)
}

// ---------------------------------------------------------------------------
// Windows walk (cfg-gated; pure helpers above are host-testable)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use rc_protocol::consts::{HOSTPID_VALUE, WUDF_DIAG_SUFFIX};
    use std::string::String;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, WIN32_ERROR,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_READ, REG_VALUE_TYPE,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn trim_wide(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn open_key(parent: HKEY, sub: &str) -> Result<HKEY, u32> {
        let path = wide(sub);
        let mut key = HKEY::default();
        let rc = unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key) };
        if rc == ERROR_SUCCESS {
            Ok(key)
        } else {
            Err(rc.0)
        }
    }

    fn enum_subkeys(key: HKEY) -> Vec<String> {
        let mut out = Vec::new();
        for index in 0..u32::MAX {
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            let rc: WIN32_ERROR = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    Some(PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None::<*mut FILETIME>,
                )
            };
            if rc == ERROR_SUCCESS {
                out.push(trim_wide(&name_buf[..name_len as usize]));
            } else {
                // ERROR_NO_MORE_ITEMS is the normal end; anything else
                // (including a corrupted key reporting FILE_NOT_FOUND
                // mid-enumeration) stops the loop rather than spinning.
                break;
            }
        }
        out
    }

    /// Type/width-aware HostPid read, mirroring the pure decoder in this
    /// module. Two steps are mandatory: with a single call the width has to
    /// be guessed first, and guessing the width is precisely the bug that
    /// shipped a false "device not connected" once already.
    fn read_host_pid(diag_key: HKEY) -> PidDecode {
        let name = wide(HOSTPID_VALUE);
        let mut value_type = REG_VALUE_TYPE(REG_NONE);
        let mut needed: u32 = 0;
        let query = unsafe {
            RegQueryValueExW(
                diag_key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut needed),
            )
        };
        // With a null data buffer the call returns either ERROR_SUCCESS (size
        // in `needed`) or ERROR_MORE_DATA, depending on Windows build; both
        // outcomes give us the width. ERROR_FILE_NOT_FOUND means no value.
        if query != ERROR_SUCCESS && query != ERROR_MORE_DATA {
            if query == ERROR_FILE_NOT_FOUND {
                return PidDecode::BadSize; // no value: treated as missing
            }
            return PidDecode::BadSize;
        }
        if needed == 0 || needed as usize > MAX_VALUE_BYTES {
            return PidDecode::BadSize;
        }
        let mut buf = [0u8; MAX_VALUE_BYTES];
        let mut len = needed;
        let mut actual_type = REG_VALUE_TYPE(REG_NONE);
        let read = unsafe {
            RegQueryValueExW(
                diag_key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut actual_type),
                Some(buf.as_mut_ptr()),
                Some(&mut len),
            )
        };
        if read != ERROR_SUCCESS {
            return PidDecode::BadSize;
        }
        let len = (len as usize).min(MAX_VALUE_BYTES);
        decode_host_pid(actual_type.0, &buf[..len])
    }

    /// Full walk of the PnP enum tree (HID, BTHLEDEVICE, and every other
    /// enumerator); the module keeps the pure u32 REG_* consts (the
    /// windows-rs REG_VALUE_TYPE variants are NOT imported so they cannot
    /// shadow them).
    pub fn scan() -> Result<HostScan, String> {
        let mut scan = HostScan::default();
        let root = open_key(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Enum")
            .map_err(|code| format!("open Enum failed, win32 error {code}"))?;
        let result = walk(&root, String::new(), 0, &mut scan);
        unsafe {
            let _ = RegCloseKey(root);
        }
        result?;
        Ok(scan)
    }

    fn walk(
        parent: &HKEY,
        prefix: String,
        depth: usize,
        scan: &mut HostScan,
    ) -> Result<(), String> {
        // Instance keys live at `Enumerator\DeviceContainer\Instance` (3
        // levels under `Enum`; BLE GATT is the deepest common form). The
        // extra levels only bound runaway recursion through housekeeping
        // subkeys (`Device Parameters`, `LogConf`, ...).
        const MAX_DEPTH: usize = 5;
        if depth > MAX_DEPTH {
            return Ok(());
        }
        for name in enum_subkeys(*parent) {
            // PnP housekeeping subtrees are skipped entirely: `Properties`
            // is ACL-locked (one denied open per key on a full-tree walk)
            // and `LogConf` never nests device instances; neither can carry
            // `Device Parameters\WUDFDiagnosticInfo`.
            if matches!(name.as_str(), "Properties" | "LogConf") {
                continue;
            }
            let child_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!(r"{prefix}\{name}")
            };
            scan.nodes_seen += 1;
            let child = match open_key(*parent, &name) {
                Ok(key) => key,
                Err(code) => {
                    scan.read_failures
                        .push((child_path.clone(), format!("key open failed ({code})")));
                    continue;
                }
            };
            // Does this node carry the diagnostic value? Open the subkey path
            // directly; a nested RegOpenKeyExW handles the two-level suffix.
            match open_key(child, WUDF_DIAG_SUFFIX) {
                Ok(diag_key) => {
                    scan.diag_keys_opened += 1;
                    let entry = HostEntry {
                        instance_id: instance_id_for_key_path(&child_path),
                        is_rc003: is_rc003_key_path(&child_path),
                        pid: match read_host_pid(diag_key) {
                            PidDecode::Pid(pid) => pid,
                            PidDecode::Zero => {
                                scan.no_host_count += 1;
                                0
                            }
                            PidDecode::BadSize | PidDecode::BadType => {
                                scan.read_failures.push((
                                    instance_id_for_key_path(&child_path),
                                    "HostPid unreadable (type/width mismatch)".to_string(),
                                ));
                                0
                            }
                        },
                    };
                    scan.entries.push(entry);
                    unsafe {
                        let _ = RegCloseKey(diag_key);
                    }
                }
                Err(_) => {
                    // Regular device instance without UMDF diagnostics: walk
                    // deeper (instance ids are nested below the interface key).
                    let sub_result = walk(&child, child_path, depth + 1, scan);
                    unsafe {
                        let _ = RegCloseKey(child);
                    }
                    sub_result?;
                    continue;
                }
            }
            unsafe {
                let _ = RegCloseKey(child);
            }
        }
        Ok(())
    }

    pub use scan as scan_hosts_impl;
}

#[cfg(target_os = "windows")]
pub use windows_impl::scan_hosts_impl as scan_hosts;

#[cfg(test)]
mod tests {
    use super::*;

    fn le(value: u64, n: usize) -> Vec<u8> {
        // Little-endian byte decomposition; shifts beyond the value width
        // must contribute zero bytes, not panic (test builds wider buffers
        // than the value).
        let bytes = value.to_le_bytes();
        (0..n).map(|i| if i < 8 { bytes[i] } else { 0 }).collect()
    }

    #[test]
    fn qword_pid_decodes() {
        // 8-byte REG_QWORD is the shape current Windows actually writes.
        assert_eq!(
            decode_host_pid(REG_QWORD, &le(4012, 8)),
            PidDecode::Pid(4012)
        );
        assert_eq!(
            decode_host_pid(REG_QWORD, &le(0x0000_0000_1F_90, 8)),
            PidDecode::Pid(0x1F90)
        );
    }

    #[test]
    fn dword_pid_decodes() {
        // Older/other builds may store a 4-byte REG_DWORD; both widths must work.
        assert_eq!(
            decode_host_pid(REG_DWORD, &le(4012, 4)),
            PidDecode::Pid(4012)
        );
        assert_eq!(
            decode_host_pid(REG_DWORD, &le(65535, 4)),
            PidDecode::Pid(65535)
        );
    }

    #[test]
    fn binary_pid_decodes_at_both_widths() {
        assert_eq!(decode_host_pid(REG_BINARY, &le(77, 8)), PidDecode::Pid(77));
        assert_eq!(decode_host_pid(REG_BINARY, &le(77, 4)), PidDecode::Pid(77));
        assert_eq!(decode_host_pid(REG_BINARY, &le(77, 3)), PidDecode::BadSize);
        assert_eq!(decode_host_pid(REG_BINARY, &le(77, 16)), PidDecode::BadSize);
    }

    #[test]
    fn zero_means_host_not_running() {
        assert_eq!(decode_host_pid(REG_QWORD, &le(0, 8)), PidDecode::Zero);
        assert_eq!(decode_host_pid(REG_DWORD, &le(0, 4)), PidDecode::Zero);
    }

    #[test]
    fn garbage_widths_and_types_fail_distinctly() {
        // Truncated buffers must not silently decode as the low bytes.
        assert_eq!(decode_host_pid(REG_QWORD, &le(4012, 4)), PidDecode::BadSize);
        assert_eq!(decode_host_pid(REG_DWORD, &le(4012, 2)), PidDecode::BadSize);
        assert_eq!(
            decode_host_pid(REG_SZ, "0123".as_bytes()),
            PidDecode::BadType
        );
        assert_eq!(decode_host_pid(REG_NONE, &[]), PidDecode::BadType);
    }

    #[test]
    fn oversized_values_are_rejected_not_truncated() {
        // A "pid" wider than u32 is malformed data, never a big pid.
        let huge = le(u64::from(u32::MAX) + 1, 8);
        assert_eq!(decode_host_pid(REG_QWORD, &huge), PidDecode::BadSize);
        let max = le(u64::from(u32::MAX), 8);
        assert_eq!(decode_host_pid(REG_QWORD, &max), PidDecode::Pid(u32::MAX));
    }

    #[test]
    fn rc003_paths_recognized_case_insensitively() {
        assert!(is_rc003_key_path(
            r"VID_2717&PID_32B8&MI_01\7&2d5f3a4b&0&0000"
        ));
        assert!(is_rc003_key_path(r"vid_2717&pid_32b8&mi_00"));
        assert!(!is_rc003_key_path(r"VID_1234&PID_5678&MI_01\7&0000"));
        assert!(!is_rc003_key_path(r"VID_2717&PID_0000&MI_01"));
        assert!(!is_rc003_key_path(r"VID_0000&PID_32B8&MI_01"));
    }

    #[test]
    fn rc003_ble_gatt_paths_recognized() {
        // Real BLE-paired RC003 on Win11: the HID service collection lives
        // under BTHLEDEVICE with the `VID&<source><vid>` GATT encoding, in
        // lowercase from the registry.
        assert!(is_rc003_key_path(
            r"BTHLEDEVICE\{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&012717_PID&32b8_REV&00a4_c05d39c3274f\b&20a7291a&1&0055"
        ));
        assert!(is_rc003_key_path(
            r"BTHLEDEVICE\{00001812-0000-1000-8000-00805F9B34FB}_DEV_VID&012717_PID&32B8_REV&00A4_C05D39C3274F\B&20A7291A&1&0055"
        ));
        // Wrong vendor source prefix (01 = Bluetooth SIG), wrong pid.
        assert!(!is_rc003_key_path(
            r"BTHLEDEVICE\{00001812-...}_Dev_VID&022717_PID&32b8_REV&00a4_c05d39c3274f\b&1&0055"
        ));
        assert!(!is_rc003_key_path(
            r"BTHLEDEVICE\{00001812-...}_Dev_VID&012717_PID&1111_REV&00a4_c05d39c3274f\b&1&0055"
        ));
        // The USB fragments must not leak into the GATT form and vice versa.
        assert!(!is_rc003_key_path(
            r"BTHLEDEVICE\{0000fe59-...}_Dev_VID&010a46_PID&9000_REV&0001_001122334455\b&1&0804"
        ));
    }

    #[test]
    fn instance_ids_keep_their_enumerator_prefix() {
        // Walk paths are relative to the `Enum` root, so the instance id is
        // the path verbatim — PnP/pnputil accept both enumerator forms.
        assert_eq!(
            instance_id_for_key_path(r"HID\VID_2717&PID_32B8&MI_01\7&2d5f3a4b&0&0000"),
            r"HID\VID_2717&PID_32B8&MI_01\7&2d5f3a4b&0&0000"
        );
        let usb = HostEntry {
            instance_id: instance_id_for_key_path(r"HID\VID_2717&PID_32B8&MI_01\7&1&0000"),
            is_rc003: true,
            pid: 99,
        };
        assert!(is_rc003_entry(&usb));
        let ble = HostEntry {
            instance_id: instance_id_for_key_path(
                r"BTHLEDEVICE\{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&012717_PID&32b8_REV&00a4_c05d39c3274f\b&20a7291a&1&0055",
            ),
            is_rc003: true,
            pid: 2672,
        };
        assert!(is_rc003_entry(&ble));
    }
}
