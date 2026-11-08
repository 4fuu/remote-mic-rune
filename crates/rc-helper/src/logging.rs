//! Simple line logger: `timestamp [LEVEL] message` appended to a file.
//!
//! Rules:
//! - Never panics: every fallible write is dropped on the floor.
//! - Never blocks the hot path meaningfully: one short mutex-guarded write
//!   per call, no flushing gymnastics (the OS page cache does the rest).
//! - When the file cannot be opened, lines are silently dropped rather than
//!   taking the helper down — the helper must keep failing open.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Logger {
    file: Mutex<Option<File>>,
}

impl Logger {
    /// Open (append) the log file, creating parent directories if needed.
    /// A logger without a file is still usable: it just drops lines.
    pub fn open(path: &Path) -> Logger {
        let file = if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            OpenOptions::new().create(true).append(true).open(path).ok()
        } else {
            OpenOptions::new().create(true).append(true).open(path).ok()
        };
        Logger {
            file: Mutex::new(file),
        }
    }

    /// A logger that drops every line (tests, dry-run without a file).
    #[allow(dead_code)] // unit-test companion
    pub fn disabled() -> Logger {
        Logger {
            file: Mutex::new(None),
        }
    }

    pub fn info(&self, message: &str) {
        self.write("INFO", message);
    }

    pub fn warn(&self, message: &str) {
        self.write("WARN", message);
    }

    pub fn error(&self, message: &str) {
        self.write("ERROR", message);
    }

    /// One `key=value` diagnostic line in a stable order.
    pub fn kv(&self, tag: &str, fields: &[(&str, String)]) {
        let joined = fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.write("INFO", &format!("{tag} {joined}"));
    }

    fn write(&self, level: &str, message: &str) {
        let line = format!("{} [{level}] {}\n", timestamp_millis(), message);
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
            }
        }
    }
}

/// Milliseconds since the Unix epoch (log timestamps, wire `E` lines).
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC-3339-ish UTC timestamp with millisecond precision, e.g.
/// `2026-11-08T09:15:42.123Z`. Pure enough to assert on the host.
pub fn format_timestamp(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let millis = unix_ms % 1000;
    // Days since epoch -> civil date (Howard Hinnant's algorithm, public
    // domain shape); keeps the logger free of any external crate.
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        millis
    )
}

fn timestamp_millis() -> String {
    format_timestamp(unix_ms())
}

/// Convert days since 1970-01-01 to (year, month, day) in the proleptic
/// Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_render_as_utc_rfc3339() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_timestamp(1), "1970-01-01T00:00:00.001Z");
        // 2026-11-08T00:00:00Z = 1794096000 s.
        assert_eq!(
            format_timestamp(1_794_096_000_000),
            "2026-11-08T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(1_794_096_000_001),
            "2026-11-08T00:00:00.001Z"
        );
        // Leap-year day (2024-02-29T13:54:56.789Z = 1709214896789 ms).
        assert_eq!(
            format_timestamp(1_709_214_896_789),
            "2024-02-29T13:54:56.789Z"
        );
    }

    #[test]
    fn unix_ms_moves_forward() {
        let a = unix_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(unix_ms() >= a);
    }

    #[test]
    fn logger_never_panics_without_file() {
        let logger = Logger::disabled();
        logger.info("nothing blows up");
        logger.kv("[TAG]", &[("k", "v".to_string())]);
        // Also with an unwritable path (a directory as file path).
        let logger = Logger::open(Path::new("/tmp"));
        logger.warn("still no panic");
    }
}
