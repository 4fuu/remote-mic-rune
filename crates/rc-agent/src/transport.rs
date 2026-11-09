//! Named-pipe transport: one JSON line per frame in both directions, three
//! threads total.
//!
//! - reader: owns reconnection. Connects to the pipe named in the boot
//!   config, feeds decoded `HelperToAgent` lines into the state machine and
//!   ships back produced messages. Idle reads poll with `PeekNamedPipe` and
//!   the shared 2 ms idle backoff, so the rx watchdog stays meaningful.
//! - writer: the ONLY writer, fed by the outbound queue (single serialized
//!   writer — interleaved concurrent writers can silently lose whole
//!   message classes). Writes are synchronous, matching the handle's mode:
//!   the connection is opened WITHOUT FILE_FLAG_OVERLAPPED, and an
//!   overlapped write on a synchronous handle is the one combination
//!   Windows documents as able to "incorrectly report that the write
//!   operation is complete" (observed in the field: reported success, zero
//!   bytes delivered). A broken pipe fails the write immediately, and the
//!   lease/heartbeat watchdogs bound any stall.
//! - heartbeat: watchdogs (auth before rx), lease-expiry release, retired
//!   handle reaping, and the periodic stats heartbeat.
//!
//! Disconnect detection does NOT rely on read/write errors (unreliable on
//! pipes); the watchdogs use "no downlink bytes" and "no renew ever" as the
//! sole liveness signals.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::Threading::GetCurrentProcessId;

use rc_protocol::agent_wire::{encode_agent_line, AgentToHelper};
use rc_protocol::consts::{AGENT_RECONNECT_MS, MAX_LINE_BYTES, PIPE_IDLE_BACKOFF_MS};

use crate::clock::{mono_ms, unix_ms};
use crate::runtime::{lock_ok, RawHandle, Runtime, RUNTIME};

/// Win32 GENERIC_READ | GENERIC_WRITE for the duplex pipe handle.
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
/// Outbound backlog cap: after this many queued
/// lines the newest is dropped and counted rather than growing unbounded.
const TX_QUEUE_CAP: usize = 512;
/// Retired handles are closed only after this long, comfortably past any
/// reasonable drain window, so a handle is never closed under an in-flight
/// write.
const RETIRE_CLOSE_MS: u64 = 2500;
/// Reader poll granularity while waiting for the next allowed reconnect.
const RETRY_POLL_MS: u64 = 100;

// ---------------------------------------------------------------- outbound

#[derive(Default)]
pub struct Outbound {
    q: Mutex<VecDeque<String>>,
    cv: Condvar,
}

impl Outbound {
    fn push(&self, line: String) -> bool {
        let mut q = lock_ok(&self.q);
        if q.len() >= TX_QUEUE_CAP {
            return false; // drop the newest, keep already-queued acks ordered
        }
        q.push_back(line);
        drop(q);
        self.cv.notify_one();
        true
    }

    fn pop_wait(&self, timeout: Duration) -> Option<String> {
        let mut q = lock_ok(&self.q);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = q.pop_front() {
                return Some(line);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(q, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            q = guard;
        }
    }
}

/// The single uplink exit. Fails soft: when there is no runtime yet, or no
/// connection, or the queue is full, the message is counted (send_dropped)
/// and dropped — never blocks, never panics. Log payloads are truncated.
pub fn enqueue_agent(msg: &AgentToHelper) {
    let Some(rt) = RUNTIME.get() else {
        return;
    };
    let mut msg = msg.clone();
    if let AgentToHelper::Log { msg: m, .. } = &mut msg {
        *m = m.chars().take(400).collect();
    }
    let line = encode_agent_line(&msg);
    {
        let mut core = lock_ok(&rt.core);
        if !core.connected {
            // Counted separately from cmd_rejected: this is the ordinary
            // reconnect window, not a security event.
            core.stats.send_dropped += 1;
            return;
        }
    }
    if !rt.out.push(line) {
        lock_ok(&rt.core).stats.send_dropped += 1;
    }
}

// ----------------------------------------------------------------- threads

/// Spawn reader, writer and heartbeat threads. Called once from boot.
pub fn spawn_threads(rt: &'static Runtime) {
    let _ = thread::Builder::new()
        .name("rc-agent-rx".into())
        .spawn(move || unsafe { reader_loop(rt) });
    let _ = thread::Builder::new()
        .name("rc-agent-tx".into())
        .spawn(move || unsafe { writer_loop(rt) });
    let _ = thread::Builder::new()
        .name("rc-agent-hb".into())
        .spawn(move || unsafe { heartbeat_loop(rt) });
}

// ------------------------------------------------------------------ reader

unsafe fn reader_loop(rt: &'static Runtime) {
    loop {
        let can_retry = {
            let core = lock_ok(&rt.core);
            core.can_retry_connect(mono_ms())
        };
        if !can_retry {
            thread::sleep(Duration::from_millis(RETRY_POLL_MS));
            continue;
        }
        let Some(h) = connect_pipe(&rt.pipe_name) else {
            // Plain connect failure: nothing to count (no contract field),
            // wait one reconnect interval and try again.
            thread::sleep(Duration::from_millis(u64::from(AGENT_RECONNECT_MS)));
            continue;
        };
        // Store the handle first so the writer can see it, then flip the
        // lease-relevant connected flag, then say hello.
        *lock_ok(&rt.conn) = Some(RawHandle(h));
        let hello = {
            let mut core = lock_ok(&rt.core);
            core.on_connected(mono_ms());
            core.hello_msg(GetCurrentProcessId(), "x64", unix_ms())
        };
        enqueue_agent(&hello);
        read_loop(rt, h);
    }
}

unsafe fn read_loop(rt: &'static Runtime, h: HANDLE) {
    let mut buf = [0u8; MAX_LINE_BYTES];
    let mut acc: Vec<u8> = Vec::with_capacity(MAX_LINE_BYTES);
    loop {
        let mut avail = 0u32;
        if PeekNamedPipe(h, None, 0, None, Some(&mut avail), None).is_err() {
            fail(rt, "read_error");
            return;
        }
        if avail == 0 {
            // Liveness comes from the heartbeat watchdogs, not from read
            // errors; idle backoff keeps this loop cheap.
            thread::sleep(Duration::from_millis(PIPE_IDLE_BACKOFF_MS));
            continue;
        }
        let mut n = 0u32;
        if ReadFile(h, Some(&mut buf), Some(&mut n), None).is_err() {
            fail(rt, "read_error");
            return;
        }
        if n == 0 {
            continue;
        }
        lock_ok(&rt.core).mark_rx(mono_ms());
        acc.extend_from_slice(&buf[..n as usize]);
        if acc.len() > MAX_LINE_BYTES {
            // One line longer than the protocol cap: the peer is broken or
            // hostile; drop the connection.
            lock_ok(&rt.core).stats.read_errors += 1;
            drop_connection(rt, "line_overflow", false);
            return;
        }
        while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = acc.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            if line.trim().is_empty() {
                continue;
            }
            let msgs = {
                let mut core = lock_ok(&rt.core);
                core.handle_line(&line, mono_ms(), unix_ms())
            };
            for m in &msgs {
                enqueue_agent(m);
            }
        }
    }
}

unsafe fn fail(rt: &'static Runtime, reason: &str) {
    lock_ok(&rt.core).stats.read_errors += 1;
    drop_connection(rt, reason, false);
}

unsafe fn connect_pipe(name: &str) -> Option<HANDLE> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    CreateFileW(
        PCWSTR(wide.as_ptr()),
        GENERIC_READ_WRITE,
        FILE_SHARE_MODE(0),
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .ok()
}

// ------------------------------------------------------------------ writer

unsafe fn writer_loop(rt: &'static Runtime) {
    loop {
        let Some(line) = rt.out.pop_wait(Duration::from_millis(250)) else {
            continue;
        };
        let h: Option<HANDLE> = {
            let guard = lock_ok(&rt.conn);
            guard.as_ref().map(|raw| raw.0)
        };
        let Some(h) = h else {
            lock_ok(&rt.core).stats.send_dropped += 1;
            continue;
        };
        match write_line(h, line.as_bytes()) {
            Ok(()) => {}
            Err(_) => {
                lock_ok(&rt.core).stats.errors += 1;
                drop_connection(rt, "write_error", false);
            }
        }
    }
}

/// One synchronous write. The connection handle is opened WITHOUT
/// FILE_FLAG_OVERLAPPED (see `connect_pipe`), so the write must be
/// synchronous too: an OVERLAPPED write on a synchronous handle is the one
/// combination Windows documents as "can incorrectly report that the write
/// operation is complete" — observed in the field as writes that report
/// success while zero bytes reach the pipe (the helper then times out
/// waiting for a hello that never arrives). A broken pipe fails fast here;
/// a full pipe cannot stall because the helper's session loop always
/// drains, and the lease/heartbeat watchdogs bound any wedge anyway.
unsafe fn write_line(h: HANDLE, bytes: &[u8]) -> Result<(), ()> {
    let mut n = 0u32;
    match WriteFile(h, Some(bytes), Some(&mut n), None) {
        Ok(()) if n as usize == bytes.len() => Ok(()),
        _ => Err(()),
    }
}

// --------------------------------------------------------------- heartbeat

unsafe fn heartbeat_loop(rt: &'static Runtime) {
    let hb_ms = {
        let core = lock_ok(&rt.core);
        core.hb_ms
    };
    loop {
        thread::sleep(Duration::from_millis(u64::from(hb_ms)));

        let mut msgs: Vec<AgentToHelper> = Vec::new();
        let mut pending_drop: Option<(&'static str, bool)> = None;
        {
            let mut core = lock_ok(&rt.core);
            // Fold the lock-free ioctl counter into the reported stats.
            core.stats.ioctl_calls =
                crate::hook::HW_IOCTL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            if let Some((reason, slow)) = core.watchdog_hit(mono_ms()) {
                match reason {
                    "auth_mismatch" => core.stats.auth_rejected += 1,
                    _ => core.stats.rx_timeouts += 1,
                }
                pending_drop = Some((reason, slow));
            }
            if let Some(edge) = core.lease_tick(mono_ms(), unix_ms()) {
                msgs.push(edge);
            }
        }
        if let Some((reason, slow)) = pending_drop {
            drop_connection(rt, reason, slow);
        }
        close_retired_handles(rt);

        let hb = {
            let core = lock_ok(&rt.core);
            core.heartbeat_msg(mono_ms(), unix_ms())
        };
        msgs.push(hb);
        for m in &msgs {
            enqueue_agent(m);
        }
    }
}

unsafe fn close_retired_handles(rt: &'static Runtime) {
    let now = mono_ms();
    let mut retired = lock_ok(&rt.retired);
    retired.retain(|(h, at)| {
        if now.saturating_sub(*at) > RETIRE_CLOSE_MS {
            let _ = CloseHandle(h.0);
            false
        } else {
            true
        }
    });
}

// ------------------------------------------------------------------- drops

/// Judge the current connection dead: say `dropped:` while still connected,
/// flip the state (backoff included), retire the handle for a delayed close.
pub fn drop_connection(rt: &'static Runtime, reason: &str, slow: bool) {
    {
        let core = lock_ok(&rt.core);
        if !core.connected {
            return; // already judged dead by another path
        }
    }
    // Emitted before the flip so it can actually leave on the live link.
    enqueue_agent(&AgentToHelper::Log {
        t: unix_ms(),
        msg: format!("dropped:{reason}"),
    });
    {
        let mut core = lock_ok(&rt.core);
        core.drop_state(reason, slow, mono_ms());
    }
    if let Some(h) = lock_ok(&rt.conn).take() {
        lock_ok(&rt.retired).push((h, mono_ms()));
    }
}

/// Count one detour-level unexpected failure (panic / API failure).
pub fn count_error() {
    if let Some(rt) = RUNTIME.get() {
        lock_ok(&rt.core).stats.errors += 1;
    }
}
