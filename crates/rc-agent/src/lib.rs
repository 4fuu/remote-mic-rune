//! rc-agent — the injected agent DLL of remote-mic-rune.
//!
//! Loaded into the WUDFHost.exe process that hosts the Xiaomi RC003 BLE HID
//! device. `rc_agent_boot` installs a Frida Gum `Interceptor` hook on
//! `ntdll!NtDeviceIoControlFile` and connects a named-pipe transport to the
//! elevated helper. Target IOCTL submissions (`0x80018483`) carry the 9-byte
//! keyboard report; the pure decision layer in [`report`] decides which
//! slots to scrub (captured buttons, canary keys) or overwrite (voice-key
//! synthesis), the state machine in [`state`] enforces the lease and every
//! command guardrail, and the hook layer applies the plan with
//! patch -> verify -> call -> restore semantics.
//!
//! Fail-open is absolute: before boot, without a valid lease, in observe
//! mode, past the clears cap, on a refused hook attach or caught panic — no
//! byte of any report is ever written, and keys fall through to Windows
//! natively.

pub mod clock;
#[cfg(target_os = "windows")]
pub mod diag;
pub mod frames;
pub mod report;
pub mod state;

#[cfg(target_os = "windows")]
pub mod boot;
#[cfg(target_os = "windows")]
pub mod hook;
#[cfg(target_os = "windows")]
pub mod logging;
#[cfg(target_os = "windows")]
pub mod runtime;
#[cfg(target_os = "windows")]
pub mod transport;
