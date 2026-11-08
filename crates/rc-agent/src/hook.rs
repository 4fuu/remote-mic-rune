//! Manual inline hook on `ntdll!NtDeviceIoControlFile`.
//!
//! The UMDF host submits HID keyboard reports to the kernel through this
//! export; at call time the output buffer already holds the 9-byte report
//! (byte 0 = report id 0x01, offsets 3/5/7 = little-endian usage slots) and
//! the input buffer is 8 bytes with bytes 4/5 == 0x02/0x01.
//!
//! Hook shape (validated layout on Win10/Win11 x64):
//!   4C 8B D1              mov r10, rcx
//!   B8 xx xx xx xx        mov eax, <syscall id>
//!   F6 04 25 xx xx xx xx 01  test byte ptr [KUSER_SHARED_DATA+...], 1
//! = 16 bytes with a whole-instruction boundary at +16. Anything else is
//! logged (`unsupported_prologue:<hex>`) and NOT hooked — fail open, no
//! capture, the process is never harmed.
//!
//! Patch: a 12-byte absolute jump (48 B8 <imm64> FF E0) plus 4 bytes of 0xCC
//! padding over the first 16 bytes; the trampoline holds the stolen 16 bytes
//! followed by an absolute jump back to fn+16. All other threads are
//! suspended around the write; an interlocked compare-exchange prevents any
//! double installation.
//!
//! The detour wraps everything in catch_unwind; on any error the original
//! function is still called untouched. Writes follow patch -> verify ->
//! call -> restore with the reference agent's semantics.

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering};

use windows::core::{s, w};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
    PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, Sleep,
    SuspendThread, THREAD_SUSPEND_RESUME,
};

use rc_protocol::agent_wire::AgentToHelper;
use rc_protocol::consts::{
    IOCTL_IN_BYTE4, IOCTL_IN_BYTE5, IOCTL_IN_LEN, REPORT_ID, REPORT_LEN, TARGET_IOCTL,
};

use crate::clock::unix_ms;
use crate::logging;
use crate::report::{
    apply_frame_counters, apply_restore_outcome, apply_write_fail, apply_write_ok, plan_frame,
    FrameInputs, SynthFrame,
};
use crate::runtime::{lock_ok, RUNTIME};
use crate::transport;

/// Exact 10-parameter NT signature; IoControlCode is parameter index 5.
pub type NtDeviceIoControlFileFn = unsafe extern "system" fn(
    file_handle: HANDLE,
    event: *mut c_void,
    apc_routine: *mut c_void,
    apc_context: *mut c_void,
    io_status_block: *mut c_void,
    io_control_code: u32,
    input_buffer: *mut c_void,
    input_buffer_length: u32,
    output_buffer: *mut c_void,
    output_buffer_length: u32,
) -> i32;

static ORIGINAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static TRAMPOLINE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// 0 = not hooked, 1 = installing, 2 = installed. Guarded by an interlocked
/// compare-exchange so two racing boot calls can never double-hook.
static HOOK_PHASE: AtomicU8 = AtomicU8::new(0);

/// Raw count of every NtDeviceIoControlFile call (reference `ioctl_calls`).
/// Kept lock-free because it is bumped on the syscall path; folded into the
/// reported stats by the heartbeat.
pub static HW_IOCTL_CALLS: AtomicU64 = AtomicU64::new(0);

const PROLOGUE_LEN: usize = 16;
const TRAMP_LEN: usize = PROLOGUE_LEN + 12;

const EXPECTED_HEAD: [u8; 4] = [0x4C, 0x8B, 0xD1, 0xB8];

fn prologue_ok(b: &[u8; PROLOGUE_LEN]) -> bool {
    // mov r10, rcx; mov eax, imm32 — then the KUSER_SHARED_DATA test whose
    // final immediate is 1. The wildcard bytes are the shared-data address.
    b[0..4] == EXPECTED_HEAD && b[8] == 0xF6 && b[9] == 0x04 && b[10] == 0x25 && b[15] == 0x01
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Install the inline hook. Returns Err(reason) without touching the process
/// when anything is unexpected; the caller logs and continues fail-open.
pub fn install() -> Result<(), String> {
    // Interlocked gate: exactly one installer wins.
    if HOOK_PHASE
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(()); // already installed or installing
    }
    let result = install_inner();
    HOOK_PHASE.store(if result.is_ok() { 2 } else { 0 }, Ordering::Release);
    result
}

fn install_inner() -> Result<(), String> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| format!("hook:ntdll_module:{e}"))?;
        let proc_addr = match GetProcAddress(ntdll, s!("NtDeviceIoControlFile")) {
            Some(f) => f,
            None => return Err("hook:export_missing".to_string()),
        };
        let target: *mut c_void = std::mem::transmute(proc_addr);
        if target.is_null() {
            return Err("hook:export_null".to_string());
        }

        // Validate the prologue BEFORE writing anything.
        let mut prologue = [0u8; PROLOGUE_LEN];
        std::ptr::copy_nonoverlapping(target as *const u8, prologue.as_mut_ptr(), PROLOGUE_LEN);
        if !prologue_ok(&prologue) {
            return Err(format!("unsupported_prologue:{}", hex(&prologue)));
        }

        // Build the trampoline before suspending threads: VirtualAlloc does
        // no loader/heap work we would want to do with the process frozen.
        let tramp =
            VirtualAlloc(None, TRAMP_LEN, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
        if tramp.is_null() {
            return Err("hook:trampoline_alloc".to_string());
        }
        // Stolen bytes at the front...
        std::ptr::copy_nonoverlapping(target as *const u8, tramp as *mut u8, PROLOGUE_LEN);
        // ...then an absolute jump back to fn+16.
        write_abs_jump(tramp.add(PROLOGUE_LEN), target.add(PROLOGUE_LEN));

        // Publish both addresses before the jump exists.
        ORIGINAL.store(target, Ordering::Release);
        TRAMPOLINE.store(tramp, Ordering::Release);

        let suspended = suspend_other_threads();

        // The 16-byte patch: 48 B8 <imm64> FF E0 (12) + 4x CC padding. The
        // jump lands on OUR detour; the detour then calls the trampoline.
        let detour_addr: *mut c_void =
            std::mem::transmute::<NtDeviceIoControlFileFn, *mut c_void>(detour);
        let mut patch = [0xCCu8; PROLOGUE_LEN];
        patch[0] = 0x48;
        patch[1] = 0xB8;
        patch[2..10].copy_from_slice(&(detour_addr as usize).to_le_bytes());
        patch[10] = 0xFF;
        patch[11] = 0xE0;

        let mut old = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(target, PROLOGUE_LEN, PAGE_EXECUTE_READWRITE, &mut old)
            .map_err(|e| format!("hook:protect:{e}"))?;
        std::ptr::copy_nonoverlapping(patch.as_ptr(), target as *mut u8, PROLOGUE_LEN);
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(target), PROLOGUE_LEN);
        let _ = VirtualProtect(target, PROLOGUE_LEN, old, &mut old);

        // Resume in reverse suspension order.
        for h in suspended.iter().rev() {
            let _ = ResumeThread(*h);
        }
        for h in suspended {
            let _ = CloseHandle(h);
        }
    }
    Ok(())
}

/// `48 B8 <imm64> FF E0` — movabs rax, imm64; jmp rax.
unsafe fn write_abs_jump(at: *mut c_void, to: *mut c_void) {
    let mut code = [0u8; 12];
    code[0] = 0x48;
    code[1] = 0xB8;
    code[2..10].copy_from_slice(&(to as usize).to_le_bytes());
    code[10] = 0xFF;
    code[11] = 0xE0;
    std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, code.len());
}

/// Suspend every other thread of this process; returns their handles in
/// suspension order for resumption. Snapshotting happens before any thread
/// is frozen, so no allocator runs under suspension.
unsafe fn suspend_other_threads() -> Vec<HANDLE> {
    let mut suspended: Vec<HANDLE> = Vec::new();
    let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
        return suspended; // no snapshot: patch anyway, as the reference did
    };
    let my_pid = GetCurrentProcessId();
    let my_tid = GetCurrentThreadId();
    let mut entry = THREADENTRY32::default();
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    if Thread32First(snap, &mut entry).is_ok() {
        loop {
            if entry.th32OwnerProcessID == my_pid && entry.th32ThreadID != my_tid {
                if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) {
                    // SuspendThread returns the previous suspend count, or
                    // (u32)-1 on failure.
                    if SuspendThread(h) != u32::MAX {
                        suspended.push(h);
                    } else {
                        let _ = CloseHandle(h);
                    }
                }
            }
            if Thread32Next(snap, &mut entry).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap);
    suspended
}

// ----------------------------------------------------------------- detour

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn detour(
    file_handle: HANDLE,
    event: *mut c_void,
    apc_routine: *mut c_void,
    apc_context: *mut c_void,
    io_status_block: *mut c_void,
    io_control_code: u32,
    input_buffer: *mut c_void,
    input_buffer_length: u32,
    output_buffer: *mut c_void,
    output_buffer_length: u32,
) -> i32 {
    HW_IOCTL_CALLS.fetch_add(1, Ordering::Relaxed);
    let call_original = || -> i32 {
        // The trampoline replays the stolen prologue (including `mov r10,
        // rcx`), so all ten arguments must be forwarded verbatim.
        call_tramp(
            file_handle,
            event,
            apc_routine,
            apc_context,
            io_status_block,
            io_control_code,
            input_buffer,
            input_buffer_length,
            output_buffer,
            output_buffer_length,
        )
    };

    // Cheap shape filter before any locking: everything the agent does not
    // own goes straight through.
    if io_control_code != TARGET_IOCTL
        || input_buffer_length != IOCTL_IN_LEN
        || output_buffer_length < REPORT_LEN as u32
        || input_buffer.is_null()
        || output_buffer.is_null()
    {
        return call_original();
    }

    let mut in_bytes = [0u8; IOCTL_IN_LEN as usize];
    std::ptr::copy_nonoverlapping(input_buffer as *const u8, in_bytes.as_mut_ptr(), in_bytes.len());
    if in_bytes[4] != IOCTL_IN_BYTE4 || in_bytes[5] != IOCTL_IN_BYTE5 {
        return call_original();
    }

    let mut report = [0u8; REPORT_LEN];
    std::ptr::copy_nonoverlapping(output_buffer as *const u8, report.as_mut_ptr(), REPORT_LEN);
    if report[0] != REPORT_ID {
        return call_original();
    }

    // Everything else runs guarded: on any panic the original function is
    // still called untouched (fail open, counted).
    let action = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_hit(&report, output_buffer, &call_original)
    }));
    match action {
        Ok(status) => status,
        Err(_) => {
            transport::count_error();
            call_original()
        }
    }
}

/// Forward all ten arguments to the trampoline (the original function's
/// first 16 stolen bytes, then a jump back to fn+16).
#[allow(clippy::too_many_arguments)]
unsafe fn call_tramp(
    file_handle: HANDLE,
    event: *mut c_void,
    apc_routine: *mut c_void,
    apc_context: *mut c_void,
    io_status_block: *mut c_void,
    io_control_code: u32,
    input_buffer: *mut c_void,
    input_buffer_length: u32,
    output_buffer: *mut c_void,
    output_buffer_length: u32,
) -> i32 {
    let tramp_addr = TRAMPOLINE.load(Ordering::Acquire);
    let f: NtDeviceIoControlFileFn = std::mem::transmute(tramp_addr);
    f(
        file_handle,
        event,
        apc_routine,
        apc_context,
        io_status_block,
        io_control_code,
        input_buffer,
        input_buffer_length,
        output_buffer,
        output_buffer_length,
    )
}

/// Decide + execute one intercepted report submission. `report` is the
/// output buffer's original content; `output_buffer` is the caller's buffer
/// (9+ valid bytes, verified by the detour); `call_original` invokes the
/// untouched original with the caller's full argument set. Returns the
/// NTSTATUS.
unsafe fn process_hit(
    report: &[u8; REPORT_LEN],
    output_buffer: *mut c_void,
    call_original: &dyn Fn() -> i32,
) -> i32 {
    let Some(rt) = RUNTIME.get() else {
        // Boot never ran (or has not yet): pure passthrough.
        return call_original();
    };

    // ---- Phase A: decide under the lock; no sleeps, no writes. ----
    let (decision, edge_msg, synth_to) = {
        let mut core = lock_ok(&rt.core);
        let now = crate::clock::mono_ms();
        let inputs = FrameInputs {
            report,
            report_usages: &core.report_usages,
            clear_usages: &core.clear_usages,
            synth_from: core.synth_from,
            synth_to: core.synth_to,
            synth_held: core.synth_held,
            gate_delay_ms: core.gate_delay_ms,
            lease_ok: core.lease_ok(now),
            mode_clear: core.mode_clear,
            clears_ok: core.stats.clears_ok,
            pressed: &core.pressed,
        };
        let d = plan_frame(&inputs);
        if d.consume_held {
            core.synth_held = false;
        }
        apply_frame_counters(&mut core.stats, &d);
        let edge_msg = d.edge.clone().map(|usages| AgentToHelper::Edge {
            t: unix_ms(),
            usages,
            reason: "state".into(),
        });
        (d, edge_msg, core.synth_to)
    };
    if let Some(m) = &edge_msg {
        transport::enqueue_agent(m);
    }

    // Synthesis traces for gated-away frames (reference synthTrace points).
    if matches!(decision.synth_frame, SynthFrame::Press | SynthFrame::Held) {
        if let Some(reason) = decision.synth_skip_reason {
            logging::synth_trace(&format!("skip reason={reason}"));
        }
    }

    // ---- Release hold: keep the synthesized key down a little longer so
    // the app can drain trailing audio. Runs on the caller's thread, by
    // design, exactly like the reference.
    if decision.release_hold_ms > 0 {
        Sleep(decision.release_hold_ms);
        logging::synth_trace(&format!("release hold_ms={}", decision.release_hold_ms));
    }

    let Some(patch) = decision.patch else {
        // No write warranted: pass the report through byte-for-byte.
        return call_original();
    };

    // ---- Press-frame gate delay: let the app finish its IME switch. ----
    if decision.gate_delay_ms > 0 {
        Sleep(decision.gate_delay_ms);
        logging::log_event(&format!("synth:gate delay_ms={}", decision.gate_delay_ms));
    }

    // ---- Phase B: patch, verify, call, restore. ----
    std::ptr::copy_nonoverlapping(patch.patched.as_ptr(), output_buffer as *mut u8, REPORT_LEN);

    // Verify the readback (report id + slots; bytes 1/2 belong to the OS,
    // which may legitimately touch them mid-call).
    let mut back = [0u8; REPORT_LEN];
    std::ptr::copy_nonoverlapping(output_buffer as *const u8, back.as_mut_ptr(), REPORT_LEN);
    let verified =
        back[0] == patch.patched[0] && back[3..REPORT_LEN] == patch.patched[3..REPORT_LEN];
    {
        let mut core = lock_ok(&rt.core);
        if verified {
            apply_write_ok(&mut core.stats, patch.synth_changed);
            if patch.synth_changed {
                // The OS now believes the synthesized key is down.
                core.synth_held = true;
            }
        } else {
            apply_write_fail(&mut core.stats);
        }
    }
    if patch.synth_changed {
        if verified {
            logging::synth_trace(&format!("replace ok to=0x{synth_to:04x}"));
            transport::enqueue_agent(&AgentToHelper::VoiceFrame {
                t: unix_ms(),
                replaced: true,
                reason: "ok".into(),
            });
        } else {
            logging::synth_trace("skip reason=verify_mismatch");
        }
    }

    // The kernel copies the (patched) buffer inside the original call.
    let status = call_original();

    // ---- Restore, unless disabled or the kernel moved underneath us. ----
    let restore_enabled = {
        let core = lock_ok(&rt.core);
        core.restore_on_leave
    };
    if verified && restore_enabled {
        let mut cur = [0u8; REPORT_LEN];
        std::ptr::copy_nonoverlapping(output_buffer as *const u8, cur.as_mut_ptr(), REPORT_LEN);
        if cur == patch.patched {
            // Still exactly our patched content: put the original 9 bytes back.
            std::ptr::copy_nonoverlapping(report.as_ptr(), output_buffer as *mut u8, REPORT_LEN);
            let mut core = lock_ok(&rt.core);
            apply_restore_outcome(&mut core.stats, true);
        } else {
            // The kernel modified it during the call: leave it alone.
            let mut core = lock_ok(&rt.core);
            apply_restore_outcome(&mut core.stats, false);
        }
    }
    status
}
