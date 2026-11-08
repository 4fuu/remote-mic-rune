//! Boot entry point and `DllMain`.
//!
//! The helper loads this DLL with `CreateRemoteThread(LoadLibraryW, path)`,
//! then starts a second remote thread on `rc_agent_boot(config_ptr)` where
//! `config_ptr` points at an `AgentBootConfig` it wrote into remote memory.
//! The config is copied defensively and validated (magic + size) before any
//! field is interpreted; a mismatch means helper and agent were built
//! against different contracts, so the agent stays dormant.
//!
//! `DllMain` deliberately does nothing but `DisableThreadLibraryCalls`:
//! doing real work under the loader lock is exactly the class of bug the
//! reference avoided by letting the helper drive boot.

use std::sync::atomic::{AtomicBool, Ordering};

use rc_protocol::consts::{AGENT_HB_MS, LEASE_MS};
use rc_protocol::handoff::AgentBootConfig;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;

/// Loader notification constant (Win32 `DLL_PROCESS_ATTACH`).
const DLL_PROCESS_ATTACH: u32 = 1;

use crate::logging;
use crate::runtime::{Runtime, RUNTIME};
use crate::state::AgentCore;
use crate::transport::{self, Outbound};

/// Boot result codes.
const BOOT_OK: u32 = 0;
const BOOT_BAD_ARGS: u32 = 1;
const BOOT_DOUBLE_ENTRY: u32 = 2;
const BOOT_RUNTIME_RACE: u32 = 3;

static BOOTED: AtomicBool = AtomicBool::new(false);

/// One-shot boot. Returns 0 on success; the process keeps running normally
/// (fail-open) on any failure.
#[no_mangle]
pub extern "system" fn rc_agent_boot(cfg: *const AgentBootConfig) -> u32 {
    // Double entry: a second remote thread must never spawn a second
    // transport or touch the runtime again.
    if BOOTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return BOOT_DOUBLE_ENTRY;
    }
    if cfg.is_null() {
        return BOOT_BAD_ARGS;
    }

    // Defensive copy before validation: the helper owns this memory and may
    // in principle be writing concurrently.
    let cfg = unsafe { cfg.read_unaligned() };
    if !cfg.valid() {
        logging::init();
        logging::file_log("boot:rejected (magic/size mismatch)");
        return BOOT_BAD_ARGS;
    }

    let Some(pipe_name) = cfg.pipe_name() else {
        logging::init();
        logging::file_log("boot:rejected (bad pipe name)");
        return BOOT_BAD_ARGS;
    };
    let token = cfg.token().unwrap_or_default();
    let build = cfg
        .build_id()
        .unwrap_or_else(|| format!("rc-agent/{}", env!("CARGO_PKG_VERSION")));
    // 0 means "use the protocol defaults" (older helpers may not set them).
    let lease_ms = if cfg.lease_ms == 0 { LEASE_MS } else { cfg.lease_ms };
    let hb_ms = if cfg.hb_ms == 0 { AGENT_HB_MS } else { cfg.hb_ms };

    logging::init();
    // A silent injected process is undebuggable; from here on everything
    // important also lands in the file log.
    std::panic::set_hook(Box::new(|info| {
        logging::file_log(&format!("panic: {info}"));
    }));
    logging::file_log(&format!("boot:ok build={build} lease_ms={lease_ms} hb_ms={hb_ms}"));

    let core = AgentCore::new(token, lease_ms, hb_ms, build);
    if RUNTIME
        .set(Runtime {
            core: std::sync::Mutex::new(core),
            out: Outbound::default(),
            conn: std::sync::Mutex::new(None),
            retired: std::sync::Mutex::new(Vec::new()),
            pipe_name,
        })
        .is_err()
    {
        return BOOT_RUNTIME_RACE;
    }
    let rt = RUNTIME.get().expect("runtime just set");

    // Hook first: with no targets configured and no lease it is fail-open by
    // construction, and installing before the transport means every report
    // is seen from the first moment. A refused hook (unknown prologue) is
    // logged and the agent continues without capture.
    match crate::hook::install() {
        Ok(()) => logging::file_log("hook:installed"),
        Err(e) => logging::file_log(&format!("hook:failed {e}")),
    }

    transport::spawn_threads(rt);
    BOOT_OK
}

/// Loader callback: nothing but thread-call disabling, ever.
#[no_mangle]
pub extern "system" fn DllMain(
    hinst_dll: HINSTANCE,
    fdw_reason: u32,
    _lpv_reserved: *mut std::ffi::c_void,
) -> windows::core::BOOL {
    if fdw_reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = DisableThreadLibraryCalls(windows::Win32::Foundation::HMODULE(hinst_dll.0));
        }
    }
    windows::core::BOOL(1)
}
