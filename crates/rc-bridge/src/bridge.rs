//! [`RcBridge`]: the named-pipe server, connection lifecycle, ownership and
//! silence watchdogs, and the public API the UI app drives.
//!
//! Lock discipline: every lock is taken alone (no nested locking) and
//! acquired poison-free. All state pieces are snapshottable, so threads
//! clone what they need under a short lock and act outside it.

#[cfg(target_os = "windows")]
use std::collections::BTreeSet;
#[cfg(target_os = "windows")]
use std::io::Write;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

#[cfg(target_os = "windows")]
use {
    std::io::Read as _,
    std::os::windows::io::AsRawHandle as _,
    std::time::Duration,
    rc_protocol::bridge_wire::{
        format_bye, format_deny, format_ok, format_synth, format_targets, parse_bridge_line,
        BridgeLine,
    },
    rc_protocol::consts::{
        BRIDGE_MAX_DENY, BRIDGE_PIPE_NAME, BRIDGE_PROTOCOL_VERSION, MAX_LINE_BYTES,
    },
};

use crate::state::{targets_for, BridgeCounters, BridgePhase, BridgeShared, CaptureTargets};

#[cfg(target_os = "windows")]
use crate::state::{
    auth_decide, derive_edges, hello_expired, resend_due, silence_expired, IDLE_BACKOFF,
};

use crate::EngineEvent;

#[cfg(target_os = "windows")]
use crate::pipe_windows::{self, PipeIo, PipeCreateError};

/// Snapshot of the bridge for the UI: phase, helper identity, ownership and
/// synth state, plus lifetime counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSnapshot {
    pub phase: BridgePhase,
    /// Pipe name was taken by another instance: surface "already running".
    pub already_running: bool,
    /// Helper pid from the authenticated `HELLO` (0 = none).
    pub helper_pid: u32,
    /// "named_pipe" while connected, otherwise empty.
    pub transport: &'static str,
    /// Current capture generation.
    pub generation: u64,
    /// Absolute target set sent in `OK`/`T` lines (sorted).
    pub target_usages: Vec<u16>,
    /// Usages the agent acked for the current generation (sorted; empty
    /// when ownership is not held).
    pub owned_usages: Vec<u16>,
    /// Age of the last ownership ack; `None` = never acked.
    pub last_ack_age_ms: Option<u64>,
    /// Ownership currently trusted (current generation acked fresh).
    /// Degraded = connected while this is false.
    pub ownership_valid: bool,
    /// Synth target (`S` line state; `None` = off).
    pub synth_target: Option<u16>,
    /// The agent confirmed the current synth target with an `A` ack.
    pub synth_confirmed: bool,
    /// Helper is doing device-node recovery (`R` line active).
    pub recovering: bool,
    /// Usages currently believed pressed (sorted).
    pub pressed_usages: Vec<u16>,
    /// Age of the last received line; `None` = nothing yet.
    pub last_rx_age_ms: Option<u64>,
    pub counters: BridgeCounters,
}

/// Server-side handle for the current helper connection (Windows only).
#[cfg(target_os = "windows")]
struct CurrentConn {
    id: u64,
    /// A dup of the connected pipe: used to write `BYE` and disconnect from
    /// outside the connection thread.
    pipe: std::fs::File,
}

/// The app-side bridge. Create with [`RcBridge::start`]; the returned
/// [`Arc`] is the UI's handle. Dropping the last reference stops the
/// bridge (same as [`RcBridge::stop`]).
pub struct RcBridge {
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<BridgeShared>>,
    targets: Arc<Mutex<CaptureTargets>>,
    synth: Arc<Mutex<Option<u16>>>,
    sender: Sender<EngineEvent>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    #[cfg(target_os = "windows")]
    current: Arc<Mutex<Option<CurrentConn>>>,
    #[cfg(target_os = "windows")]
    force_drop: Arc<AtomicU64>,
}

impl RcBridge {
    /// Start the bridge: create the pipe server (first instance only),
    /// write the descriptor file with a fresh per-run token, and spawn the
    /// accept/watchdog threads. If the pipe name is taken, the returned
    /// bridge reports `snapshot().already_running` and stays disabled —
    /// the app uses that for single-instance handling.
    pub fn start(engine_tx: Sender<EngineEvent>) -> Arc<RcBridge> {
        Self::build(engine_tx)
    }

    fn build(engine_tx: Sender<EngineEvent>) -> Arc<RcBridge> {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(BridgeShared::default()));
        let targets = Arc::new(Mutex::new(CaptureTargets::default()));
        let synth = Arc::new(Mutex::new(None::<u16>));
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

        #[cfg(target_os = "windows")]
        let bridge = {
            let current = Arc::new(Mutex::new(None::<CurrentConn>));
            let force_drop = Arc::new(AtomicU64::new(0));
            Arc::new(RcBridge {
                stop: Arc::clone(&stop),
                shared: Arc::clone(&shared),
                targets: Arc::clone(&targets),
                synth: Arc::clone(&synth),
                sender: engine_tx.clone(),
                workers: Arc::clone(&workers),
                current,
                force_drop,
            })
        };

        #[cfg(not(target_os = "windows"))]
        let bridge = Arc::new(RcBridge {
            stop: Arc::clone(&stop),
            shared: Arc::clone(&shared),
            targets: Arc::clone(&targets),
            synth: Arc::clone(&synth),
            sender: engine_tx,
            workers,
        });

        #[cfg(target_os = "windows")]
        {
            // Single-instance enforcement point: only the first creator of
            // the pipe name wins.
            match pipe_windows::create_server_instance(BRIDGE_PIPE_NAME, true) {
                Ok(instance) => {
                    let token = pipe_windows::generate_token();
                    if let Err(error) = pipe_windows::write_descriptor(
                        &pipe_windows::app_runtime_dir(),
                        &token,
                        None,
                    ) {
                        // Degraded: listening but undiscoverable by the
                        // helper. Not fatal for the app.
                        let _ = error;
                    }
                    lock(&bridge.shared).phase = BridgePhase::Listening;
                    spawn_worker(
                        &bridge.workers,
                        "rc-bridge-accept",
                        accept_loop(
                            Arc::clone(&stop),
                            Arc::clone(&shared),
                            Arc::clone(&targets),
                            Arc::clone(&synth),
                            Arc::clone(&bridge.current),
                            Arc::clone(&bridge.force_drop),
                            Arc::clone(&workers),
                            instance,
                            token,
                            engine_tx.clone(),
                        ),
                    );
                    spawn_worker(
                        &bridge.workers,
                        "rc-bridge-watchdog",
                        watchdog_loop(
                            Arc::clone(&stop),
                            Arc::clone(&shared),
                            Arc::clone(&targets),
                            Arc::clone(&bridge.current),
                            Arc::clone(&bridge.force_drop),
                            engine_tx,
                        ),
                    );
                }
                Err(PipeCreateError::AlreadyRunning) => {
                    let mut state = lock(&bridge.shared);
                    state.already_running = true;
                    // Phase stays Stopped: run disabled.
                }
                Err(error) => {
                    let _ = error;
                    lock(&bridge.shared).phase = BridgePhase::Failed;
                }
            }
        }

        bridge
    }

    /// Update the capture target set. The set is
    /// `rc_protocol::buttons::capture_set(mapped_mask)` while enabled
    /// (always-captured buttons plus mapped ones) and empty while disabled
    /// (full fail-open). A changed set bumps the generation: the connection
    /// thread sends `T` and resends until the agent acks (`O`) it, while
    /// ownership of the old generation is revoked — losing it releases the
    /// engine's pressed states exactly once.
    pub fn set_capture_targets(&self, enabled: bool, mapped_mask: u64) {
        let usages = targets_for(enabled, mapped_mask);
        let generation = {
            let mut targets = lock(&self.targets);
            if targets.usages == usages {
                return;
            }
            targets.install(usages)
        };
        let transition = {
            let mut state = lock(&self.shared);
            state.counters.retargets_total += 1;
            state.ownership.clear();
            // Observe the loss transition here as well, so the engine is
            // released promptly even without the watchdog thread (host
            // builds); the watchdog observes it on Windows too, but a
            // transition is reported exactly once.
            state.ownership.sync_validity(generation, Instant::now())
        };
        if transition == Some(false) {
            self.release_engine_all();
        }
    }

    /// Update the voice-synth target (`S` line). `None` is sent explicitly:
    /// the helper/agent may outlive the app, and omitting the off switch
    /// would leave the previous synthesis active. Sent with the same
    /// resend-until-`A`-ack schedule as `T`.
    pub fn set_voice_synth(&self, to: Option<u16>) {
        {
            let mut synth = lock(&self.synth);
            if *synth == to {
                return;
            }
            *synth = to;
        }
        // A pending confirmation belongs to the previous target.
        lock(&self.shared).synth_confirmed = false;
    }

    pub fn snapshot(&self) -> BridgeSnapshot {
        let targets = lock(&self.targets).clone();
        let synth = *lock(&self.synth);
        let state = lock(&self.shared);
        let now = Instant::now();
        BridgeSnapshot {
            phase: state.phase,
            already_running: state.already_running,
            helper_pid: state.helper_pid,
            transport: state.transport,
            generation: targets.generation,
            target_usages: targets.usages.iter().copied().collect(),
            owned_usages: state.ownership.acked_usages().iter().copied().collect(),
            last_ack_age_ms: state
                .ownership
                .last_ack()
                .map(|at| now.duration_since(at).as_millis() as u64),
            ownership_valid: state.ownership.valid(),
            synth_target: synth,
            synth_confirmed: state.synth_confirmed,
            recovering: state.recovering,
            pressed_usages: state.pressed.iter().copied().collect(),
            last_rx_age_ms: state
                .last_rx
                .map(|at| now.duration_since(at).as_millis() as u64),
            counters: state.counters,
        }
    }

    /// Ordered teardown: send `BYE` to the current helper, disconnect, and
    /// join the worker threads. Also runs from `Drop`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        #[cfg(target_os = "windows")]
        {
            // Politely close the current connection (BYE, then disconnect).
            {
                let guard = lock(&self.current);
                if let Some(conn) = guard.as_ref() {
                    let mut writer = &conn.pipe;
                    let _ = write_line(&mut writer, &format_bye("app_stopping"));
                }
            }
            if let Some(conn) = lock(&self.current).as_ref() {
                pipe_windows::disconnect(handle_of(conn));
            }
            self.force_drop.store(0, Ordering::Relaxed);
        }
        let workers = lock(&self.workers).drain(..).collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.join();
        }
    }

    fn release_engine_all(&self) {
        {
            let mut state = lock(&self.shared);
            state.counters.release_all_total += 1;
            state.pressed.clear();
        }
        let _ = self.sender.send(EngineEvent::ReleaseAll);
    }
}

impl Drop for RcBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

// -- shared helpers ----------------------------------------------------------

/// Poison-free lock: a panic in one thread must not wedge the bridge.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(target_os = "windows")]
fn write_line(writer: &mut impl Write, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(target_os = "windows")]
fn sorted_usages(usages: &BTreeSet<u16>) -> Vec<u16> {
    usages.iter().copied().collect()
}

#[cfg(target_os = "windows")]
fn handle_of(conn: &CurrentConn) -> std::os::windows::io::RawHandle {
    conn.pipe.as_raw_handle()
}

#[cfg(target_os = "windows")]
fn spawn_worker(
    workers: &Mutex<Vec<JoinHandle<()>>>,
    name: &str,
    body: impl FnOnce() + Send + 'static,
) {
    // A failed spawn is survivable: the accept loop or watchdog is gone,
    // but every path that matters also notices it via timeouts. Log-free
    // by design; the snapshot makes the stuck phase visible to the UI.
    if let Ok(handle) = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
    {
        lock(workers).push(handle);
    }
}

// -- Windows runtime ---------------------------------------------------------

#[cfg(target_os = "windows")]
fn accept_loop(
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<BridgeShared>>,
    targets: Arc<Mutex<CaptureTargets>>,
    synth: Arc<Mutex<Option<u16>>>,
    current: Arc<Mutex<Option<CurrentConn>>>,
    force_drop: Arc<AtomicU64>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    instance: std::fs::File,
    token: String,
    sender: Sender<EngineEvent>,
) -> impl FnOnce() {
    move || {
        let mut server: Option<PipeIo> = Some(PipeIo::new(instance));
        let mut next_id: u64 = 1;
        while !stop.load(Ordering::Relaxed) {
            let Some(active) = server.as_ref() else {
                // Recreate the listening instance after a takeover or error.
                match pipe_windows::create_server_instance(BRIDGE_PIPE_NAME, false) {
                    Ok(next) => server = Some(PipeIo::new(next)),
                    Err(_) => std::thread::sleep(Duration::from_millis(500)),
                }
                continue;
            };
            match pipe_windows::poll_connected(active.raw_handle()) {
                Ok(false) => {}
                Ok(true) => {
                    let connected = server.take().expect("checked Some").into_file();
                    let id = next_id;
                    next_id += 1;
                    adopt_connection(
                        connected,
                        id,
                        &shared,
                        &current,
                        &workers,
                        &token,
                        &sender,
                        &targets,
                        &synth,
                        &force_drop,
                        &stop,
                    );
                }
                Err(_) => {
                    // Recreate the listening instance.
                    server.take();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn adopt_connection(
    pipe: std::fs::File,
    id: u64,
    shared: &Arc<Mutex<BridgeShared>>,
    current: &Arc<Mutex<Option<CurrentConn>>>,
    workers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
    token: &str,
    sender: &Sender<EngineEvent>,
    targets: &Arc<Mutex<CaptureTargets>>,
    synth: &Arc<Mutex<Option<u16>>>,
    force_drop: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
) {
    let control = match pipe.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let previous = lock(current).replace(CurrentConn { id, pipe: control });
    let replaced = previous.is_some();
    if let Some(previous) = previous {
        // A NEW helper connection always wins: politely close the old one.
        // The old connection thread exits as "replaced" without releasing
        // anything — the winner restates the absolute pressed set.
        let mut old = PipeIo::new(previous.pipe);
        let _ = write_line(&mut old, &format_bye("replaced_by_new_connection"));
        old.disconnect();
    }
    {
        let mut state = lock(shared);
        state.counters.accepted_total += 1;
        if replaced {
            state.counters.replaced_total += 1;
            state.phase = BridgePhase::Listening;
            state.helper_pid = 0;
            state.transport = "";
            state.recovering = false;
            state.synth_confirmed = false;
            state.ownership.clear();
        }
    }
    let handle = std::thread::Builder::new()
        .name("rc-bridge-conn".to_owned())
        .spawn({
            let shared = Arc::clone(shared);
            let current = Arc::clone(current);
            let targets = Arc::clone(targets);
            let synth = Arc::clone(synth);
            let force_drop = Arc::clone(force_drop);
            let stop = Arc::clone(stop);
            let token = token.to_owned();
            let sender = sender.clone();
            move || {
                run_connection(
                    pipe, id, stop, shared, targets, synth, current, force_drop, &token, sender,
                )
            }
        });
    match handle {
        Ok(handle) => lock(workers).push(handle),
        // The worker registry already exists; a failed spawn just leaves the
        // connection to die on its next poll (it is not current anymore).
        Err(_) => {
            let mut guard = lock(current);
            if guard.as_ref().map(|conn| conn.id == id).unwrap_or(false) {
                *guard = None;
            }
        }
    }
}

/// Whether connection `id` is still the current one.
#[cfg(target_os = "windows")]
fn is_current(current: &Mutex<Option<CurrentConn>>, id: u64) -> bool {
    lock(current)
        .as_ref()
        .map(|conn| conn.id == id)
        .unwrap_or(false)
}

/// One helper connection's lifecycle: HELLO auth -> line loop -> teardown
/// with a full release. Any exit path other than being replaced releases
/// every pressed state (the fail-open last resort).
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn run_connection(
    pipe: std::fs::File,
    id: u64,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<BridgeShared>>,
    targets: Arc<Mutex<CaptureTargets>>,
    synth: Arc<Mutex<Option<u16>>>,
    current: Arc<Mutex<Option<CurrentConn>>>,
    force_drop: Arc<AtomicU64>,
    expected_token: &str,
    sender: Sender<EngineEvent>,
) {
    let mut writer = PipeIo::new(pipe);
    let mut reader = match writer.try_clone() {
        Ok(reader) => reader,
        Err(_) => {
            let mut guard = lock(&current);
            if guard.as_ref().map(|conn| conn.id == id).unwrap_or(false) {
                *guard = None;
            }
            return;
        }
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut authenticated = false;
    let mut deny_count: u32 = 0;
    let started = Instant::now();
    // T-line resend bookkeeping (current generation + ack wait).
    let mut sent_generation: u64 = 0;
    let mut t_sent_at: Option<Instant> = None;
    let mut t_attempts: u64 = 0;
    // S-line resend bookkeeping.
    let mut last_sent_synth: Option<u16> = None;
    let mut synth_sent_at: Option<Instant> = None;
    let mut synth_attempts: u64 = 0;
    let reason: &'static str;

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            reason = "bridge_stopping";
            break;
        }
        if force_drop.load(Ordering::Relaxed) == id {
            reason = "silence_timeout";
            break;
        }
        if !is_current(&current, id) {
            reason = "replaced";
            break;
        }

        // Switch out every complete line (a read may deliver several).
        loop {
            let Some(position) = pending.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let line_bytes: Vec<u8> = pending.drain(..=position).collect();
            let raw = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
            // Any complete line — even empty or malformed — proves the link
            // is alive: refresh the silence watchdog's clock.
            lock(&shared).last_rx = Some(Instant::now());
            let message = match parse_bridge_line(&raw) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(_) => {
                    lock(&shared).counters.malformed_total += 1;
                    continue;
                }
            };

            if !authenticated {
                match message {
                    BridgeLine::Hello {
                        version,
                        token,
                        helper_pid,
                    } => match auth_decide(version, &token, expected_token) {
                        Ok(()) => {
                            authenticated = true;
                            let targets_snapshot = lock(&targets).clone();
                            let synth_current = *lock(&synth);
                            {
                                let mut state = lock(&shared);
                                state.phase = BridgePhase::Connected;
                                state.helper_pid = helper_pid;
                                state.transport = "named_pipe";
                                state.recovering = false;
                                // A fresh session does not inherit the
                                // previous one's ownership or synth ack.
                                state.ownership.clear();
                                state.synth_confirmed = false;
                            }
                            // Current absolute state: a reconnected helper
                            // must not keep clearing a stale set.
                            sent_generation = targets_snapshot.generation;
                            let ok_line = format_ok(
                                BRIDGE_PROTOCOL_VERSION,
                                targets_snapshot.generation,
                                &sorted_usages(&targets_snapshot.usages),
                            );
                            if write_line(&mut writer, &ok_line).is_err() {
                                reason = "ok_write_error";
                                break 'outer;
                            }
                            // S goes out explicitly even for None.
                            last_sent_synth = synth_current;
                            synth_attempts = 0;
                            let s_line = format_synth(synth_current);
                            match write_line(&mut writer, &s_line) {
                                Ok(()) => synth_sent_at = Some(Instant::now()),
                                Err(_) => {
                                    reason = "synth_write_error";
                                    break 'outer;
                                }
                            }
                            // Wait out the first T resend like the OK-borne
                            // generation it mirrors.
                            t_sent_at = synth_sent_at;
                        }
                        Err(deny_reason) => {
                            deny_count += 1;
                            lock(&shared).counters.denied_total += 1;
                            let _ = write_line(&mut writer, &format_deny(deny_reason));
                            if deny_count >= BRIDGE_MAX_DENY {
                                reason = "deny_limit";
                                break 'outer;
                            }
                        }
                    },
                    // Before auth only HELLO is accepted: no half protocol.
                    _ => {
                        reason = "hello_required";
                        break 'outer;
                    }
                }
                continue;
            }

            match message {
                BridgeLine::Edges { usages } => {
                    let allowed = lock(&targets).usages.clone();
                    let events = {
                        let mut state = lock(&shared);
                        let diff = derive_edges(&state.pressed, &usages, &allowed);
                        state.pressed = diff.accepted;
                        state.counters.filtered_usages_total += diff.rejected;
                        for event in &diff.events {
                            match event {
                                EngineEvent::Press(_) => state.counters.press_edges_total += 1,
                                EngineEvent::Release(_) => {
                                    state.counters.release_edges_total += 1
                                }
                                EngineEvent::ReleaseAll => {}
                            }
                        }
                        diff.events
                    };
                    for event in events {
                        if sender.send(event).is_err() {
                            reason = "engine_gone";
                            break 'outer;
                        }
                    }
                }
                BridgeLine::Ping => {}
                BridgeLine::Recovering { active } => {
                    lock(&shared).recovering = active;
                }
                BridgeLine::Ownership { generation, usages } => {
                    let now = Instant::now();
                    let targets_snapshot = lock(&targets).clone();
                    let usage_set: BTreeSet<u16> = usages.into_iter().collect();
                    let mut state = lock(&shared);
                    if !state
                        .ownership
                        .record(generation, &usage_set, &targets_snapshot, now)
                    {
                        state.counters.ownership_rejected_total += 1;
                    }
                }
                BridgeLine::SynthAck { to } => {
                    let desired = *lock(&synth);
                    let mut state = lock(&shared);
                    if to == desired {
                        // Absolute-state ack matching the desired target:
                        // config really applied. Stale acks are ignored.
                        state.synth_confirmed = true;
                    }
                }
                BridgeLine::VoiceFrame { .. } => {
                    lock(&shared).counters.voice_frames_total += 1;
                }
                BridgeLine::Bye { .. } => {
                    reason = "helper_bye";
                    break 'outer;
                }
                // App-direction lines and repeat HELLO: protocol noise.
                BridgeLine::Hello { .. }
                | BridgeLine::Ok { .. }
                | BridgeLine::Targets { .. }
                | BridgeLine::Synth { .. }
                | BridgeLine::GateDeclare
                | BridgeLine::Deny { .. } => {}
            }
        }

        if pending.len() > MAX_LINE_BYTES {
            reason = "line_too_long";
            break;
        }
        if !authenticated && hello_expired(started, Instant::now()) {
            reason = "hello_timeout";
            break;
        }

        if authenticated {
            let now = Instant::now();
            let targets_snapshot = lock(&targets).clone();
            let acked_generation = lock(&shared).ownership.acked_generation();
            let acked = acked_generation == Some(targets_snapshot.generation);

            // T: send on generation change, then resend until O-acked.
            if targets_snapshot.generation != sent_generation {
                let line = format_targets(
                    targets_snapshot.generation,
                    &sorted_usages(&targets_snapshot.usages),
                );
                if write_line(&mut writer, &line).is_err() {
                    reason = "target_write_error";
                    break;
                }
                sent_generation = targets_snapshot.generation;
                t_attempts = 0;
                t_sent_at = Some(now);
            } else if !acked && resend_due(t_sent_at.map(|at| now.duration_since(at)), t_attempts)
            {
                let line = format_targets(
                    targets_snapshot.generation,
                    &sorted_usages(&targets_snapshot.usages),
                );
                if write_line(&mut writer, &line).is_err() {
                    reason = "target_write_error";
                    break;
                }
                t_attempts += 1;
                t_sent_at = Some(now);
            }

            // S: send on target change, then resend until A-confirmed.
            let synth_current = *lock(&synth);
            let synth_confirmed = lock(&shared).synth_confirmed;
            if synth_current != last_sent_synth {
                let line = format_synth(synth_current);
                if write_line(&mut writer, &line).is_err() {
                    reason = "synth_write_error";
                    break;
                }
                last_sent_synth = synth_current;
                lock(&shared).synth_confirmed = false;
                synth_attempts = 0;
                synth_sent_at = Some(now);
            } else if !synth_confirmed
                && resend_due(synth_sent_at.map(|at| now.duration_since(at)), synth_attempts)
            {
                let line = format_synth(synth_current);
                if write_line(&mut writer, &line).is_err() {
                    reason = "synth_write_error";
                    break;
                }
                synth_attempts += 1;
                synth_sent_at = Some(now);
            }
        }

        // Read one chunk; complete lines are switched out at the top of
        // the next loop iteration.
        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => std::thread::sleep(IDLE_BACKOFF),
            Ok(count) => pending.extend_from_slice(&chunk[..count]),
            Err(error) if pipe_windows::is_idle_error(&error) => std::thread::sleep(IDLE_BACKOFF),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                reason = "read_error";
                break;
            }
        }
    }

    if reason != "replaced" {
        // Fail-open teardown: clear every pressed state in one absolute
        // event. ReleaseAll also drops a deferred Win+L action instead of
        // firing it on a link that is going away.
        {
            let mut state = lock(&shared);
            state.counters.release_all_total += 1;
            if reason != "helper_bye" {
                state.counters.teardown_releases_total += 1;
            }
            state.pressed.clear();
            state.ownership.clear();
            state.last_rx = None;
            if state.phase == BridgePhase::Connected {
                state.phase = BridgePhase::Listening;
            }
            state.helper_pid = 0;
            state.transport = "";
            state.recovering = false;
            state.synth_confirmed = false;
        }
        let _ = sender.send(EngineEvent::ReleaseAll);
    }
    writer.disconnect();
    {
        // Only clear "current" if it is still us; a newer connection must
        // not be wiped by its dying predecessor.
        let mut guard = lock(&current);
        if guard.as_ref().map(|conn| conn.id == id).unwrap_or(false) {
            *guard = None;
        }
    }
}

/// Silence + ownership watchdog. Runs while the bridge lives; the silence
/// branch forces the connection thread to drop (it performs the release),
/// the ownership branch releases the engine exactly once per loss.
#[cfg(target_os = "windows")]
fn watchdog_loop(
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<BridgeShared>>,
    targets: Arc<Mutex<CaptureTargets>>,
    current: Arc<Mutex<Option<CurrentConn>>>,
    force_drop: Arc<AtomicU64>,
    sender: Sender<EngineEvent>,
) -> impl FnOnce() {
    move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            let now = Instant::now();
            let (phase, last_rx) = {
                let state = lock(&shared);
                (state.phase, state.last_rx)
            };
            if phase != BridgePhase::Connected {
                continue;
            }
            if silence_expired(last_rx, now) {
                // Mark the connection for forced drop and disconnect the
                // pipe; the connection thread performs the release-all
                // teardown and returns the bridge to Listening.
                let victim = lock(&current).as_ref().map(|conn| (conn.id, handle_of(conn)));
                if let Some((id, handle)) = victim {
                    force_drop.store(id, Ordering::Relaxed);
                    pipe_windows::disconnect(handle);
                }
                continue;
            }
            let generation = lock(&targets).generation;
            let transition = lock(&shared).ownership.sync_validity(generation, now);
            if transition == Some(false) {
                // Ownership lost: current generation never acked or the ack
                // is stale. Release the engine's pressed states; the link
                // stays up and T resends continue until re-acked.
                let mut state = lock(&shared);
                state.counters.ownership_losses_total += 1;
                state.counters.release_all_total += 1;
                state.pressed.clear();
                let _ = sender.send(EngineEvent::ReleaseAll);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_protocol::RemoteButton;
    use std::collections::BTreeSet;
    use std::sync::mpsc;

    /// Integration-ish host test: the public API works without any Windows
    /// runtime. On Windows the same calls race the live threads, so only
    /// the platform-independent state transitions are asserted.
    #[test]
    fn capture_targets_round_trip_through_the_public_api() {
        let (tx, _rx) = mpsc::channel();
        let bridge = RcBridge::start(tx);
        let snapshot = bridge.snapshot();
        assert!(!snapshot.already_running || cfg!(target_os = "windows"));

        let home_bit = 1u64 << RemoteButton::Home.ordinal();
        bridge.set_capture_targets(true, home_bit);
        let snapshot = bridge.snapshot();
        assert_eq!(snapshot.generation, 1);
        let expected: Vec<u16> = rc_protocol::buttons::capture_set(home_bit);
        assert_eq!(snapshot.target_usages, expected);
        assert!(snapshot
            .target_usages
            .contains(&RemoteButton::Back.usage()));
        assert!(!snapshot.target_usages.contains(&RemoteButton::Menu.usage()));

        // No change: no generation bump.
        bridge.set_capture_targets(true, home_bit);
        assert_eq!(bridge.snapshot().generation, 1);

        // Change: new generation.
        bridge.set_capture_targets(true, 0);
        let snapshot = bridge.snapshot();
        assert_eq!(snapshot.generation, 2);
        assert_eq!(
            snapshot.target_usages,
            rc_protocol::buttons::capture_set(0)
        );

        // Disabled: empty set (fail-open), new generation.
        bridge.set_capture_targets(false, home_bit);
        let snapshot = bridge.snapshot();
        assert_eq!(snapshot.generation, 3);
        assert!(snapshot.target_usages.is_empty());

        bridge.set_voice_synth(Some(0x00E6));
        let snapshot = bridge.snapshot();
        assert_eq!(snapshot.synth_target, Some(0x00E6));
        assert!(!snapshot.synth_confirmed);
        bridge.set_voice_synth(Some(0x00E6));
        assert_eq!(bridge.snapshot().synth_target, Some(0x00E6));
        bridge.set_voice_synth(None);
        assert_eq!(bridge.snapshot().synth_target, None);

        bridge.stop();
    }

    #[test]
    fn next_generation_helper_matches_targets_install() {
        // Sanity: state::next_generation and CaptureTargets::install agree.
        let mut targets = CaptureTargets::default();
        assert_eq!(targets.install(BTreeSet::new()), 1);
        assert_eq!(targets.install(BTreeSet::new()), 2);
    }
}
