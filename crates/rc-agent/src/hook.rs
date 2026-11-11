//! Frida Gum `Interceptor` hook on `ntdll!NtDeviceIoControlFile`.
//!
//! The UMDF host submits HID keyboard reports to the kernel through this
//! export; at call time the output buffer already holds the 9-byte report
//! (byte 0 = report id 0x01, offsets 3/5/7 = little-endian usage slots) and
//! the input buffer is 8 bytes with bytes 4/5 == 0x02/0x01.
//!
//! Hook shape: Frida Gum's [`Interceptor`] provides the enter/leave hook.
//! `on_enter` keeps the target filter (IOCTL 0x80018483, in len 8,
//! in[4]/in[5] == 0x02/0x01, out len >= 9, report id 0x01), runs the
//! [`plan_frame`]/`apply_*` decision path — edge emission and the
//! gate-delay / release-hold sleeps included, on the intercepted thread —
//! then patches and verifies the output buffer. Gum invokes the original
//! function in between. `on_leave` restores the original 9 bytes when the
//! buffer still contains exactly our patched content, and counts
//! `kernel_changed` otherwise. This is the same hook shape the reference
//! JS agent (`rc003_agent.js`, `installHook()`) runs in the same WUDFHost;
//! Gum relocates the prologue with full instruction awareness, which
//! removes the hand-written pattern table and trampoline whose 16-byte
//! assumptions crashed in `KiUserCallbackDispatcher` on Win11 26100.
//!
//! The listener object is shared by every thread that enters the function,
//! so it holds no state: per-invocation bookkeeping lives on a thread-local
//! stack ([`crate::frames`]) pushed before anything can fail, and every
//! callback body is wrapped in `catch_unwind` — on any panic the original
//! function is still called untouched (fail open, counted). A byte is
//! written only with a plan and a valid lease; everything else passes
//! through byte-for-byte.
//!
//! Toolchain gate: the Frida Gum devkit ships as an MSVC static library, so
//! the hook is built only for `target_env = "msvc"` (the production
//! toolchain). Other Windows toolchains compile a fail-open stub whose
//! `install()` logs `hook:unsupported_toolchain` and installs nothing.

use std::sync::atomic::AtomicU64;

/// Raw count of every NtDeviceIoControlFile call.
/// Kept lock-free because it is bumped on the syscall path; folded into the
/// reported stats by the heartbeat.
pub static HW_IOCTL_CALLS: AtomicU64 = AtomicU64::new(0);

/// Raw count of calls that passed the shape filter (target ioctl, 8-byte
/// input with the expected selector, keyboard report id). Exposed for the
/// diagnostic pipe: it separates "no reports flow through this host" from
/// "reports flow but nothing matches".
pub static HW_TARGET_HITS: AtomicU64 = AtomicU64::new(0);

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub use imp::install;

/// Fail-open stub for Windows toolchains the Frida Gum devkit does not
/// support: the GNU cross-build produces the agent without capture. Never a
/// silent no-op — the exact tag is logged and the refusal recorded.
#[cfg(all(target_os = "windows", not(target_env = "msvc")))]
pub fn install() -> Result<(), String> {
    crate::logging::file_log("hook:unsupported_toolchain");
    Err("unsupported_toolchain".to_string())
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
mod imp {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;

    use frida_gum as gum;
    use gum::interceptor::{Interceptor, InvocationContext, InvocationListener};
    use gum::{Gum, NativePointer};
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::Sleep;

    use rc_protocol::agent_wire::AgentToHelper;
    use rc_protocol::consts::{
        IOCTL_IN_BYTE4, IOCTL_IN_BYTE5, IOCTL_IN_LEN, REPORT_ID, REPORT_LEN, TARGET_IOCTL,
    };

    use super::HW_IOCTL_CALLS;
    use crate::clock::unix_ms;
    use crate::frames::{EnterFrame, FrameStack};
    use crate::logging;
    use crate::report::{
        apply_frame_counters, apply_frame_pressed, apply_restore_outcome, apply_write_fail,
        apply_write_ok, plan_frame, restore_decision, FrameInputs, LeaveAction, SynthFrame,
    };
    use crate::runtime::{lock_ok, RUNTIME};
    use crate::transport;

    /// Gum must stay initialized for the process lifetime: the hook's
    /// callbacks run inside Gum's trampolines.
    static GUM: OnceLock<Gum> = OnceLock::new();

    thread_local! {
        /// In-flight invocations on this thread. Pushed before anything can
        /// fail, popped by the paired `on_leave`.
        static FRAMES: RefCell<FrameStack> = RefCell::new(FrameStack::default());
    }

    /// The shared listener. Gum may call both callbacks from any thread at
    /// the same time, so this type carries no state — the `&mut self`
    /// receiver is the binding's calling convention, not an exclusive
    /// borrow we may act on. All shared state is atomic or thread-local;
    /// everything else lives in RUNTIME behind its mutex.
    struct NtListener;

    impl InvocationListener for NtListener {
        fn on_enter(&mut self, context: InvocationContext) {
            // The push happens before anything can fail: a panic later in
            // this body still leaves a poppable passthrough frame.
            FRAMES.with(|f| f.borrow_mut().push(EnterFrame::passthrough()));
            HW_IOCTL_CALLS.fetch_add(1, Ordering::Relaxed);
            // Fail open around every callback: on a panic the original
            // function still runs untouched, counted.
            if catch_unwind(AssertUnwindSafe(|| unsafe { enter(context) })).is_err() {
                transport::count_error();
            }
        }

        fn on_leave(&mut self, _context: InvocationContext) {
            // Gum pairs leave with enter LIFO; pop this invocation's frame.
            let Some(frame) = FRAMES.with(|f| f.borrow_mut().pop()) else {
                return; // unpaired leave: nothing was pushed, nothing to do
            };
            if catch_unwind(AssertUnwindSafe(|| unsafe { leave(&frame) })).is_err() {
                transport::count_error();
            }
        }
    }

    /// Install the Interceptor hook on `ntdll!NtDeviceIoControlFile`.
    /// Returns Err(reason) without touching the process when anything is
    /// unexpected; the caller logs and continues fail-open.
    pub fn install() -> Result<(), String> {
        let gum = GUM.get_or_init(Gum::obtain);

        let ntdll = unsafe { GetModuleHandleW(w!("ntdll.dll")) }
            .map_err(|e| format!("hook:ntdll_module:{e}"))?;
        let proc_addr = match unsafe { GetProcAddress(ntdll, s!("NtDeviceIoControlFile")) } {
            Some(f) => f,
            None => return Err("hook:export_missing".to_string()),
        };
        let target = NativePointer(unsafe { std::mem::transmute::<_, *mut c_void>(proc_addr) });
        if target.is_null() {
            return Err("hook:export_null".to_string());
        }

        // The listener instance must live at one address for the process
        // lifetime; Gum stores the raw pointer. It carries no state, so
        // sharing it across threads is safe.
        let listener: &'static mut NtListener = Box::leak(Box::new(NtListener));
        // Gum's interceptor is a weak-backed global singleton: `obtain`
        // returns the one strong reference this process holds, and when
        // that reference is dropped Gum disposes the interceptor and
        // removes every attached function context, uninstalling the hook.
        // Nothing may drop it while the hook is installed, so forget both
        // the interceptor and the attach handle (a `static` will not do:
        // `Interceptor` is not `Sync`). On the attach-error path above,
        // dropping is harmless: nothing was attached yet.
        let mut interceptor = Interceptor::obtain(gum);
        let handle = interceptor
            .attach(target, listener)
            .map_err(|e| format!("hook:attach:{e}"))?;
        std::mem::forget(handle);
        std::mem::forget(interceptor);
        Ok(())
    }

    /// Enter callback body: filter, decide, patch, verify. Runs on the
    /// caller's thread; Gum submits the (possibly patched) buffer to the
    /// original function right after this returns.
    unsafe fn enter(context: InvocationContext) {
        // Cheap shape filter before any locking: everything the agent does
        // not own goes straight through.
        let io_control_code = context.arg(5) as u32;
        let input_buffer_length = context.arg(7) as u32;
        let output_buffer_length = context.arg(9) as u32;
        let input_buffer = context.arg(6) as *const u8;
        let output_buffer = context.arg(8) as *mut u8;
        if io_control_code != TARGET_IOCTL
            || input_buffer_length != IOCTL_IN_LEN
            || output_buffer_length < REPORT_LEN as u32
            || input_buffer.is_null()
            || output_buffer.is_null()
        {
            return;
        }

        let mut in_bytes = [0u8; IOCTL_IN_LEN as usize];
        std::ptr::copy_nonoverlapping(input_buffer, in_bytes.as_mut_ptr(), in_bytes.len());
        if in_bytes[4] != IOCTL_IN_BYTE4 || in_bytes[5] != IOCTL_IN_BYTE5 {
            return;
        }

        let mut report = [0u8; REPORT_LEN];
        std::ptr::copy_nonoverlapping(output_buffer, report.as_mut_ptr(), REPORT_LEN);
        if report[0] != REPORT_ID {
            return;
        }

        super::HW_TARGET_HITS.fetch_add(1, Ordering::Relaxed);
        process_hit(&report, output_buffer);
    }

    /// Decide + execute one intercepted report submission. `report` is the
    /// output buffer's original content; `output_buffer` is the caller's
    /// buffer (9+ valid bytes, verified by the filter). This thread's top
    /// frame is upgraded to a hit when (and only when) a verified patch
    /// landed.
    unsafe fn process_hit(report: &[u8; REPORT_LEN], output_buffer: *mut u8) {
        let Some(rt) = RUNTIME.get() else {
            // Boot never ran (or has not yet): pure passthrough.
            return;
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
            apply_frame_pressed(&mut core.pressed, &d);
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

        // Synthesis traces for gated-away frames (rate-limited trace points).
        if matches!(decision.synth_frame, SynthFrame::Press | SynthFrame::Held) {
            if let Some(reason) = decision.synth_skip_reason {
                logging::synth_trace(&format!("skip reason={reason}"));
            }
        }

        // ---- Release hold: keep the synthesized key down a little longer
        // so the app can drain trailing audio. Runs on the caller's thread,
        // by design.
        if decision.release_hold_ms > 0 {
            Sleep(decision.release_hold_ms);
            logging::synth_trace(&format!("release hold_ms={}", decision.release_hold_ms));
        }

        let Some(patch) = decision.patch else {
            // No write warranted: pass the report through byte-for-byte.
            return;
        };

        // ---- Press-frame gate delay: let the app finish its IME switch. ----
        if decision.gate_delay_ms > 0 {
            Sleep(decision.gate_delay_ms);
            logging::log_event(&format!("synth:gate delay_ms={}", decision.gate_delay_ms));
        }

        // ---- Patch, verify, record: the kernel copies the (patched) buffer
        // when Gum invokes the original function. ----
        std::ptr::copy_nonoverlapping(patch.patched.as_ptr(), output_buffer, REPORT_LEN);

        // Verify the readback (report id + slots; bytes 1/2 belong to the
        // OS, which may legitimately touch them mid-call).
        let mut back = [0u8; REPORT_LEN];
        std::ptr::copy_nonoverlapping(output_buffer, back.as_mut_ptr(), REPORT_LEN);
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

        FRAMES.with(|f| {
            f.borrow_mut()
                .mark_hit(output_buffer as usize, *report, patch.patched, verified)
        });
    }

    /// Leave callback body: restore the original bytes when the buffer
    /// still contains exactly our patched content. Runs on the caller's
    /// thread, after Gum's call to the original function.
    unsafe fn leave(frame: &EnterFrame) {
        if frame.output_buffer == 0 {
            // Passthrough frame: nothing was written, nothing to restore.
            return;
        }
        let Some(rt) = RUNTIME.get() else {
            return;
        };
        let restore_enabled = {
            let core = lock_ok(&rt.core);
            core.restore_on_leave
        };
        let buf = frame.output_buffer as *mut u8;
        let mut cur = [0u8; REPORT_LEN];
        std::ptr::copy_nonoverlapping(buf, cur.as_mut_ptr(), REPORT_LEN);
        match restore_decision(frame.patch_verified, restore_enabled, &cur, &frame.patched) {
            LeaveAction::Nothing => {}
            LeaveAction::KernelChanged => {
                // The kernel modified it during the call: leave it alone.
                let mut core = lock_ok(&rt.core);
                apply_restore_outcome(&mut core.stats, false);
            }
            LeaveAction::Restore => {
                // Still exactly our patched content: put the original 9
                // bytes back.
                std::ptr::copy_nonoverlapping(frame.original.as_ptr(), buf, REPORT_LEN);
                let mut core = lock_ok(&rt.core);
                apply_restore_outcome(&mut core.stats, true);
            }
        }
    }
}
