//! Every timing constant, safety cap, and fixed name in one place.
//!
//! Values follow the reference implementation's real-device-validated numbers.
//! Do not tune these blindly: each one encodes a fail-open budget measured on
//! hardware (see docs/MECHANISMS.md).

/// Transport: named pipe the agent (inside WUDFHost) connects to.
/// Suffix with the helper PID; the helper passes the full name to the DLL in
/// [`crate::handoff::AgentBootConfig`].
pub const AGENT_PIPE_PREFIX: &str = r"\\.\pipe\RemoteMicRune.Agent.";

/// Transport: fixed bridge pipe between helper and app. Creating the first
/// instance is what enforces single-instance for the app.
pub const BRIDGE_PIPE_NAME: &str = r"\\.\pipe\RemoteMicRune.Bridge.v1";

/// Bridge line protocol version (HELLO/OK handshake).
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// Descriptor file the app publishes for the helper: pipe name, token, pid.
/// Directory: `%LOCALAPPDATA%\RemoteMicRune\`.
pub const BRIDGE_DESCRIPTOR_FILE: &str = "bridge.ini";

/// App runtime dir under LOCALAPPDATA.
pub const APP_DIR_NAME: &str = "RemoteMicRune";

/// Agent lease: if no renew arrives within this window the agent stops
/// clearing/synthesizing reports (fail-open). Helper renews at
/// [`LEASE_RENEW_INTERVAL_MS`].
pub const LEASE_MS: u32 = 2000;

/// Helper -> agent renew period. Must be well below [`LEASE_MS`].
pub const LEASE_RENEW_INTERVAL_MS: u32 = 500;

/// Agent -> helper heartbeat period (carries stats + pressed state).
pub const AGENT_HB_MS: u32 = 500;

/// Agent link: silence on the downlink beyond this means the helper is gone.
/// TCP/pipe errors are NOT a reliable liveness signal on Windows; only
/// "no renew within the window" is.
pub const AGENT_RX_TIMEOUT_MS: u32 = 3000;

/// Agent link: connected but never renewed within this window => the peer
/// holds a different token (we were superseded). Stop fighting for the slot.
pub const AGENT_AUTH_TIMEOUT_MS: u32 = 4000;

/// Agent link reconnect interval after a link-level drop.
pub const AGENT_RECONNECT_MS: u32 = 1000;

/// Bridge (app side): no line from the helper within this window => link dead,
/// release every pressed button.
pub const BRIDGE_SILENCE_TIMEOUT_MS: u32 = 3000;

/// Bridge (app side): helper pings every 1s (`P` line).
pub const BRIDGE_PING_INTERVAL_MS: u32 = 1000;

/// Ownership of keys is only trusted while the agent ACKed the current
/// generation within this window. Must expire BEFORE the agent lease
/// (2s) so the legacy keyboard path recovers first.
pub const OWNERSHIP_TIMEOUT_MS: u32 = 1500;

/// Report-layer synthesis: delay presenting the voice-key DOWN frame so the
/// app can finish switching IMEs (~53ms measured) before the key lands.
/// 0 = gate off.
pub const GATE_DELAY_MS: u32 = 150;

/// Report-layer synthesis: hold the synthesized key's UP frame this long on
/// physical release so the app can drain trailing audio.
pub const RELEASE_HOLD_MS: u32 = 200;

/// Report-layer synthesis connect timeout for the agent's initial init.
pub const AGENT_CONNECT_TIMEOUT_MS: u32 = 3000;

/// Hard cap on report modifications per agent instance. A runaway loop must
/// not be able to clear reports forever.
pub const MAX_CLEARS: u64 = 500_000;

/// Max protocol line length on any link; longer lines drop the connection.
pub const MAX_LINE_BYTES: usize = 4096;

/// Named pipe buffer size for both links.
pub const PIPE_BUFFER_BYTES: u32 = 4096;

/// Max auth failures before the bridge drops a connection.
pub const BRIDGE_MAX_DENY: u32 = 3;

/// Idle read backoff for pipe nowait polling (prevents a busy loop eating a
/// core; bounds extra edge latency at this value).
pub const PIPE_IDLE_BACKOFF_MS: u64 = 2;

// ---------------------------------------------------------------------------
// Report layer (agent inside WUDFHost)
// ---------------------------------------------------------------------------

/// The UMDF host->kernel HID report submission IOCTL intercepted by the agent.
/// Semantics established by the reference project's taps on real hardware.
pub const TARGET_IOCTL: u32 = 0x8001_8483;

/// Keyboard report id the agent touches.
pub const REPORT_ID: u8 = 0x01;

/// Input buffer length of the target IOCTL submission.
pub const IOCTL_IN_LEN: u32 = 8;

/// Input buffer bytes 4/5 must equal these (operation/selector).
pub const IOCTL_IN_BYTE4: u8 = 0x02;
pub const IOCTL_IN_BYTE5: u8 = 0x01;

/// Minimum output buffer length touched by the agent (full report length).
pub const REPORT_LEN: usize = 9;

/// Byte offsets of the three 16-bit usage slots inside the report.
/// Slots are little-endian: usage = lo | (hi << 8).
pub const USAGE_SLOT_OFFSETS: [usize; 3] = [3, 5, 7];

/// Gesture recognizer timings (reference values, "Mac parity").
pub const DOUBLE_CLICK_WINDOW_MS: u64 = 300;
pub const LONG_PRESS_THRESHOLD_MS: u64 = 550;
pub const REPEAT_START_DELAY_MS: u64 = 350;

// ---------------------------------------------------------------------------
// Device identity
// ---------------------------------------------------------------------------

/// Xiaomi remote USB hardware id fragments used to identify RC003 device
/// instances in the registry (HKLM\SYSTEM\CurrentControlSet\Enum\HID\...).
pub const DEVICE_VID: &str = "VID_2717";
pub const DEVICE_PID: &str = "PID_32B8";

/// Registry suffix under a device instance key that carries `HostPid`
/// (the PID of the WUDFHost.exe hosting the device).
pub const WUDF_DIAG_SUFFIX: &str = r"Device Parameters\WUDFDiagnosticInfo";
pub const HOSTPID_VALUE: &str = "HostPid";

/// Scheduled task name used for one-UAC enable of the helper.
pub const TASK_NAME: &str = r"RemoteMicRuneHelper";

/// Cooldown between device-node recovery attempts (`pnputil /remove-device` +
/// `/scan-devices` can take 60s+).
pub const RECOVERY_COOLDOWN_MS: u64 = 60_000;
