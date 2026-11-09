//! Best-effort diagnostics: an append-only log file under `%TEMP%` plus a
//! mirrored `Log` line to the helper. Nothing here may fail the hook: every
//! error is swallowed, and synth frame traces are rate-limited (8/s) so a
//! chatty device cannot flood the link.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use rc_protocol::agent_wire::AgentToHelper;

use crate::clock::{mono_ms, unix_ms};

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

const LOG_TRUNC_CHARS: usize = 400;
const SYNTH_TRACE_MAX_PER_SEC: u32 = 8;
const SYNTH_TRACE_WINDOW_MS: u64 = 1000;

struct TraceWindow {
    window_ms: u64,
    count: u32,
    dropped: u32,
}

static TRACE: Mutex<TraceWindow> = Mutex::new(TraceWindow {
    window_ms: 0,
    count: 0,
    dropped: 0,
});

fn log_path() -> &'static Option<PathBuf> {
    LOG_PATH.get_or_init(|| {
        let pid = std::process::id();
        // Candidate directories in preference order. A session-0 WUDFHost
        // runs with a restricted service token: %TEMP% may be unset or
        // unwritable there, so the shared ProgramData directory (where the
        // helper also logs) is the fallback that keeps field debugging
        // possible.
        let mut dirs: Vec<PathBuf> = Vec::new();
        for var in ["TEMP", "TMP"] {
            if let Ok(dir) = std::env::var(var) {
                if !dir.is_empty() && !dirs.contains(&PathBuf::from(&dir)) {
                    dirs.push(PathBuf::from(dir));
                }
            }
        }
        if let Ok(pd) = std::env::var("ProgramData") {
            dirs.push(PathBuf::from(pd).join("RemoteMicRune"));
        }
        for dir in &dirs {
            let path = dir.join(format!("rcr-agent-{pid}.log"));
            // The candidate must be openable for append; the first one that
            // is wins for the lifetime of the process.
            if OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .is_ok()
            {
                return Some(path);
            }
        }
        None
    })
}

/// Create the path lazily on first use (called from boot).
pub fn init() {
    let _ = log_path();
}

/// Append one timestamped line to the log file. Best-effort: open errors,
/// write errors and a missing %TEMP% are all ignored.
pub fn file_log(msg: &str) {
    let Some(path) = log_path() else {
        return;
    };
    let line = format!("{} [{}] {msg}\n", unix_ms(), mono_ms());
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    use std::io::Write;
    let _ = f.write_all(line.as_bytes());
}

/// Log to the file and mirror to the helper (truncated). Safe before boot:
/// the file copy still works, the mirror is skipped.
pub fn log_event(msg: &str) {
    file_log(msg);
    let truncated: String = msg.chars().take(LOG_TRUNC_CHARS).collect();
    crate::transport::enqueue_agent(&AgentToHelper::Log {
        t: unix_ms(),
        msg: truncated,
    });
}

/// Rate-limited synthesis frame trace (8 lines per second, the overflow
/// counted and carried on the next line that does get through).
pub fn synth_trace(msg: &str) {
    let now = unix_ms();
    let mut t = match TRACE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if now.saturating_sub(t.window_ms) >= SYNTH_TRACE_WINDOW_MS {
        t.window_ms = now;
        t.count = 0;
    }
    if t.count >= SYNTH_TRACE_MAX_PER_SEC {
        t.dropped += 1;
        return;
    }
    t.count += 1;
    let dropped = std::mem::take(&mut t.dropped);
    drop(t);
    let suffix = if dropped > 0 {
        format!(" dropped={dropped}")
    } else {
        String::new()
    };
    log_event(&format!("synth:frame {msg}{suffix}"));
}
