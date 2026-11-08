//! Named-pipe transport: one JSON line per frame in both directions, three
//! threads total.
//!
//! - reader: owns reconnection. Connects to the pipe named in the boot
//!   config, feeds decoded `HelperToAgent` lines into the state machine and
//!   ships back produced messages. Idle reads poll with `PeekNamedPipe` and
//!   the shared 2 ms idle backoff, so the rx watchdog stays meaningful.
//! - writer: the ONLY writer, fed by the outbound queue (single serialized
//!   writer — the reference lost whole message classes to concurrent writes).
//!   Writes are overlapped with a timeout plus `CancelIoEx`, so a stuck peer
//!   can never wedge this thread.
//! - heartbeat: watchdogs (auth before rx), lease-expiry release, retired
//!   handle reaping, and the periodic stats heartbeat.
//!
//! Disconnect detection does NOT rely on read/write errors (unreliable on
//! pipes); the watchdogs use "no downlink bytes" and "no renew ever" exactly
//! like the reference.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, HRESULT};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, ResetEvent, WaitForSingleObject,
};

use rc_protocol::agent_wire::{encode_agent_line, AgentToHelper};
use rc_protocol::consts::{AGENT_RECONNECT_MS, MAX_LINE_BYTES, PIPE_IDLE_BACKOFF_MS};

use crate::clock::{mono_ms, unix_ms};
use crate::runtime::{lock_ok, RawHandle, Runtime, RUNTIME};

/// Win32 GENERIC_READ | GENERIC_WRITE for the duplex pipe handle.
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
/// Outbound backlog cap (reference TX_QUEUE_MAX): after this many queued
/// lines the newest is dropped and counted rather than growing unbounded.
const TX_QUEUE_CAP: usize = 512;
/// A single serialized write may take at most this long before it is
/// cancelled and the connection is treated as dead.
const WRITE_TIMEOUT_MS: u32 = 1000;
/// Retired handles are closed only after this long, comfortably past any
/// writer's WRITE_TIMEOUT + cancel window, so a handle is never closed under
/// an in-flight overlapped write.
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
    // One manual-reset event reused for every overlapped write of this
    // (single) writer thread.
    let Ok(ev) = CreateEventW(None, true, false, PCWSTR::null()) else {
        return;
    };
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
        match write_line(h, ev, line.as_bytes()) {
            Ok(()) => {}
            Err(()) => {
                lock_ok(&rt.core).stats.errors += 1;
                drop_connection(rt, "write_error", false);
            }
        }
    }
}

/// One serialized overlapped write with a hard timeout. Ok only when the
/// bytes were actually flushed to the pipe.
unsafe fn write_line(h: HANDLE, ev: HANDLE, bytes: &[u8]) -> Result<(), ()> {
    let _ = ResetEvent(ev);
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;
    let mut n = 0u32;
    match WriteFile(h, Some(bytes), Some(&mut n), Some(&mut ov)) {
        Ok(()) => Ok(()),
        // ERROR_IO_PENDING as HRESULT_FROM_WIN32(997): the write is queued on
        // the event; wait for completion or cancel it on timeout.
        Err(e) if e.code() == HRESULT::from_win32(ERROR_IO_PENDING.0) => {
            if WaitForSingleObject(ev, WRITE_TIMEOUT_MS) == WAIT_OBJECT_0 {
                GetOverlappedResult(h, &ov, &mut n, false).map(|_| ()).map_err(|_| ())
            } else {
                // Peer is not draining: cancel our own I/O so this thread
                // can never wedge on a full pipe.
                let _ = CancelIoEx(h, None);
                let _ = WaitForSingleObject(ev, WRITE_TIMEOUT_MS);
                Err(())
            }
        }
        Err(_) => Err(()),
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
            core.stats.ioctl_calls = crate::hook::HW_IOCTL_CALLS
                .load(std::sync::atomic::Ordering::Relaxed);
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
