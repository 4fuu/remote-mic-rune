//! Per-invocation bookkeeping for the Interceptor callbacks.
//!
//! Frida Gum delivers `on_enter`/`on_leave` pairs for every intercepted call
//! from any thread, and one listener object services all of them. Per-
//! invocation state therefore cannot live in the listener: it lives on a
//! per-thread stack ([`FrameStack`]), pushed by `on_enter` and popped by the
//! paired `on_leave`. Nested invocations of the hooked function on the same
//! thread stack up and pair up LIFO, exactly as Gum pairs the callbacks.
//!
//! Pure and platform-independent so `cargo test` covers the bookkeeping on
//! any host; the Windows hook layer only supplies the buffer bytes.

use rc_protocol::consts::REPORT_LEN;

/// One intercepted invocation, as `on_enter` recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnterFrame {
    /// Output buffer address; `0` = passthrough (no write was planned, the
    /// leave callback must not touch anything).
    pub output_buffer: usize,
    /// The buffer's content before the patch: the bytes a restore writes back.
    pub original: [u8; REPORT_LEN],
    /// The content we wrote and expect to still be there after the call.
    pub patched: [u8; REPORT_LEN],
    /// Whether the post-write readback confirmed the patch landed. Only a
    /// verified patch may ever be restored.
    pub patch_verified: bool,
}

impl EnterFrame {
    /// A frame for a call that passes through untouched.
    pub fn passthrough() -> Self {
        EnterFrame {
            output_buffer: 0,
            original: [0; REPORT_LEN],
            patched: [0; REPORT_LEN],
            patch_verified: false,
        }
    }
}

/// LIFO stack of in-flight invocations on one thread.
#[derive(Debug, Default)]
pub struct FrameStack {
    frames: Vec<EnterFrame>,
}

impl FrameStack {
    /// Record an invocation at `on_enter`. Every callback pushes first, so a
    /// panic later in the decision path still leaves a poppable frame.
    pub fn push(&mut self, frame: EnterFrame) {
        self.frames.push(frame);
    }

    /// Upgrade the top frame once a patch landed. The frame keeps its
    /// passthrough status when the call was filtered out or nothing was
    /// written: only real hits are marked.
    pub fn mark_hit(
        &mut self,
        output_buffer: usize,
        original: [u8; REPORT_LEN],
        patched: [u8; REPORT_LEN],
        patch_verified: bool,
    ) {
        if let Some(top) = self.frames.last_mut() {
            *top = EnterFrame {
                output_buffer,
                original,
                patched,
                patch_verified,
            };
        }
    }

    /// Consume the frame of the invocation that is returning. Returns `None`
    /// when `on_leave` fires without a matching `on_enter` (must not happen;
    /// Gum pairs the callbacks), in which case there is nothing to restore.
    pub fn pop(&mut self) -> Option<EnterFrame> {
        self.frames.pop()
    }

    /// In-flight invocations pushed but not yet popped.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(buffer: usize, seed: u8, verified: bool) -> EnterFrame {
        let mut original = [0u8; REPORT_LEN];
        let mut patched = [0u8; REPORT_LEN];
        original[0] = 0x01;
        patched[0] = 0x01;
        for (i, slot) in patched.iter_mut().enumerate().skip(3) {
            *slot = seed + i as u8;
        }
        EnterFrame {
            output_buffer: buffer,
            original,
            patched,
            patch_verified: verified,
        }
    }

    #[test]
    fn nested_invocations_pop_lifo_and_keep_passthroughs_separate() {
        let mut stack = FrameStack::default();
        // outer call filtered out at enter: passthrough frame
        stack.push(EnterFrame::passthrough());
        // nested call patched a buffer of its own
        stack.push(hit(0x5000, 1, true));
        assert_eq!(stack.depth(), 2);

        // the inner leave pops the inner frame only
        let inner = stack.pop().expect("inner frame");
        assert_eq!(inner.output_buffer, 0x5000);
        assert!(inner.patch_verified);
        // the outer leave pops the passthrough
        let outer = stack.pop().expect("outer frame");
        assert_eq!(outer.output_buffer, 0);
        assert!(stack.pop().is_none());
    }

    #[test]
    fn an_enter_panic_after_push_still_leaves_a_poppable_passthrough() {
        // enter pushes first, then dies before any decision: leave must pop
        // an empty frame and touch nothing
        let mut stack = FrameStack::default();
        stack.push(EnterFrame::passthrough());
        // (decision panicked here — no mark_hit)
        let frame = stack.pop().expect("frame pushed before the panic");
        assert_eq!(frame, EnterFrame::passthrough());
    }

    #[test]
    fn mark_hit_upgrades_only_the_top_frame() {
        let mut stack = FrameStack::default();
        stack.push(EnterFrame::passthrough());
        stack.push(EnterFrame::passthrough());
        let f = hit(0x6000, 2, false); // verified=false: an unverified write
        stack.mark_hit(f.output_buffer, f.original, f.patched, f.patch_verified);
        let top = stack.pop().expect("top");
        assert_eq!(top.output_buffer, 0x6000);
        assert!(!top.patch_verified);
        let under = stack.pop().expect("under");
        assert_eq!(under, EnterFrame::passthrough());
    }
}
