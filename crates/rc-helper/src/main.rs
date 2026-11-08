//! rc-helper — the elevated helper process of remote-mic-rune.
//!
//! Startup sequence (spec-exact):
//!   parse args -> task commands short-circuit -> open log -> scan registry
//!   -> verdict (refuse shared/ambiguous without --force) -> dry-run prints
//!   the plan and exits -> create the agent pipe server -> inject (or boot
//!   the resident agent: reconnect flow) -> read the app descriptor ->
//!   connect the app link -> HELLO/OK -> forward initial targets to the
//!   agent -> run loops (renew, ping, watchdogs, duration) -> teardown.
//!
//! One ordering note: the app descriptor is read BEFORE the injection's boot
//! stage, because the boot config embeds the descriptor's token. The scan,
//! verdict, and dry-run all happen before any descriptor wait, so a dry run
//! never depends on the app.
//!
//! Teardown is best-effort-atomic: Disarm goes to the agent FIRST, everything
//! else (Bye both links, flush, 1 s grace, exit) after. If the helper dies
//! without any teardown, the agent's 2 s lease expiry returns the remote to
//! native behavior — that fail-open is sacred and nothing here weakens it.

#![cfg_attr(all(target_os = "windows", not(feature = "console")), windows_subsystem = "windows")]
// The runtime plumbing below (pipes, injection, teardown) is cfg-gated to
// Windows; on the host build it is unused by design while the pure decision
// functions stay testable. Keep that build quiet without hiding anything on
// the real target (windows builds must stay warning-clean).
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

mod agent_link;
mod app_link;
mod cli;
mod host;
mod inject;
mod logging;
mod pipe;
mod recovery;
mod registry;
mod task;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_link::AgentEvent;
use app_link::AppEvent;
use logging::Logger;

// Everything below touches named pipes, the registry, or process injection:
// it exists only on Windows and is unit-tested on the host through the pure
// functions it calls into.
#[cfg(target_os = "windows")]
use {
    std::sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    std::time::{Duration, Instant},
    rc_protocol::agent_wire::HelperToAgent,
    rc_protocol::bridge_wire::{
        all_known_buttons, format_bye, format_edges, format_hello, format_ownership,
        format_ping, format_recovering, format_synth_ack, format_voice_frame,
    },
    rc_protocol::buttons::VOICE_KEY_HID_USAGE,
    rc_protocol::consts::{
        AGENT_AUTH_TIMEOUT_MS, AGENT_PIPE_PREFIX, AGENT_RX_TIMEOUT_MS, BRIDGE_PIPE_NAME,
        BRIDGE_PROTOCOL_VERSION, GATE_DELAY_MS, LEASE_RENEW_INTERVAL_MS, RECOVERY_COOLDOWN_MS,
    },
    rc_protocol::handoff::BridgeDescriptor,
    agent_link::AgentLink,
    app_link::AppLink,
};

/// Combined event bus: links publish, the main loop consumes.
#[derive(Debug)]
enum Event {
    Agent(AgentEvent),
    App(AppEvent),
    /// Background recovery attempt finished.
    RecoveryDone { new_pid: Option<u32> },
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn set_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[cfg(target_os = "windows")]
fn install_ctrl_handler() {
    use windows::core::BOOL;
    use windows::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT,
        CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
        set_shutdown();
        // Claim the console-level events so the process is not killed before
        // teardown; logoff/shutdown keep default handling.
        match ctrl {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => BOOL(1),
            CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => BOOL(0),
            _ => BOOL(0),
        }
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

#[cfg(not(target_os = "windows"))]
fn install_ctrl_handler() {}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse_args(raw) {
        Ok(args) => args,
        Err(msg) if msg.is_empty() => {
            // --help: print usage, exit 0 per convention (documented codes
            // reserve 2 for wrong usage; help is not wrong usage).
            println!("{}", cli::USAGE);
            std::process::exit(cli::exit::OK);
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("{}", cli::USAGE);
            std::process::exit(cli::exit::USAGE);
        }
    };

    // Task commands short-circuit: they never touch the device scan.
    let task_requested = args.install_task || args.remove_task || args.task_status;

    #[cfg(target_os = "windows")]
    {
        let logger = Arc::new(Logger::open(&default_log_path(&args)));
        if task_requested {
            let ok = if args.install_task {
                task::windows_impl::install_task(&logger)
            } else if args.remove_task {
                task::windows_impl::remove_task(&logger)
            } else {
                task::windows_impl::task_status(&logger)
            };
            std::process::exit(if ok { cli::exit::OK } else { cli::exit::FAILURE });
        }
        install_ctrl_handler();
        std::process::exit(run(args, logger));
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = task_requested;
        if args.dry_run {
            // Host testing aid: parse + scan-free plan echo.
            println!("dry-run on a non-Windows host: registry scan unavailable");
            std::process::exit(cli::exit::OK);
        }
        eprintln!("rc-helper requires Windows (it injects into WUDFHost.exe)");
        std::process::exit(cli::exit::FAILURE);
    }
}

/// `%ProgramData%\RemoteMicRune\helper-<pid>.log` — ProgramData, NOT
/// LOCALAPPDATA: a scheduled task runs in a fresh session-0 context where
/// the user's LOCALAPPDATA is not the profile the log belongs to.
fn default_log_path(args: &cli::Args) -> std::path::PathBuf {
    if let Some(path) = &args.log {
        return path.clone();
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        std::path::PathBuf::from(base)
            .join("RemoteMicRune")
            .join(format!("helper-{}.log", std::process::id()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        std::path::PathBuf::from("/tmp")
            .join("RemoteMicRune")
            .join(format!("helper-{}.log", std::process::id()))
    }
}

/// Default agent DLL location: next to the helper exe.
fn default_dll_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default()
        .join("rc_agent.dll")
}

fn local_appdata() -> String {
    std::env::var("LOCALAPPDATA").unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The run loop (Windows)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn run(args: cli::Args, logger: Arc<Logger>) -> i32 {
    use host::HostVerdict;
    use rc_protocol::consts::BRIDGE_DESCRIPTOR_FILE;

    logger.kv(
        "[START]",
        &[
            ("pid", std::process::id().to_string()),
            ("build", inject::helper_build_id()),
            ("force", args.force.to_string()),
            ("observe", args.observe.to_string()),
            ("no_restore", args.no_restore.to_string()),
            (
                "canary",
                args.canary_usage
                    .map(|u| format!("0x{u:04X}"))
                    .unwrap_or_else(|| "-".into()),
            ),
            ("follow_app", args.follow_app.to_string()),
            ("duration", args.duration_secs.to_string()),
        ],
    );

    // 1. Scan the registry (full walk; counters for attribution).
    let scan = match registry::scan_hosts() {
        Ok(scan) => scan,
        Err(e) => {
            logger.error(&format!("[STOP] registry scan failed: {e}"));
            return cli::exit::FAILURE;
        }
    };
    logger.kv(
        "[SCAN]",
        &[
            ("nodes", scan.nodes_seen.to_string()),
            ("diag_keys", scan.diag_keys_opened.to_string()),
            ("host_pid_zero", scan.no_host_count.to_string()),
            ("read_failures", scan.read_failures.len().to_string()),
            ("entries", scan.entries.len().to_string()),
        ],
    );
    for (instance, reason) in &scan.read_failures {
        logger.warn(&format!("[SCAN] unreadable HostPid at {instance}: {reason}"));
    }

    // 2. Verdict.
    let verdict = host::verdict_from_scan(&scan);
    logger.kv("[VERDICT]", &[("result", format!("{verdict:?}"))]);

    // 3. Refusals + overrides (--target-pid wins over everything).
    let target_pid = match host::apply_overrides(&verdict, args.force, args.target_pid) {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            logger.error(
                "[STOP] no live RC003 host: the remote is not connected, or its UMDF host \
                 is not running (HostPid=0)",
            );
            return cli::exit::NO_HOST;
        }
        Err(HostVerdict::Shared { pid, members, rc003_members, .. }) => {
            logger.error(&format!(
                "[STOP] refused shared host: pid {pid} hosts {members} device nodes \
                 ({rc003_members} of them RC003). Injecting it would affect the user's \
                 other BLE devices. Pass --force (or --target-pid) ONLY when you know \
                 which remote is the intended one."
            ));
            return cli::exit::SHARED_HOST;
        }
        Err(HostVerdict::Ambiguous { pids }) => {
            logger.error(&format!(
                "[STOP] refused ambiguous setup: {} distinct live RC003 hosts ({:?}). \
                 Pick one with --target-pid.",
                pids.len(),
                pids
            ));
            return cli::exit::AMBIGUOUS;
        }
        Err(other) => {
            logger.error(&format!("[STOP] refused: {other:?}"));
            return cli::exit::FAILURE;
        }
    };

    // Device instance id for recovery: from the verdict, or the scan when
    // --target-pid pointed somewhere else.
    let instance_id: Option<String> = match &verdict {
        HostVerdict::Exclusive { instance_id, .. } => Some(instance_id.clone()),
        HostVerdict::Shared { instance_id, .. } if !instance_id.is_empty() => {
            Some(instance_id.clone())
        }
        _ => scan
            .entries
            .iter()
            .find(|e| e.pid == target_pid && e.is_rc003)
            .map(|e| e.instance_id.clone()),
    };

    // 4. Dry run: plan and exit before anything is created or injected.
    if args.dry_run {
        println!("plan:");
        println!("  target pid      : {target_pid}");
        println!("  instance id     : {}", instance_id.as_deref().unwrap_or("-"));
        println!(
            "  agent dll       : {}",
            args.agent_dll.as_ref().unwrap_or(&default_dll_path()).display()
        );
        println!("  agent pipe      : {AGENT_PIPE_PREFIX}{}", std::process::id());
        println!("  bridge pipe     : {BRIDGE_PIPE_NAME}");
        println!(
            "  descriptor      : {} ({}\\RemoteMicRune\\{BRIDGE_DESCRIPTOR_FILE})",
            if app_link::read_descriptor(&local_appdata()).is_ok() { "present" } else { "missing" },
            "%LOCALAPPDATA%"
        );
        println!("  observe         : {}", args.observe);
        println!("  restore         : {}", !args.no_restore);
        println!(
            "  canary usage    : {}",
            args.canary_usage.map(|u| format!("0x{u:04X}")).unwrap_or_else(|| "-".into())
        );
        println!("  verdict         : {verdict:?}");
        logger.info("dry-run complete");
        return cli::exit::OK;
    }

    // 5. Wait for the app descriptor (needed for the boot token). Under
    // --follow-app the wait is bounded by the grace window.
    let descriptor = match wait_for_descriptor(&args, &logger) {
        Some(descriptor) => descriptor,
        None => {
            // follow-app and the app never showed up: nothing to do.
            logger.warn("app descriptor never appeared within the follow-app grace; exiting");
            return cli::exit::OK;
        }
    };

    // 6. Agent pipe server.
    let pipe_name = format!("{AGENT_PIPE_PREFIX}{}", std::process::id());
    let (tx, rx): (Sender<Event>, Receiver<Event>) = std::sync::mpsc::channel();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<AgentEvent>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let bus_tx = tx.clone();
    let bus_forward_shutdown = shutdown.clone();
    std::thread::Builder::new()
        .name("bus-agent".into())
        .spawn(move || {
            while let Ok(event) = agent_rx.recv() {
                if bus_tx.send(Event::Agent(event)).is_err() {
                    break;
                }
            }
            let _ = bus_forward_shutdown;
        })
        .ok();

    let agent = Arc::new(match AgentLink::start(
        pipe_name.clone(),
        agent_tx,
        descriptor.token.clone(),
        logger.clone(),
    ) {
        Ok(agent) => agent,
        Err(e) => {
            logger.error(&format!("[STOP] agent pipe server failed: {e}"));
            return cli::exit::FAILURE;
        }
    });
    logger.kv(
        "[AGENT-SERVER]",
        &[("pipe", pipe_name.clone()), ("sddl", agent_link::AGENT_PIPE_SDDL.to_string())],
    );

    // 7. Resolve + validate the agent DLL, then inject (or boot resident).
    let dll = args.agent_dll.clone().unwrap_or_else(default_dll_path);
    if !dll.exists() {
        logger.error(&format!(
            "[STOP] agent DLL not found at {}. Build rc-agent and place it next to the \
             helper, or pass --agent-dll.",
            dll.display()
        ));
        teardown(&agent, None, &descriptor.token, "missing-dll", &logger);
        return cli::exit::BAD_DLL;
    }
    logger.kv("[INJECT]", &[("pid", target_pid.to_string()), ("dll", dll.display().to_string())]);
    if let Err(e) = inject::inject(
        target_pid,
        &dll,
        &pipe_name,
        &descriptor.token,
        &inject::helper_build_id(),
        &logger,
    ) {
        logger.error(&format!("[STOP] injection failed: {e}"));
        return cli::exit::INJECT_FAILED;
    }

    // 8. App link: HELLO and wait for OK (bounded; DENY -> descriptor retry).
    let mut runtime = Runtime {
        args: args.clone(),
        logger: logger.clone(),
        token: descriptor.token.clone(),
        generation: 0,
        observe: args.observe,
        restore_on: !args.no_restore,
        canary: args.canary_usage,
        gate_declared: false,
        synth_to: None,
        last_targets: None,
        agent_session_live: false,
        agent_hello_seen: false,
        agent_connected_at: Instant::now(),
        last_agent_line: Instant::now(),
        target_pid,
        instance_id,
        recovering: Arc::new(AtomicBool::new(false)),
        recovery_last_attempt: None,
        injected: true,
        start: Instant::now(),
        last_ping: Instant::now(),
        last_hb_state: (true, false, true),
        last_hb_log: Instant::now(),
    };

    let mut app: Option<AppLink> = None;
    let app_grace = if args.follow_app {
        Some(Instant::now() + Duration::from_millis(app_link::FOLLOW_APP_GRACE_MS))
    } else {
        None
    };

    // Renew timer thread: 500 ms, the ONLY thing keeping the agent armed.
    {
        let renew_agent = agent.clone();
        let runtime_token = descriptor.token.clone();
        let renew_shutdown = shutdown.clone();
        let renew_logger = logger.clone();
        std::thread::Builder::new()
            .name("renew".into())
            .spawn(move || {
                while !renew_shutdown.load(Ordering::SeqCst) {
                    renew_agent.send(HelperToAgent::Renew {
                        token: runtime_token.clone(),
                    });
                    std::thread::sleep(Duration::from_millis(
                        LEASE_RENEW_INTERVAL_MS as u64,
                    ));
                }
                let _ = renew_logger;
            })
            .ok();
    }

    // 9. Event loop.
    let mut exit_code = cli::exit::OK;
    let mut connect_backoff: Option<Instant> = None;

    loop {
        // Shutdown / duration first: teardown must not be starved by I/O.
        if SHUTDOWN.load(Ordering::SeqCst) {
            runtime.logger.info("shutdown signal received");
            teardown(&agent, app.as_ref(), &runtime.token, "signal", &runtime.logger);
            break;
        }
        if runtime.args.duration_secs > 0
            && runtime.start.elapsed() >= Duration::from_secs(runtime.args.duration_secs)
        {
            runtime.logger.info("duration elapsed");
            teardown(&agent, app.as_ref(), &runtime.token, "duration", &runtime.logger);
            break;
        }

        // App (re)connect state machine.
        if app.as_ref().map(|a| a.is_connected()) != Some(true) {
            app = match connect_app(&runtime, tx.clone(), shutdown.clone()) {
                AppConnect::Connected(link) => {
                    runtime.logger.info("app link connected");
                    connect_backoff = None;
                    Some(link)
                }
                AppConnect::Failed => None,
            };
            if app.is_none() {
                if let Some(deadline) = app_grace {
                    if Instant::now() >= deadline {
                        runtime.logger.warn("app link gone past follow-app grace; exiting");
                        teardown(&agent, None, &runtime.token, "app-gone", &runtime.logger);
                        break;
                    }
                }
                let backoff_until =
                    connect_backoff.unwrap_or_else(|| Instant::now() + Duration::from_millis(app_link::RECONNECT_BACKOFF_MS));
                if Instant::now() < backoff_until {
                    // Sleep in small slices so shutdown stays responsive.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                connect_backoff = Some(Instant::now() + Duration::from_millis(app_link::RECONNECT_BACKOFF_MS));
            }
        } else {
            // Ping every second while connected.
            if app_link::ping_due(runtime.last_ping) {
                if let Some(link) = &app {
                    link.send_line(format_ping());
                }
                runtime.last_ping = Instant::now();
            }
        }

        // Agent watchdogs (only while the loop is otherwise idle; they run
        // every pass, the sleeps above are the only place we linger).
        if let Some(reason) = runtime.agent_watchdog() {
            runtime.logger.warn(&format!("agent watchdog: {reason}"));
            if reason.contains("superseded") {
                // Transport up but no hello inside the auth window: another
                // helper owns the agent; fighting it would only flap keys.
                teardown(&agent, app.as_ref(), &runtime.token, &reason, &runtime.logger);
                exit_code = cli::exit::NO_AGENT_HELLO;
                break;
            }
            runtime.maybe_start_recovery(&agent, app.as_ref(), tx.clone());
        } else {
            runtime.maybe_start_recovery(&agent, app.as_ref(), tx.clone());
        }

        // Dispatch events.
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Agent(event)) => {
                runtime.handle_agent_event(&agent, app.as_ref(), event);
                runtime.last_ping = runtime.last_ping.max(Instant::now() - Duration::from_secs(1));
            }
            Ok(Event::App(event)) => {
                match event {
                    AppEvent::Disconnected => {
                        // Keys must return to native IMMEDIATELY on app loss.
                        runtime.logger.warn("app link lost; disarming agent");
                        agent.send(HelperToAgent::Disarm {
                            token: runtime.token.clone(),
                        });
                        agent.send(HelperToAgent::Bye {
                            token: runtime.token.clone(),
                        });
                        runtime.last_targets = None;
                    }
                    other => runtime.handle_app_event(&agent, other),
                }
            }
            Ok(Event::RecoveryDone { new_pid }) => {
                runtime.recovering.store(false, Ordering::SeqCst);
                runtime.recovery_last_attempt = Some(logging::unix_ms());
                if let Some(pid) = new_pid {
                    runtime.logger.info(&format!("recovery injected new host pid {pid}"));
                    runtime.target_pid = pid;
                }
                if let Some(link) = &app {
                    link.send_line(format_recovering(false));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // All bus senders dropped: should not happen (bus thread holds
                // one), treat as fatal.
                teardown(&agent, app.as_ref(), &runtime.token, "internal", &runtime.logger);
                exit_code = cli::exit::FAILURE;
                break;
            }
        }
    }

    agent.stop();
    exit_code
}

#[allow(clippy::needless_return)]
#[cfg(not(target_os = "windows"))]
fn run(_args: cli::Args, _logger: Arc<Logger>) -> i32 {
    cli::exit::FAILURE
}

#[cfg(target_os = "windows")]
enum AppConnect {
    Connected(AppLink),
    Failed,
}

/// One app-connect attempt: read descriptor, open the pipe, send HELLO and
/// wait for OK/DENY. Both answers arrive asynchronously as AppEvents; this
/// function only reports transport success.
#[cfg(target_os = "windows")]
fn connect_app(
    runtime: &Runtime,
    events: Sender<Event>,
    shutdown: Arc<AtomicBool>,
) -> AppConnect {
    let descriptor = match app_link::read_descriptor(&local_appdata()) {
        Ok(descriptor) => descriptor,
        Err(e) => {
            runtime.logger.kv("[APP]", &[("connect", "descriptor_missing".into()), ("detail", e)]);
            return AppConnect::Failed;
        }
    };
    runtime.logger.kv(
        "[APP]",
        &[
            ("connect", "descriptor_loaded".into()),
            ("version", descriptor.version.to_string()),
            ("pid", descriptor.pid.to_string()),
        ],
    );
    let hello = format_hello(BRIDGE_PROTOCOL_VERSION, &descriptor.token, std::process::id());
    let (app_tx, app_rx) = std::sync::mpsc::channel::<AppEvent>();
    let bus_tx = events;
    std::thread::Builder::new()
        .name("bus-app".into())
        .spawn(move || {
            while let Ok(event) = app_rx.recv() {
                if bus_tx.send(Event::App(event)).is_err() {
                    break;
                }
            }
        })
        .ok();
    match AppLink::connect(
        &descriptor.pipe,
        hello,
        app_tx,
        shutdown,
        runtime.logger.clone(),
    ) {
        Ok(link) => AppConnect::Connected(link),
        Err(_) => AppConnect::Failed,
    }
}

/// Poll for the descriptor until it exists and version-matches. The wait is
/// bounded by whichever comes first: the follow-app grace window (when set)
/// or `--duration`. An unbounded wait would leave a `--duration` run hanging
/// before its timer ever starts.
#[cfg(target_os = "windows")]
fn wait_for_descriptor(
    args: &cli::Args,
    logger: &Logger,
) -> Option<BridgeDescriptor> {
    let mut deadline: Option<Instant> = if args.follow_app {
        Some(Instant::now() + Duration::from_millis(app_link::FOLLOW_APP_GRACE_MS))
    } else {
        None
    };
    if args.duration_secs > 0 {
        let duration_end = Instant::now() + Duration::from_secs(args.duration_secs);
        deadline = Some(match deadline {
            Some(existing) => existing.min(duration_end),
            None => duration_end,
        });
    }
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return None;
        }
        match app_link::read_descriptor(&local_appdata()) {
            Ok(descriptor) => {
                logger.kv(
                    "[DESCRIPTOR]",
                    &[
                        ("version", descriptor.version.to_string()),
                        ("pid", descriptor.pid.to_string()),
                        ("pipe", descriptor.pipe.clone()),
                    ],
                );
                return Some(descriptor);
            }
            Err(e) => {
                logger.warn(&format!("waiting for app descriptor: {e}"));
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return None;
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

/// Disarm FIRST, then everything else. Best-effort-atomic teardown.
#[cfg(target_os = "windows")]
fn teardown(
    agent: &AgentLink,
    app: Option<&AppLink>,
    token: &str,
    reason: &str,
    logger: &Logger,
) {
    logger.info(&format!("teardown ({reason}): disarm first"));
    // 1. Disarm (the agent releases any pressed edges immediately).
    agent.send(HelperToAgent::Disarm { token: token.to_string() });
    // 2. Everything else after.
    agent.send(HelperToAgent::Bye { token: token.to_string() });
    if let Some(app) = app {
        // close() sends the BYE line first, then stops the pump.
        app.close(Some(format_bye(reason)));
    }
    std::thread::sleep(Duration::from_secs(1));
    logger.info("teardown complete");
}

// ---------------------------------------------------------------------------
// Runtime state + event handling
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
struct Runtime {
    args: cli::Args,
    logger: Arc<Logger>,
    token: String,
    generation: u64,
    observe: bool,
    restore_on: bool,
    canary: Option<u16>,
    gate_declared: bool,
    synth_to: Option<u16>,
    last_targets: Option<(u64, Vec<u16>)>,
    agent_session_live: bool,
    agent_hello_seen: bool,
    agent_connected_at: Instant,
    last_agent_line: Instant,
    target_pid: u32,
    instance_id: Option<String>,
    recovering: Arc<AtomicBool>,
    recovery_last_attempt: Option<u64>,
    injected: bool,
    start: Instant,
    last_ping: Instant,
    last_hb_state: (bool, bool, bool),
    last_hb_log: Instant,
}

#[cfg(target_os = "windows")]
impl Runtime {
    /// Watchdog verdicts; returns Some(reason) when the agent link looks
    /// broken. Two windows, from the protocol contract:
    /// - never authed within AGENT_AUTH_TIMEOUT_MS -> superseded/foreign peer;
    /// - no downlink line within AGENT_RX_TIMEOUT_MS after auth -> dead link.
    fn agent_watchdog(&self) -> Option<String> {
        if !self.agent_session_live || !self.injected {
            return None;
        }
        if !self.agent_hello_seen
            && self.agent_connected_at.elapsed() >= Duration::from_millis(AGENT_AUTH_TIMEOUT_MS as u64)
        {
            return Some("no hello within auth window (superseded by another helper?)".into());
        }
        if self.agent_hello_seen
            && self.last_agent_line.elapsed() >= Duration::from_millis(AGENT_RX_TIMEOUT_MS as u64)
        {
            return Some("agent link silent beyond rx timeout".into());
        }
        None
    }

    /// Recovery state machine entry point. Only when: injected once, no live
    /// agent session, not already recovering, device still present, cooldown
    /// elapsed.
    fn maybe_start_recovery(&mut self, agent: &AgentLink, app: Option<&AppLink>, events: Sender<Event>) {
        if !self.injected || self.recovering.load(Ordering::SeqCst) || self.agent_session_live {
            return;
        }
        let device_present = registry::scan_hosts()
            .map(|scan| recovery::device_present(&scan))
            .unwrap_or(false);
        if !recovery::should_recover(
            true,
            device_present,
            self.recovery_last_attempt,
            logging::unix_ms(),
            RECOVERY_COOLDOWN_MS,
        ) {
            return;
        }
        let Some(instance_id) = self.instance_id.clone() else {
            self.logger
                .warn("recovery skipped: no device instance id recorded from the first scan");
            return;
        };
        self.recovering.store(true, Ordering::SeqCst);
        self.logger.warn(&format!(
            "starting device-node recovery for {instance_id} (agent link dead, device present)"
        ));
        if let Some(app) = app {
            app.send_line(format_recovering(true));
        }
        // Keep the agent pipe server accepting; the fresh host's agent will
        // connect after re-injection. The previous session is already gone.
        let _ = agent;
        let logger = self.logger.clone();
        let dll = self.args.agent_dll.clone().unwrap_or_else(default_dll_path);
        let pipe_name = format!("{AGENT_PIPE_PREFIX}{}", std::process::id());
        let token = self.token.clone();
        // The fresh agent will be booted with this token; its Hello must match.
        agent.set_expected_token(token.clone());
        let build = inject::helper_build_id();
        let recovering_flag = self.recovering.clone();
        std::thread::Builder::new()
            .name("recovery".into())
            .spawn(move || {
                let result = recovery::windows_impl::run_recovery(&instance_id, &logger)
                    .and_then(|new_pid| {
                        inject::inject(new_pid, &dll, &pipe_name, &token, &build, &logger)
                            .map_err(|e| {
                                logger.error(&format!("recovery re-injection failed: {e}"));
                                e
                            })
                            .ok()
                            .map(|_| new_pid)
                    });
                recovering_flag.store(false, Ordering::SeqCst);
                let _ = events.send(Event::RecoveryDone { new_pid: result });
            })
            .ok();
    }

    fn handle_agent_event(&mut self, agent: &AgentLink, app: Option<&AppLink>, event: AgentEvent) {
        match event {
            AgentEvent::Connected => {
                self.agent_session_live = true;
                self.agent_hello_seen = false;
                self.agent_connected_at = Instant::now();
                self.last_agent_line = Instant::now();
            }
            AgentEvent::Hello(hello) => {
                self.last_agent_line = Instant::now();
                if hello.token != self.token {
                    self.logger.warn(&format!(
                        "agent hello token mismatch (pid {}, build {}): dismissing",
                        hello.pid, hello.build
                    ));
                    agent.dismiss_current();
                    return;
                }
                self.agent_hello_seen = true;
                if hello.build != inject::helper_build_id() {
                    self.logger.warn(&format!(
                        "agent build mismatch: agent={} helper={} (stale agent dll?)",
                        hello.build,
                        inject::helper_build_id()
                    ));
                }
                if hello.arch != "x64" {
                    self.logger.warn(&format!("agent reports arch {} (expected x64)", hello.arch));
                }
                self.logger.kv(
                    "[AGENT-HELLO]",
                    &[
                        ("pid", hello.pid.to_string()),
                        ("build", hello.build.clone()),
                        ("arch", hello.arch.clone()),
                        ("lease_ms", hello.lease_ms.to_string()),
                        ("mode_clear", hello.mode_clear.to_string()),
                    ],
                );
                if hello.lease_ms != rc_protocol::consts::LEASE_MS {
                    self.logger.warn(&format!(
                        "agent lease {} ms != helper lease {} ms",
                        hello.lease_ms,
                        rc_protocol::consts::LEASE_MS
                    ));
                }
                // Post-auth static configuration.
                agent.send(HelperToAgent::Mode { token: self.token.clone(), clear: !self.observe });
                agent.send(HelperToAgent::Restore {
                    token: self.token.clone(),
                    on: self.restore_on,
                });
                // Re-apply whatever the app has configured so far (the agent
                // may be a fresh boot into a restarted host).
                if let Some((generation, usages)) = self.last_targets.clone() {
                    self.send_targets(agent, generation, &usages);
                }
                if let Some(to) = self.synth_to {
                    self.send_synth(agent, Some(to));
                } else {
                    self.send_synth(agent, None);
                }
                if self.gate_declared {
                    agent.send(HelperToAgent::Gate {
                        token: self.token.clone(),
                        on: true,
                        delay_ms: Some(GATE_DELAY_MS),
                    });
                }
            }
            AgentEvent::Hb(hb) => {
                self.last_agent_line = Instant::now();
                self.generation = hb.target_generation;
                // Heartbeats are dense; log on state flips or every 30 s.
                let state = (hb.lease_ok, hb.disarmed, hb.connected);
                if state != self.last_hb_state || self.last_hb_log.elapsed() >= Duration::from_secs(30) {
                    self.logger.kv(
                        "[HB]",
                        &[
                            ("lease_ok", hb.lease_ok.to_string()),
                            ("disarmed", hb.disarmed.to_string()),
                            ("connected", hb.connected.to_string()),
                            ("since_renew_ms", hb.since_renew_ms.to_string()),
                            ("generation", hb.target_generation.to_string()),
                            (
                                "clears_ok",
                                hb.stats.clears_ok.to_string(),
                            ),
                            (
                                "canary_hits",
                                hb.stats.canary_hits.to_string(),
                            ),
                        ],
                    );
                    self.last_hb_state = state;
                    self.last_hb_log = Instant::now();
                }
                let _ = app;
            }
            AgentEvent::Edge { usages, reason } => {
                self.last_agent_line = Instant::now();
                if let Some(link) = app {
                    link.send_line(format_edges(logging::unix_ms(), &usages));
                    if reason != "state" && !usages.is_empty() {
                        self.logger.warn(&format!(
                            "edge with forced-release reason {reason:?} carried non-empty set"
                        ));
                    }
                } else {
                    self.logger.warn("edge received with no app link; dropped (absolute state self-heals)");
                }
            }
            AgentEvent::TargetsAck { generation, usages } => {
                self.last_agent_line = Instant::now();
                if let Some(link) = app {
                    link.send_line(format_ownership(generation, &usages));
                }
            }
            AgentEvent::SynthAck { from, to } => {
                self.last_agent_line = Instant::now();
                if let Some(link) = app {
                    link.send_line(format_synth_ack((to != 0).then_some(to)));
                }
                let _ = from;
            }
            AgentEvent::VoiceFrame { replaced, reason } => {
                self.last_agent_line = Instant::now();
                if let Some(link) = app {
                    link.send_line(format_voice_frame(replaced, &reason));
                }
            }
            AgentEvent::Log(msg) => {
                self.last_agent_line = Instant::now();
                self.logger.info(&format!("[AGENT] {msg}"));
            }
            AgentEvent::Bye => {
                self.last_agent_line = Instant::now();
                self.logger.info("agent said bye");
                if let Some(link) = app {
                    link.send_line(format_bye("agent-bye"));
                }
            }
            AgentEvent::Disconnected { had_hello } => {
                self.agent_session_live = false;
                self.agent_hello_seen = false;
                self.logger.warn(&format!(
                    "agent session disconnected (had_hello={had_hello}); watchdog will decide recovery"
                ));
            }
        }
    }

    fn handle_app_event(&mut self, agent: &AgentLink, event: AppEvent) {
        match event {
            AppEvent::Ok { version, generation, usages } => {
                if version != BRIDGE_PROTOCOL_VERSION {
                    self.logger.error(&format!(
                        "app OK carries version {version}, helper speaks {BRIDGE_PROTOCOL_VERSION}; \
                         refusing to fight a half-understood protocol"
                    ));
                    return;
                }
                self.logger.kv(
                    "[APP-OK]",
                    &[
                        ("version", version.to_string()),
                        ("generation", generation.to_string()),
                        ("usages", format!("{usages:04X?}")),
                    ],
                );
                self.last_targets = Some((generation, usages.clone()));
                self.send_targets(agent, generation, &usages);
            }
            AppEvent::Targets { generation, usages } => {
                self.last_targets = Some((generation, usages.clone()));
                self.send_targets(agent, generation, &usages);
            }
            AppEvent::Synth { to } => {
                if let Some(to) = to {
                    if !rc_protocol::buttons::VOICE_SYNTHABLE_USAGES.contains(&to) {
                        self.logger.warn(&format!(
                            "app requested synth to 0x{to:04X}, which is not on the \
                             hardware-validated whitelist; ignored"
                        ));
                        return;
                    }
                }
                self.synth_to = to;
                self.send_synth(agent, to);
            }
            AppEvent::GateDeclare => {
                self.gate_declared = true;
                agent.send(HelperToAgent::Gate {
                    token: self.token.clone(),
                    on: true,
                    delay_ms: Some(GATE_DELAY_MS),
                });
            }
            AppEvent::Deny { reason } => {
                // DENY: the token we sent does not match; the app may have
                // rotated it. Re-read the descriptor so the next HELLO (the
                // reconnect below) carries the fresh token.
                self.logger
                    .warn(&format!("app denied hello: {reason}; re-reading descriptor"));
                if let Ok(descriptor) = app_link::read_descriptor(&local_appdata()) {
                    self.token = descriptor.token;
                    // The next agent hello must present this token.
                    agent.set_expected_token(self.token.clone());
                }
            }
            AppEvent::Bye { reason } => {
                self.logger.warn(&format!("app said bye ({reason}); disarming agent"));
                agent.send(HelperToAgent::Disarm { token: self.token.clone() });
                agent.send(HelperToAgent::Bye { token: self.token.clone() });
                self.last_targets = None;
            }
            AppEvent::Disconnected => {
                // Handled by the main loop (needs the Option<AppLink> swap).
            }
        }
    }

    /// Forward a targets set to the agent: `report` = app's button usages
    /// (whitelist-checked), `clear` = report plus the canary usage.
    /// Unknown usages are clamped out rather than forwarded (the agent would
    /// reject the whole command otherwise).
    fn send_targets(&self, agent: &AgentLink, generation: u64, usages: &[u16]) {
        let mut report: Vec<u16> = usages
            .iter()
            .copied()
            .filter(|u| all_known_buttons(&[*u]))
            .collect();
        report.sort_unstable();
        report.dedup();
        let mut clear = report.clone();
        if let Some(canary) = self.canary {
            if !clear.contains(&canary) {
                clear.push(canary);
            }
        }
        clear.sort_unstable();
        if report.len() != usages.len() {
            self.logger.warn(&format!(
                "targets clamped: app sent {:04X?}, forwarding known buttons only",
                usages
            ));
        }
        self.logger.kv(
            "[TARGETS]",
            &[
                ("generation", generation.to_string()),
                ("report", format!("{report:04X?}")),
                ("clear", format!("{clear:04X?}")),
            ],
        );
        agent.send(HelperToAgent::Targets {
            token: self.token.clone(),
            report,
            clear,
            generation,
        });
    }

    fn send_synth(&self, agent: &AgentLink, to: Option<u16>) {
        match to {
            Some(to) => agent.send(HelperToAgent::Synth {
                token: self.token.clone(),
                from: VOICE_KEY_HID_USAGE,
                to,
            }),
            None => agent.send(HelperToAgent::SynthOff {
                token: self.token.clone(),
            }),
        }
    }
}

// Heartbeat log throttling state (last_hb_state / last_hb_log) lives on the
// Runtime struct itself.
