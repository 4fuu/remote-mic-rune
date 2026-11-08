//! Process-wide runtime shared by the hook, the transport and logging.
//!
//! One instance is created by `rc_agent_boot` and published through
//! `RUNTIME`; before that, every hook path is a fail-open passthrough.

use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Foundation::HANDLE;

use crate::state::AgentCore;
use crate::transport::Outbound;

/// A raw Win32 handle that may be shared across threads. `HANDLE` itself is
/// a raw pointer and neither `Send` nor `Sync`; the handle VALUE is trivially
/// copyable and valid process-wide, which is exactly what we share (the pipe
/// is written by the writer thread and closed by the heartbeat thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawHandle(pub HANDLE);
unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

pub struct Runtime {
    pub core: Mutex<AgentCore>,
    pub out: Outbound,
    /// Handle of the current pipe connection; None while disconnected.
    pub conn: Mutex<Option<RawHandle>>,
    /// Handles retired but not yet closed. Closing is deferred past any
    /// writer's in-flight overlapped write; the heartbeat performs the close.
    pub retired: Mutex<Vec<(RawHandle, u64)>>,
    pub pipe_name: String,
}

pub static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Lock that survives a poisoned mutex: after a caught panic the guarded
/// data is still consistent (every mutator runs under catch_unwind), so the
/// show goes on instead of turning one panic into a lock outage.
pub fn lock_ok<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
