//! Agent link: named-pipe server the injected agent connects to.
//!
//! Transport: `\\.\pipe\RemoteMicRune.Agent.<helper pid>` (the full name is
//! handed to the DLL in the boot config). The pipe is created with an
//! explicit SDDL security descriptor; it is a loopback-only transport and
//! the agent additionally token-checks every command, so a forged renew
//! never arms anything.
//!
//! Sessions: one client at a time, newest wins. When a second agent connects
//! (fresh boot into a restarted host), the previous session gets a polite,
//! token-signed Bye and is disconnected.
//!
//! The lease lives in the main loop's renew timer; this module only moves
//! bytes. Framing helpers are pure and host-tested.

use rc_protocol::agent_wire::{AgentHello, AgentHeartbeat};
use rc_protocol::consts::MAX_LINE_BYTES;

// Named-pipe transport plumbing exists only on Windows; the framing helpers
// above and the event enum are host-testable.
#[cfg(target_os = "windows")]
use {
    std::sync::atomic::{AtomicBool, Ordering},
    std::sync::mpsc::{Receiver, Sender, TryRecvError},
    std::sync::{Arc, Mutex},
    rc_protocol::agent_wire::{decode_agent_line, encode_helper_line, HelperToAgent},
    rc_protocol::consts::{PIPE_BUFFER_BYTES, PIPE_IDLE_BACKOFF_MS},
};

/// SDDL for the agent pipe: owner-pipe (no inherited ACEs), generic-all for
/// World. Reachable from any local process, which is exactly why the token
/// check on every command is mandatory.
pub const AGENT_PIPE_SDDL: &str = "D:P(A;;GA;;;WD)";

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A client connected to the server pipe.
    Connected,
    /// First line after connect; token validity is checked by the main loop.
    Hello(AgentHello),
    Hb(AgentHeartbeat),
    /// Absolute pressed set plus its reason ("state" or a forced release).
    Edge { usages: Vec<u16>, reason: String },
    TargetsAck { generation: u64, usages: Vec<u16> },
    SynthAck { from: u16, to: u16 },
    VoiceFrame { replaced: bool, reason: String },
    Log(String),
    /// The agent said goodbye.
    Bye,
    /// The pipe session ended (disconnect, error, or superseded).
    Disconnected { had_hello: bool },
}

/// Extract complete `\n`-terminated lines from the read buffer.
/// Oversized lines (protocol violations) are dropped without killing the
/// session; a buffer growing past the line cap without a newline is reset.
pub fn split_lines(buffer: &mut Vec<u8>, incoming: &[u8]) -> Vec<String> {
    buffer.extend_from_slice(incoming);
    let mut out = Vec::new();
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let drained: Vec<u8> = buffer.drain(..=pos).collect();
        let mut line = String::from_utf8_lossy(&drained[..drained.len() - 1]).to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        if line.len() <= MAX_LINE_BYTES {
            out.push(line);
        }
    }
    if buffer.len() > MAX_LINE_BYTES {
        // No newline within a full line's worth of bytes: the peer is not
        // speaking this protocol; drop the garbage instead of growing forever.
        buffer.clear();
    }
    out
}

/// State shared between the link handle, the accept loop, and one session.
#[cfg(target_os = "windows")]
struct SessionShared {
    commands: Mutex<Sender<HelperToAgent>>,
    close: AtomicBool,
    /// Token learned from the agent's Hello; used to sign the polite Bye.
    token: Mutex<Option<String>>,
    hello_seen: AtomicBool,
}

#[cfg(target_os = "windows")]
impl SessionShared {
    fn send(&self, command: HelperToAgent) -> bool {
        match self.commands.lock() {
            Ok(tx) => tx.send(command).is_ok(),
            Err(_) => false,
        }
    }

    fn send_bye(&self) {
        let token = self
            .token
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        let _ = self
            .commands
            .lock()
            .map(|tx| tx.send(HelperToAgent::Bye { token }));
        self.close.store(true, Ordering::SeqCst);
    }
}

#[cfg(target_os = "windows")]
struct Inner {
    current: Mutex<Option<Arc<SessionShared>>>,
    shutdown: AtomicBool,
    /// The token a genuine agent presents in its Hello. Starts as the boot
    /// token and is updated whenever the helper boots a new agent (device
    /// recovery) or adopts a rotated app token.
    expected_token: Mutex<String>,
}

#[cfg(target_os = "windows")]
pub struct AgentLink {
    inner: Arc<Inner>,
}

#[cfg(target_os = "windows")]
impl AgentLink {
    /// Create the server pipe and start the accept loop thread. The
    /// accept loop ends when `stop()` is called (or the process exits);
    /// the helper's own shutdown flag drives teardown through that call.
    /// `expected_token` is the token a genuine agent must present.
    pub fn start(
        pipe_name: String,
        events: Sender<AgentEvent>,
        expected_token: String,
        logger: std::sync::Arc<crate::logging::Logger>,
    ) -> Result<AgentLink, String> {
        // Fail fast if the pipe cannot be created at all: without it no
        // agent can ever connect and every later step is dead code.
        let mut probe = crate::pipe::ServerPipe::create(&pipe_name, AGENT_PIPE_SDDL, PIPE_BUFFER_BYTES)?;
        probe.close();

        let inner = Arc::new(Inner {
            current: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            expected_token: Mutex::new(expected_token),
        });
        let link = AgentLink { inner: inner.clone() };
        let accept_logger = logger;
        std::thread::Builder::new()
            .name("agent-accept".into())
            .spawn(move || accept_loop(pipe_name, events, inner, accept_logger))
            .map_err(|e| format!("failed to spawn accept loop: {e}"))?;
        Ok(link)
    }
    /// Queue one command to the CURRENT agent session (no-op if none).
    pub fn send(&self, command: HelperToAgent) {
        if let Ok(guard) = self.inner.current.lock() {
            if let Some(session) = guard.as_ref() {
                // Send errors mean the session thread is already gone; the
                // Disconnected event handles the state change.
                let _ = session.send(command);
            }
        }
    }

    /// Adopt a new expected token (app rotation, recovery re-injection).
    /// Applies to the NEXT hello; a live authenticated session is unaffected.
    pub fn set_expected_token(&self, token: String) {
        if let Ok(mut guard) = self.inner.expected_token.lock() {
            *guard = token;
        }
    }

    /// Force-close the current session (e.g. Hello with a wrong token).
    pub fn dismiss_current(&self) {
        if let Ok(mut guard) = self.inner.current.lock() {
            if let Some(session) = guard.take() {
                session.send_bye();
            }
        }
    }

    /// Stop accepting; dismiss the live session. Called at teardown.
    pub fn stop(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.dismiss_current();
    }
}

#[cfg(target_os = "windows")]
fn accept_loop(
    server_name: String,
    events: Sender<AgentEvent>,
    inner: Arc<Inner>,
    logger: std::sync::Arc<crate::logging::Logger>,
) {
    while !inner.shutdown.load(Ordering::SeqCst) {
        let mut pipe =
            match crate::pipe::ServerPipe::create(&server_name, AGENT_PIPE_SDDL, PIPE_BUFFER_BYTES)
            {
                Ok(pipe) => pipe,
                Err(e) => {
                    logger.error(&format!("agent server pipe create failed: {e}"));
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    continue;
                }
            };
        if pipe.connect_blocking().is_err() {
            logger.warn("agent server pipe connect failed; recreating instance");
            pipe.close();
            continue;
        }
        if inner.shutdown.load(Ordering::SeqCst) {
            pipe.close();
            break;
        }
        logger.info(&format!("agent client connected on {server_name} (instance handle 0x{:X})", pipe.handle.0 as usize));
        let _ = events.send(AgentEvent::Connected);

        // Newest wins: politely dismiss any session still attached.
        if let Ok(mut guard) = inner.current.lock() {
            if let Some(old) = guard.take() {
                logger.info("newest wins: dismissing previous agent session");
                old.send_bye();
            }
        }

        let (tx, rx) = std::sync::mpsc::channel::<HelperToAgent>();
        let shared = Arc::new(SessionShared {
            commands: Mutex::new(tx),
            close: AtomicBool::new(false),
            token: Mutex::new(None),
            hello_seen: AtomicBool::new(false),
        });
        if let Ok(mut guard) = inner.current.lock() {
            *guard = Some(shared.clone());
        }
        let session_events = events.clone();
        let session_logger = logger.clone();
        let session_shared = shared;
        let session_inner = inner.clone();
        if std::thread::Builder::new()
            .name("agent-session".into())
            .spawn(move || {
                session_loop(pipe, rx, session_shared, session_inner, session_events, session_logger);
            })
            .is_err()
        {
            logger.error("failed to spawn agent session thread");
        }
    }
}

#[cfg(target_os = "windows")]
fn session_loop(
    mut pipe: crate::pipe::ServerPipe,
    commands: Receiver<HelperToAgent>,
    shared: Arc<SessionShared>,
    inner: std::sync::Arc<Inner>,
    events: Sender<AgentEvent>,
    logger: std::sync::Arc<crate::logging::Logger>,
) {
    let mut read_buf = [0u8; 4096];
    let mut line_buf: Vec<u8> = Vec::new();
    let idle = std::time::Duration::from_millis(PIPE_IDLE_BACKOFF_MS);
    loop {
        // Commands out (drained first so a queued Bye always lands before
        // the next sleep).
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    let line = encode_helper_line(&command);
                    if pipe.write_all(line.as_bytes()).is_err() || pipe.write_all(b"\n").is_err() {
                        logger.warn("agent write failed; session ending");
                        break;
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if shared.close.load(Ordering::SeqCst) {
            break;
        }
        // Events in.
        match pipe.read_poll(&mut read_buf) {
            Ok(0) => {}
            Ok(n) => {
                for line in split_lines(&mut line_buf, &read_buf[..n]) {
                    match decode_agent_line(&line) {
                        Some(rc_protocol::agent_wire::AgentToHelper::Hello { t, hello }) => {
                            let _ = t;
                            // Record whatever token the agent presented so a
                            // later polite Bye is signed correctly, even for
                            // a rejected peer.
                            if let Ok(mut guard) = shared.token.lock() {
                                *guard = Some(hello.token.clone());
                            }
                            // Only a helper-booted agent may stay: its boot
                            // config carries the token we injected. A mismatch
                            // means a foreign helper's agent found our pipe.
                            let expected = inner
                                .expected_token
                                .lock()
                                .map(|g| g.clone())
                                .unwrap_or_default();
                            if hello.token != expected {
                                logger.warn("agent hello token mismatch; disconnecting the peer");
                                let bye = encode_helper_line(&HelperToAgent::Bye {
                                    token: hello.token.clone(),
                                });
                                let _ = pipe.write_all(bye.as_bytes());
                                let _ = pipe.write_all(b"\n");
                                pipe.flush();
                                break;
                            }
                            shared.hello_seen.store(true, Ordering::SeqCst);
                            let _ = events.send(AgentEvent::Hello(hello));
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::Hb { t, hb }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::Hb(hb));
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::Edge { t, usages, reason }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::Edge { usages, reason });
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::TargetsAck { t, generation, usages }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::TargetsAck { generation, usages });
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::SynthAck { t, from, to }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::SynthAck { from, to });
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::VoiceFrame { t, replaced, reason }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::VoiceFrame { replaced, reason });
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::Log { t, msg }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::Log(msg));
                        }
                        Some(rc_protocol::agent_wire::AgentToHelper::Bye { t }) => {
                            let _ = t;
                            let _ = events.send(AgentEvent::Bye);
                        }
                        None => {
                            logger.warn(&format!(
                                "undecodable agent line ({} bytes) dropped",
                                line.len()
                            ));
                        }
                    }
                }
            }
            Err(_) => {
                logger.info("agent pipe read error; session ending");
                break;
            }
        }
        if shared.close.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(idle);
    }
    let had_hello = shared.hello_seen.load(Ordering::SeqCst);
    // Detach bookkeeping: the accept loop replaces the `current` slot before
    // the old session thread observes `close`, so the session loop never
    // mutates that slot; it only publishes the disconnect.
    let _ = events.send(AgentEvent::Disconnected { had_hello });
    pipe.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_basic_and_crlf() {
        let mut buf = Vec::new();
        let lines = split_lines(&mut buf, b"{\"a\":1}\n{\"a\":2}\r\n");
        assert_eq!(lines, vec![r#"{"a":1}"#, r#"{"a":2}"#]);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_lines_wait_for_newline() {
        let mut buf = Vec::new();
        assert!(split_lines(&mut buf, b"hello wor").is_empty());
        let lines = split_lines(&mut buf, b"ld\nnext\n");
        assert_eq!(lines, vec!["hello world", "next"]);
    }

    #[test]
    fn oversized_lines_are_dropped_not_fatal() {
        let mut buf = Vec::new();
        let big = vec![b'x'; MAX_LINE_BYTES + 100];
        let mut payload = big;
        payload.push(b'\n');
        payload.extend_from_slice(b"ok\n");
        let lines = split_lines(&mut buf, &payload);
        assert_eq!(lines, vec!["ok"]);
    }

    #[test]
    fn runaway_buffer_is_reset() {
        let mut buf = Vec::new();
        let chunk = vec![b'y'; MAX_LINE_BYTES + 1];
        assert!(split_lines(&mut buf, &chunk).is_empty());
        assert!(buf.is_empty(), "buffer over the cap without a newline must reset");
    }

    #[test]
    fn sddl_descriptor_is_owner_piped_world_access() {
        assert_eq!(AGENT_PIPE_SDDL, "D:P(A;;GA;;;WD)");
    }

    #[test]
    fn event_forwarding_shapes_carry_protocol_fields() {
        // Spot-check the event enum keeps exactly the fields the bridge
        // formatter needs.
        let e = AgentEvent::Edge {
            usages: vec![0x4A],
            reason: "state".into(),
        };
        assert_eq!(
            e,
            AgentEvent::Edge {
                usages: vec![0x4A],
                reason: "state".into()
            }
        );
        let a = AgentEvent::TargetsAck {
            generation: 3,
            usages: vec![],
        };
        assert!(matches!(a, AgentEvent::TargetsAck { generation: 3, .. }));
    }
}
