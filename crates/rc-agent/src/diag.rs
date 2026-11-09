//! Field diagnostics over a dedicated named pipe.
//!
//! Inside the UMDF sandbox the host token cannot write any filesystem
//! location, so `file_log` silently drops everything. This module keeps an
//! in-memory ring of the last log lines (mirrored from `file_log`) plus
//! thread liveness counters, and serves them on
//! `\\.\pipe\RemoteMicRune.AgentDiag.<pid>`: connect, send any byte, read
//! the story. Diagnostic only; nothing here may fail the agent.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Security::PSECURITY_DESCRIPTOR;
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};

const RING_CAP: usize = 512;

static RING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Liveness counters: each long-lived thread bumps its counter on every
/// loop iteration; a dump shows whether a thread is spinning or wedged.
pub static LOOP_RX: AtomicU64 = AtomicU64::new(0);
pub static LOOP_TX: AtomicU64 = AtomicU64::new(0);
pub static LOOP_HB: AtomicU64 = AtomicU64::new(0);
pub static WRITE_OK: AtomicU64 = AtomicU64::new(0);
pub static WRITE_FAIL: AtomicU64 = AtomicU64::new(0);
/// Last WriteFile failure code on the uplink (0xF0000001 = short write).
pub static LAST_WRITE_ERR: AtomicU64 = AtomicU64::new(0);

/// Mirror one line into the ring (called from `file_log`).
pub fn ring_push(line: &str) {
    if let Ok(mut ring) = RING.lock() {
        if ring.len() >= RING_CAP {
            ring.pop_front();
        }
        ring.push_back(line.to_string());
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_dump() -> String {
    let mut out = format!(
        "diag:pid={} loops rx={} tx={} hb={} write_ok={} write_fail={} last_write_err={:#x} ioctl_calls={} target_hits={}\n",
        std::process::id(),
        LOOP_RX.load(Ordering::Relaxed),
        LOOP_TX.load(Ordering::Relaxed),
        LOOP_HB.load(Ordering::Relaxed),
        WRITE_OK.load(Ordering::Relaxed),
        WRITE_FAIL.load(Ordering::Relaxed),
        LAST_WRITE_ERR.load(Ordering::Relaxed),
        crate::hook::HW_IOCTL_CALLS.load(Ordering::Relaxed),
        crate::hook::HW_TARGET_HITS.load(Ordering::Relaxed),
    );
    if let Ok(ring) = RING.lock() {
        for line in ring.iter() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Serve dump requests forever, one client at a time.
pub fn serve() {
    let name = format!(r"\\.\pipe\RemoteMicRune.AgentDiag.{}", std::process::id());
    let wname = wide(&name);
    let sddl = wide("D:P(A;;GA;;;WD)");
    loop {
        unsafe {
            let mut sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: windows::Win32::Foundation::FALSE,
            };
            let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .is_ok()
            {
                sa.lpSecurityDescriptor = sd.0;
            }
            let pipe = CreateNamedPipeW(
                PCWSTR(wname.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                8192,
                8192,
                0,
                Some(&sa),
            );
            if pipe.0 as isize == -1 {
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
            if ConnectNamedPipe(pipe, None).is_err() {
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
                continue;
            }
            // One request per connection: any byte means "dump".
            let mut buf = [0u8; 64];
            let mut got: u32 = 0;
            let _ = ReadFile(pipe, Some(&mut buf), Some(&mut got), None);
            let dump = build_dump();
            let mut written: u32 = 0;
            let _ = WriteFile(pipe, Some(dump.as_bytes()), Some(&mut written), None);
            let _ = FlushFileBuffers(pipe);
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
    }
}
