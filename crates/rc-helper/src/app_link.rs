//! App link: named-pipe CLIENT toward the app's bridge server.
//!
//! The app publishes `%LOCALAPPDATA%\RemoteMicRune\bridge.ini` (pipe name,
//! token, pid, protocol version). The helper connects to
//! `\\.\pipe\RemoteMicRune.Bridge.v1`, sends
//! `HELLO <version> <token> <pid>` and waits for `OK` (or `DENY`, which
//! sends us back to re-reading the descriptor with backoff — the token may
//! have been rotated by a restart).
//!
//! On app disconnect the MAIN loop tears the agent down (Disarm + Bye) so
//! keys return to native immediately; whether this process then exits
//! (`--follow-app`) or retries the descriptor is the caller's choice.
//! Ping (`P`) every 1 s keeps the app's silence watchdog fed.

use std::path::PathBuf;

use rc_protocol::consts::{
    APP_DIR_NAME, BRIDGE_DESCRIPTOR_FILE, BRIDGE_PING_INTERVAL_MS, BRIDGE_PROTOCOL_VERSION,
};
use rc_protocol::handoff::BridgeDescriptor;

// Pipe pumping exists only on Windows; path/parsing helpers stay host-tested.
#[cfg(target_os = "windows")]
use {
    std::sync::atomic::{AtomicBool, Ordering},
    std::sync::mpsc::{Receiver, Sender, TryRecvError},
    std::sync::{Arc, Mutex},
    rc_protocol::bridge_wire::{parse_bridge_line, BridgeLine},
    rc_protocol::consts::PIPE_IDLE_BACKOFF_MS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// HELLO answered with OK; carries the initial targets generation.
    Ok { version: u32, generation: u64, usages: Vec<u16> },
    Targets { generation: u64, usages: Vec<u16> },
    Synth { to: Option<u16> },
    GateDeclare,
    Deny { reason: String },
    Bye { reason: String },
    /// The pipe session ended.
    Disconnected,
}

/// Default descriptor path: `%LOCALAPPDATA%\RemoteMicRune\bridge.ini`.
pub fn descriptor_path(local_appdata: &str) -> PathBuf {
    PathBuf::from(local_appdata)
        .join(APP_DIR_NAME)
        .join(BRIDGE_DESCRIPTOR_FILE)
}

/// Backoff for descriptor re-reads after DENY or a failed connect.
pub const RECONNECT_BACKOFF_MS: u64 = 2000;

/// How long `--follow-app` waits for the app to (re)appear before exiting.
/// Must exceed an app cold start (seconds) but not much more — a helper that
/// never exits is a zombie elevated process.
pub const FOLLOW_APP_GRACE_MS: u64 = 20_000;

/// Read + parse + version-check the descriptor.
pub fn read_descriptor(local_appdata: &str) -> Result<BridgeDescriptor, String> {
    let path = descriptor_path(local_appdata);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("descriptor {} unreadable: {e}", path.display()))?;
    let descriptor = BridgeDescriptor::decode(&text).map_err(|e| {
        format!(
            "descriptor {} invalid: {e}",
            path.display()
        )
    })?;
    if !descriptor.version_ok() {
        return Err(format!(
            "descriptor version {} != protocol version {BRIDGE_PROTOCOL_VERSION}",
            descriptor.version
        ));
    }
    Ok(descriptor)
}

#[cfg(target_os = "windows")]
struct SessionShared {
    lines: Mutex<Sender<String>>,
    close: AtomicBool,
}

#[cfg(target_os = "windows")]
impl SessionShared {
    fn send(&self, line: String) -> bool {
        match self.lines.lock() {
            Ok(tx) => tx.send(line).is_ok(),
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "windows")]
pub struct AppLink {
    shared: Arc<SessionShared>,
    connected: Arc<AtomicBool>,
}

#[cfg(target_os = "windows")]
impl AppLink {
    /// Connect to the bridge pipe, spawn the pump threads.
    /// `hello_line` is sent immediately after connect.
    pub fn connect(
        pipe_name: &str,
        hello_line: String,
        events: Sender<AppEvent>,
        shutdown: Arc<AtomicBool>,
        logger: std::sync::Arc<crate::logging::Logger>,
    ) -> Result<AppLink, String> {
        let client = crate::pipe::PipeClient::open(pipe_name, 5000)?;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let shared = Arc::new(SessionShared {
            lines: Mutex::new(tx),
            close: AtomicBool::new(false),
        });
        let connected = Arc::new(AtomicBool::new(true));
        if shared.send(hello_line.clone()) {
            logger.info(&format!("sent {hello_line}"));
        }
        let pump_shared = shared.clone();
        let pump_connected = connected.clone();
        std::thread::Builder::new()
            .name("app-session".into())
            .spawn(move || {
                session_loop(client, rx, pump_shared, pump_connected, events, shutdown, logger);
            })
            .map_err(|e| format!("failed to spawn app session: {e}"))?;
        Ok(AppLink { shared, connected })
    }

    pub fn send_line(&self, line: String) {
        if self.connected.load(Ordering::SeqCst) {
            let _ = self.shared.lines.lock().map(|tx| tx.send(line));
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Send the goodbye line and close the session promptly.
    pub fn close(&self, bye_line: Option<String>) {
        if let Some(line) = bye_line {
            self.send_line(line);
        }
        self.shared.close.store(true, Ordering::SeqCst);
    }
}

#[cfg(target_os = "windows")]
fn session_loop(
    mut client: crate::pipe::PipeClient,
    lines: Receiver<String>,
    shared: Arc<SessionShared>,
    connected: Arc<AtomicBool>,
    events: Sender<AppEvent>,
    shutdown: Arc<AtomicBool>,
    logger: std::sync::Arc<crate::logging::Logger>,
) {
    let mut read_buf = [0u8; 4096];
    let mut line_buf: Vec<u8> = Vec::new();
    let idle = std::time::Duration::from_millis(PIPE_IDLE_BACKOFF_MS);
    loop {
        // Lines out.
        loop {
            match lines.try_recv() {
                Ok(line) => {
                    if client.write_all(line.as_bytes()).is_err()
                        || client.write_all(b"\n").is_err()
                    {
                        logger.warn("app pipe write failed; session ending");
                        break;
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if shared.close.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Lines in.
        match client.read_poll(&mut read_buf) {
            Ok(0) => {}
            Ok(n) => {
                for line in crate::agent_link::split_lines(&mut line_buf, &read_buf[..n]) {
                    match parse_bridge_line(&line) {
                        Ok(Some(parsed)) => {
                            let event = match parsed {
                                BridgeLine::Ok { version, generation, usages } => {
                                    Some(AppEvent::Ok { version, generation, usages })
                                }
                                BridgeLine::Targets { generation, usages } => {
                                    Some(AppEvent::Targets { generation, usages })
                                }
                                BridgeLine::Synth { to } => Some(AppEvent::Synth { to }),
                                BridgeLine::GateDeclare => Some(AppEvent::GateDeclare),
                                BridgeLine::Deny { reason } => Some(AppEvent::Deny { reason }),
                                BridgeLine::Bye { reason } => Some(AppEvent::Bye { reason }),
                                // Lines the helper never receives.
                                BridgeLine::Hello { .. }
                                | BridgeLine::Edges { .. }
                                | BridgeLine::Ping
                                | BridgeLine::Recovering { .. }
                                | BridgeLine::Ownership { .. }
                                | BridgeLine::SynthAck { .. }
                                | BridgeLine::VoiceFrame { .. } => None,
                            };
                            if let Some(event) = event {
                                let _ = events.send(event);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            logger.warn(&format!("undecodable app line ({e})"));
                        }
                    }
                }
            }
            Err(_) => {
                logger.info("app pipe read error; session ending");
                break;
            }
        }
        if shared.close.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(idle);
    }
    connected.store(false, Ordering::SeqCst);
    let _ = events.send(AppEvent::Disconnected);
    client.close();
}

/// Ping interval helper for the main loop's timer.
pub fn ping_due(last_ping: std::time::Instant) -> bool {
    last_ping.elapsed() >= std::time::Duration::from_millis(BRIDGE_PING_INTERVAL_MS as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_path_uses_localappdata_dir() {
        let path = descriptor_path(r"C:\Users\u\AppData\Local");
        let s = path.to_string_lossy().replace('/', "\\");
        assert!(s.ends_with(r"\RemoteMicRune\bridge.ini"), "got {s}");
        assert!(s.contains("AppData\\Local"), "got {s}");
    }

    #[test]
    fn descriptor_read_reports_missing_file() {
        let err = read_descriptor(r"C:\definitely\not\here").unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn ping_interval_matches_contract() {
        let start = std::time::Instant::now();
        assert!(!ping_due(start));
        std::thread::sleep(std::time::Duration::from_millis(5));
        // 5 ms is far below the 1 s interval.
        assert!(!ping_due(start));
    }

    #[test]
    fn reconnect_backoff_is_documented_value() {
        assert_eq!(RECONNECT_BACKOFF_MS, 2000);
        assert_eq!(FOLLOW_APP_GRACE_MS, 20_000);
        assert_eq!(BRIDGE_PROTOCOL_VERSION, 1);
    }
}
