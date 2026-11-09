//! Pure report-layer decision logic for the RC003 keyboard report
//! (report id 0x01, three little-endian 16-bit usage slots at offsets 3/5/7).
//!
//! Everything here is platform-independent so `cargo test` covers it on any
//! host; the Windows hook layer only executes the returned plan.
//!
//! The one gate that must never regress: a report is *touched* when ANY of
//! the three views — report set (edge reporting), clear set (canary keys
//! included), or the synthesis voice key — is present. Gating on the report
//! view alone would silently disable the canary feature: reports containing
//! only a canary key (or only the voice key) would early-exit and the
//! canary would be dead code.

use rc_protocol::agent_wire::AgentStats;
use rc_protocol::consts::{MAX_CLEARS, RELEASE_HOLD_MS, REPORT_LEN, USAGE_SLOT_OFFSETS};

/// Classification of one report frame from the synthesis point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SynthFrame {
    /// Voice key absent and no replacement pending.
    #[default]
    None,
    /// Voice key present, no replacement held: the press frame.
    Press,
    /// Voice key present and a replacement is still held (continuation).
    Held,
    /// Voice key absent but a replacement is still held: the physical
    /// release frame (the zero report that must be held back briefly).
    Release,
}

/// The patch to apply to a report, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchPlan {
    /// Full 9-byte report as it must be written to the output buffer.
    pub patched: [u8; REPORT_LEN],
    /// True when a slot was overwritten with the synthesis usage (not just zeroed).
    pub synth_changed: bool,
}

/// Read one 16-bit little-endian usage slot from the report.
pub fn slot_usage(report: &[u8; REPORT_LEN], slot: usize) -> u16 {
    let off = USAGE_SLOT_OFFSETS[slot];
    u16::from(report[off]) | (u16::from(report[off + 1]) << 8)
}

/// All three slot usages (0 = empty slot).
pub fn slot_usages(report: &[u8; REPORT_LEN]) -> [u16; 3] {
    [
        slot_usage(report, 0),
        slot_usage(report, 1),
        slot_usage(report, 2),
    ]
}

/// Sorted, de-duplicated, non-zero usages of `slots` that are members of `set`.
fn usages_in(slots: &[u16; 3], set: &[u16]) -> Vec<u16> {
    let mut out: Vec<u16> = slots
        .iter()
        .copied()
        .filter(|u| *u != 0 && set.contains(u))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The report view: target usages to report as edges.
pub fn view_report_set(report: &[u8; REPORT_LEN], report_usages: &[u16]) -> Vec<u16> {
    usages_in(&slot_usages(report), report_usages)
}

/// The clear view: usages to scrub from the report. The clear set is a
/// superset of the report set and may carry extra canary keys, so this view
/// is distinct from [`view_report_set`] by design.
pub fn view_clear_set(report: &[u8; REPORT_LEN], clear_usages: &[u16]) -> Vec<u16> {
    usages_in(&slot_usages(report), clear_usages)
}

/// The synthesis view: is the voice key present and waiting for replacement?
pub fn view_synth(report: &[u8; REPORT_LEN], synth_from: u16) -> bool {
    synth_from != 0 && slot_usages(report).contains(&synth_from)
}

/// The union gate. Must consider all three views; see the module docs.
pub fn should_touch_report(
    report_set_in: bool,
    clear_set_in: bool,
    synth_in: bool,
) -> bool {
    report_set_in || clear_set_in || synth_in
}

/// Absolute pressed set: report-view usages present in this frame, sorted.
pub fn pressed_usages(report: &[u8; REPORT_LEN], report_usages: &[u16]) -> Vec<u16> {
    view_report_set(report, report_usages)
}

/// Edge derivation on absolute sets: `Some(next)` only when the pressed set
/// actually changed. `next` is already sorted.
pub fn edge_changed(current: &[u16], next: &[u16]) -> Option<Vec<u16>> {
    if current == next {
        None
    } else {
        Some(next.to_vec())
    }
}

/// Build the patched report: zero the two bytes of every slot whose usage is
/// in the clear set; replace the voice-key slot with the synthesis usage when
/// synthesis is configured. Bytes 0..3 (report id / modifiers / reserved) are
/// never touched. `None` when the patch would change nothing.
pub fn patch_report(
    report: &[u8; REPORT_LEN],
    clear_usages: &[u16],
    synth_from: u16,
    synth_to: u16,
) -> Option<PatchPlan> {
    let mut patched = *report;
    let mut changed = false;
    let mut synth_changed = false;
    for &off in USAGE_SLOT_OFFSETS.iter() {
        let u = u16::from(report[off]) | (u16::from(report[off + 1]) << 8);
        if u == 0 {
            continue;
        }
        if synth_from != 0 && u == synth_from {
            // In-slot replacement: the physical release naturally zeroes the
            // slot, which HID state semantics read as the synthesized key's
            // UP — a stuck key is structurally impossible.
            patched[off] = (synth_to & 0xff) as u8;
            patched[off + 1] = (synth_to >> 8) as u8;
            changed = true;
            synth_changed = true;
            continue;
        }
        if clear_usages.contains(&u) {
            patched[off] = 0;
            patched[off + 1] = 0;
            changed = true;
        }
    }
    changed.then_some(PatchPlan {
        patched,
        synth_changed,
    })
}

/// Classify the current frame from the synthesis point of view.
pub fn classify_synth_frame(synth_from: u16, synth_in: bool, synth_held: bool) -> SynthFrame {
    if synth_from == 0 {
        return SynthFrame::None;
    }
    match (synth_in, synth_held) {
        (true, false) => SynthFrame::Press,
        (true, true) => SynthFrame::Held,
        (false, true) => SynthFrame::Release,
        (false, false) => SynthFrame::None,
    }
}

/// Everything the detour needs to know about one frame, as inputs.
#[derive(Debug, Clone, Copy)]
pub struct FrameInputs<'a> {
    pub report: &'a [u8; REPORT_LEN],
    pub report_usages: &'a [u16],
    pub clear_usages: &'a [u16],
    pub synth_from: u16,
    pub synth_to: u16,
    /// Whether the previous frame actually landed a replacement (the OS still
    /// believes the synthesized key is down).
    pub synth_held: bool,
    /// Current press-frame presentation delay; 0 = gate disabled.
    pub gate_delay_ms: u32,
    /// Lease valid and armed right now (renewed within LEASE_MS, connected,
    /// not disarmed).
    pub lease_ok: bool,
    /// false = observe mode: never write a single byte.
    pub mode_clear: bool,
    /// Current clears_ok counter, checked against MAX_CLEARS.
    pub clears_ok: u64,
    /// Current absolute pressed set (sorted).
    pub pressed: &'a [u16],
}

/// The action plan for one frame. Counter fields are deltas to add.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameDecision {
    /// `Some(sorted set)` when the absolute pressed set changed: emit an
    /// Edge (reason "state"). Computed before any gate, so edges flow even
    /// when the lease is expired — report truth first.
    pub edge: Option<Vec<u16>>,
    pub edge_sent: u64,
    /// Release-hold delay to sleep before letting a physical-release frame
    /// through (0 = none).
    pub release_hold_ms: u32,
    /// Press-frame presentation delay to sleep before patching (0 = none).
    pub gate_delay_ms: u32,
    /// The patch to write, if any.
    pub patch: Option<PatchPlan>,
    pub synth_frame: SynthFrame,
    /// Set when the release branch consumed a held replacement: clear the
    /// held flag after this plan is decided.
    pub consume_held: bool,
    /// Rate-limited trace reason for a synthesis frame that ends untouched.
    pub synth_skip_reason: Option<&'static str>,
    // Counter deltas.
    pub target_hits: u64,
    pub canary_hits: u64,
    pub lease_expired: u64,
}

/// Decide what to do with one intercepted report. The onEnter ordering is
/// fixed:
/// edges first, then the release hold, then the union gate, then the
/// lease / mode / cap gates, then the press-frame gate delay, then patching.
pub fn plan_frame(inp: &FrameInputs) -> FrameDecision {
    let slots = slot_usages(inp.report);
    let report_in = usages_in(&slots, inp.report_usages);
    let clear_in = usages_in(&slots, inp.clear_usages);
    let synth_in = view_synth(inp.report, inp.synth_from);
    let synth_frame = classify_synth_frame(inp.synth_from, synth_in, inp.synth_held);

    let mut d = FrameDecision {
        synth_frame,
        ..FrameDecision::default()
    };

    // 1. Absolute-state edges: emitted regardless of lease/mode/caps.
    let next = pressed_usages(inp.report, inp.report_usages);
    d.edge = edge_changed(inp.pressed, &next);
    d.edge_sent = u64::from(d.edge.is_some());

    // 2. Voice-key release hold: the frame that ends a replacement must be
    //    delayed even when it carries no target keys, so this runs before the
    //    union gate. Armed-and-clear only; any doubt = fail open.
    if synth_frame == SynthFrame::Release && inp.lease_ok && inp.mode_clear {
        d.release_hold_ms = RELEASE_HOLD_MS;
        d.consume_held = true;
    }

    // 3. The union gate (report ∪ clear ∪ synth).
    if !should_touch_report(!report_in.is_empty(), !clear_in.is_empty(), synth_in) {
        return d;
    }

    if !report_in.is_empty() {
        d.target_hits = 1;
    }
    // Canary detection: a cleared usage that is not part of the report set.
    if clear_in.iter().any(|u| !inp.report_usages.contains(u)) {
        d.canary_hits = 1;
    }

    // 4. Write gates: fail open on the first doubt.
    if !inp.lease_ok {
        d.lease_expired = 1;
        d.synth_skip_reason = synth_in.then_some("lease");
        return d;
    }
    if !inp.mode_clear {
        d.synth_skip_reason = synth_in.then_some("mode_observe");
        return d;
    }
    if inp.clears_ok >= MAX_CLEARS {
        d.synth_skip_reason = synth_in.then_some("clear_cap");
        return d;
    }

    // 5. Press-frame presentation delay: the app switched IMEs only after it
    //    received the edge, so the synthesized DOWN frame waits for it.
    //    Applies to the press frame; release frames are held in step 2.
    if synth_frame == SynthFrame::Press && inp.gate_delay_ms > 0 {
        d.gate_delay_ms = inp.gate_delay_ms;
    }

    // 6. Patch.
    d.patch = patch_report(inp.report, inp.clear_usages, inp.synth_from, inp.synth_to);
    if d.patch.is_none() {
        d.synth_skip_reason = synth_in.then_some("nochange");
    }
    d
}

/// Fold the plan's counter deltas into the stats.
pub fn apply_frame_counters(stats: &mut AgentStats, d: &FrameDecision) {
    stats.target_hits += d.target_hits;
    stats.canary_hits += d.canary_hits;
    stats.lease_expired += d.lease_expired;
    stats.edges_sent += d.edge_sent;
    if d.gate_delay_ms > 0 {
        stats.gate_delays += 1;
    }
}

/// The write landed and verified: count it, and count a synthesis hit.
pub fn apply_write_ok(stats: &mut AgentStats, synth_changed: bool) {
    stats.clears_ok += 1;
    if synth_changed {
        stats.synth_hits += 1;
    }
}

/// The write failed or the readback disagreed: the clear did not happen.
pub fn apply_write_fail(stats: &mut AgentStats) {
    stats.clears_fail += 1;
}

/// Post-call restore outcome: `true` = original bytes written back,
/// `false` = the kernel changed the buffer meanwhile, left untouched.
pub fn apply_restore_outcome(stats: &mut AgentStats, restored: bool) {
    if restored {
        stats.restores_ok += 1;
    } else {
        stats.kernel_changed += 1;
    }
}

/// What the leave callback must do with a finished invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveAction {
    /// Leave the buffer alone, count nothing.
    Nothing,
    /// The kernel rewrote the buffer during the call: skip the restore,
    /// count `kernel_changed`.
    KernelChanged,
    /// The buffer still holds exactly our patched bytes: write the original
    /// content back, count `restores_ok`.
    Restore,
}

/// The post-call restore decision. A patch that never landed (readback
/// mismatch) or restore disabled means no bookkeeping at all; a verified
/// patch is restored only while the buffer still contains exactly the bytes
/// we wrote — the kernel may legitimately have moved underneath us. The
/// comparison is against the patched content, never the original report:
/// every plan changes at least one byte, so comparing the wrong side would
/// restore over a kernel-modified buffer.
pub fn restore_decision(
    patch_verified: bool,
    restore_enabled: bool,
    current: &[u8; REPORT_LEN],
    patched: &[u8; REPORT_LEN],
) -> LeaveAction {
    if !patch_verified || !restore_enabled {
        return LeaveAction::Nothing;
    }
    if current == patched {
        LeaveAction::Restore
    } else {
        LeaveAction::KernelChanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_protocol::buttons::{VOICE_KEY_HID_USAGE, VOICE_SYNTHABLE_USAGES};
    use rc_protocol::consts::GATE_DELAY_MS;

    const REPORT_SET: &[u16] = &[0x00F1, 0x0080]; // back, volume_up
    const CLEAR_SET: &[u16] = &[0x00F1, 0x0080, 0x004A]; // + home as canary
    const SYNTH_FROM: u16 = VOICE_KEY_HID_USAGE;
    const SYNTH_TO: u16 = 0x00E6;

    fn report_with(slots: [u16; 3]) -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[0] = 0x01;
        r[1] = 0x00; // modifiers
        r[2] = 0x00; // reserved
        for (i, &u) in slots.iter().enumerate() {
            let off = USAGE_SLOT_OFFSETS[i];
            r[off] = (u & 0xff) as u8;
            r[off + 1] = (u >> 8) as u8;
        }
        r
    }

    fn base_inp<'a>(
        report: &'a [u8; REPORT_LEN],
        pressed: &'a [u16],
        held: bool,
        lease_ok: bool,
    ) -> FrameInputs<'a> {
        FrameInputs {
            report,
            report_usages: REPORT_SET,
            clear_usages: CLEAR_SET,
            synth_from: SYNTH_FROM,
            synth_to: SYNTH_TO,
            synth_held: held,
            gate_delay_ms: GATE_DELAY_MS,
            lease_ok,
            mode_clear: true,
            clears_ok: 0,
            pressed,
        }
    }

    #[test]
    fn slots_decode_little_endian_at_fixed_offsets() {
        let r = report_with([0x1234, 0, 0xABCD]);
        assert_eq!(slot_usages(&r), [0x1234, 0, 0xABCD]);
        // bytes 0..3 must stay untouched by every helper
        assert_eq!(&r[0..3], &[0x01, 0x00, 0x00]);
    }

    #[test]
    fn views_split_report_clear_and_synth() {
        let r = report_with([0x004A, 0x00F1, SYNTH_FROM]);
        assert_eq!(view_report_set(&r, REPORT_SET), vec![0x00F1]);
        assert_eq!(view_clear_set(&r, CLEAR_SET), vec![0x004A, 0x00F1]);
        assert!(view_synth(&r, SYNTH_FROM));
        assert!(!view_synth(&r, 0));
    }

    #[test]
    fn gate_is_the_union_of_all_three_views() {
        // report-set key only
        let r = report_with([0x00F1, 0, 0]);
        assert!(should_touch_report(true, false, false));
        let v = view_report_set(&r, REPORT_SET);
        assert!(should_touch_report(!v.is_empty(), false, false));

        // canary-only report: a report-view-only gate would early-exit here
        let r = report_with([0x004A, 0, 0]);
        let v = view_clear_set(&r, CLEAR_SET);
        assert!(!v.is_empty());
        assert!(should_touch_report(false, true, false));
        assert!(plan_frame(&base_inp(&r, &[], false, true)).patch.is_some());

        // voice-only report: the same bug class for synthesis
        let r = report_with([SYNTH_FROM, 0, 0]);
        let d = plan_frame(&base_inp(&r, &[], false, true));
        assert!(d.patch.is_some());
        assert_eq!(d.synth_frame, SynthFrame::Press);

        // nothing relevant: gate closed, no patch, no counters
        let r = report_with([0x0052, 0, 0]); // up: not in any set
        let d = plan_frame(&base_inp(&r, &[], false, true));
        assert!(d.patch.is_none());
        assert_eq!(d.target_hits + d.canary_hits, 0);
    }

    #[test]
    fn empty_slots_never_touch() {
        let r = report_with([0, 0, 0]);
        let d = plan_frame(&base_inp(&r, &[], false, true));
        assert!(d.patch.is_none());
        assert_eq!(d.edge, None);
        assert_eq!(d.edge_sent, 0);
    }

    #[test]
    fn usage_zero_is_never_a_member_of_any_set() {
        let r = report_with([0, 0, 0]);
        assert!(view_report_set(&r, &[0]).is_empty());
        assert!(view_clear_set(&r, &[0, 0x00F1]).is_empty());
    }

    #[test]
    fn patch_zeroes_only_target_slot_bytes() {
        let r = report_with([0x00F1, 0x004A, 0x0052]);
        let p = patch_report(&r, CLEAR_SET, 0, 0).expect("patch expected");
        assert_eq!(p.patched[0], 0x01);
        assert_eq!(p.patched[1], 0x00);
        assert_eq!(p.patched[2], 0x00);
        assert_eq!(&p.patched[3..9], &[0, 0, 0, 0, 0x52, 0x00]); // home+back zeroed, up untouched
        assert!(!p.synth_changed);
    }

    #[test]
    fn patch_replaces_voice_slot_with_synth_usage() {
        let r = report_with([SYNTH_FROM, 0, 0]);
        let p = patch_report(&r, &[], SYNTH_FROM, SYNTH_TO).expect("patch expected");
        assert_eq!(p.patched[3], (SYNTH_TO & 0xff) as u8);
        assert_eq!(p.patched[4], (SYNTH_TO >> 8) as u8);
        assert!(p.synth_changed);
    }

    #[test]
    fn patch_may_zero_and_replace_in_one_frame() {
        let r = report_with([SYNTH_FROM, 0x004A, 0]);
        let p = patch_report(&r, CLEAR_SET, SYNTH_FROM, SYNTH_TO).expect("patch expected");
        assert_eq!(&p.patched[3..9], &[0xE6, 0x00, 0, 0, 0, 0]);
        assert!(p.synth_changed);
    }

    #[test]
    fn patch_returns_none_when_nothing_matches() {
        let r = report_with([0x0052, 0, 0]);
        assert!(patch_report(&r, CLEAR_SET, 0, 0).is_none());
        let empty: [u8; REPORT_LEN] = report_with([0, 0, 0]);
        assert!(patch_report(&empty, CLEAR_SET, SYNTH_FROM, SYNTH_TO).is_none());
    }

    #[test]
    fn synth_targets_come_from_the_contract_whitelist() {
        assert!(VOICE_SYNTHABLE_USAGES.contains(&0x00E6));
        assert!(VOICE_SYNTHABLE_USAGES.contains(&0x00E2));
        assert_eq!(SYNTH_FROM, 0x003E);
    }

    #[test]
    fn pressed_set_is_absolute_sorted_and_deduped() {
        let r = report_with([0x0080, 0x00F1, 0x0080]);
        // ascending numeric order, deduplicated
        assert_eq!(pressed_usages(&r, REPORT_SET), vec![0x0080, 0x00F1]);
    }

    #[test]
    fn edges_fire_only_on_change() {
        assert_eq!(edge_changed(&[0x00F1], &[0x00F1]), None);
        assert_eq!(edge_changed(&[], &[0x00F1]), Some(vec![0x00F1]));
        assert_eq!(edge_changed(&[0x00F1, 0x0080], &[0x00F1]), Some(vec![0x00F1]));
    }

    #[test]
    fn synth_frame_classification() {
        assert_eq!(classify_synth_frame(SYNTH_FROM, true, false), SynthFrame::Press);
        assert_eq!(classify_synth_frame(SYNTH_FROM, true, true), SynthFrame::Held);
        assert_eq!(classify_synth_frame(SYNTH_FROM, false, true), SynthFrame::Release);
        assert_eq!(classify_synth_frame(SYNTH_FROM, false, false), SynthFrame::None);
        assert_eq!(classify_synth_frame(0, false, false), SynthFrame::None);
    }

    #[test]
    fn lease_expired_skips_writes_but_still_reports_edges() {
        let r = report_with([0x00F1, 0, 0]);
        let d = plan_frame(&base_inp(&r, &[], false, false));
        assert!(d.patch.is_none());
        assert_eq!(d.lease_expired, 1);
        assert_eq!(d.edge, Some(vec![0x00F1])); // truth first
        assert_eq!(d.release_hold_ms, 0);
    }

    #[test]
    fn observe_mode_reports_edges_but_never_writes() {
        let r = report_with([0x00F1, 0, 0]);
        let mut inp = base_inp(&r, &[], false, true);
        inp.mode_clear = false;
        let d = plan_frame(&inp);
        assert!(d.patch.is_none());
        assert_eq!(d.lease_expired, 0);
        assert_eq!(d.edge, Some(vec![0x00F1]));
    }

    #[test]
    fn clears_cap_stops_writes_fail_open() {
        let r = report_with([0x00F1, 0, 0]);
        let mut inp = base_inp(&r, &[], false, true);
        inp.clears_ok = MAX_CLEARS;
        let d = plan_frame(&inp);
        assert!(d.patch.is_none());
        let mut inp = base_inp(&r, &[], false, true);
        inp.clears_ok = MAX_CLEARS - 1;
        assert!(plan_frame(&inp).patch.is_some());
    }

    #[test]
    fn gate_delay_applies_to_press_frames_only() {
        let press = report_with([SYNTH_FROM, 0, 0]);
        let d = plan_frame(&base_inp(&press, &[], false, true));
        assert_eq!(d.gate_delay_ms, GATE_DELAY_MS);

        let held = report_with([SYNTH_FROM, 0, 0]);
        let d = plan_frame(&base_inp(&held, &[], true, true));
        assert_eq!(d.gate_delay_ms, 0);
        assert_eq!(d.synth_frame, SynthFrame::Held);

        let plain = report_with([0x00F1, 0, 0]);
        let d = plan_frame(&base_inp(&plain, &[], false, true));
        assert_eq!(d.gate_delay_ms, 0);

        // gate disabled end to end
        let mut inp = base_inp(&press, &[], false, true);
        inp.gate_delay_ms = 0;
        assert_eq!(plan_frame(&inp).gate_delay_ms, 0);
    }

    #[test]
    fn release_hold_applies_only_when_armed_and_pending() {
        let released = report_with([0, 0, 0]);
        let d = plan_frame(&base_inp(&released, &[], true, true));
        assert_eq!(d.release_hold_ms, RELEASE_HOLD_MS);
        assert_eq!(d.synth_frame, SynthFrame::Release);
        assert!(d.consume_held);

        // lease gone: no hold, held flag left alone (fail-open passthrough)
        let d = plan_frame(&base_inp(&released, &[], true, false));
        assert_eq!(d.release_hold_ms, 0);
        assert!(!d.consume_held);

        // observe mode: no hold
        let mut inp = base_inp(&released, &[], true, true);
        inp.mode_clear = false;
        assert_eq!(plan_frame(&inp).release_hold_ms, 0);

        // nothing held: plain empty report passes straight through
        let d = plan_frame(&base_inp(&released, &[], false, true));
        assert_eq!(d.release_hold_ms, 0);
    }

    #[test]
    fn counter_helpers_fill_the_reference_stats() {
        let mut stats = AgentStats::default();
        let press = report_with([SYNTH_FROM, 0, 0]);
        let d = plan_frame(&base_inp(&press, &[], false, true));
        apply_frame_counters(&mut stats, &d);
        assert_eq!(stats.gate_delays, 1);
        apply_write_ok(&mut stats, true);
        assert_eq!(stats.clears_ok, 1);
        assert_eq!(stats.synth_hits, 1);
        apply_write_fail(&mut stats);
        assert_eq!(stats.clears_fail, 1);
        apply_restore_outcome(&mut stats, true);
        apply_restore_outcome(&mut stats, false);
        assert_eq!(stats.restores_ok, 1);
        assert_eq!(stats.kernel_changed, 1);
    }

    #[test]
    fn restore_never_touches_an_unverified_or_disabled_hit() {
        let r = report_with([SYNTH_FROM, 0, 0]);
        let p = patch_report(&r, &[], SYNTH_FROM, SYNTH_TO).expect("patch expected");
        // readback failed after the write: nothing may be counted or written
        assert_eq!(
            restore_decision(false, true, &p.patched, &p.patched),
            LeaveAction::Nothing
        );
        // restore switched off after the patch landed: leave the buffer alone
        assert_eq!(
            restore_decision(true, false, &p.patched, &p.patched),
            LeaveAction::Nothing
        );
    }

    #[test]
    fn restore_writes_original_back_only_over_our_own_patched_bytes() {
        let r = report_with([SYNTH_FROM, 0x004A, 0]);
        let p = patch_report(&r, CLEAR_SET, SYNTH_FROM, SYNTH_TO).expect("patch expected");
        // asymmetric setup: the patch always differs from the original
        assert_ne!(p.patched, r);
        // buffer still exactly ours: restore the original 9 bytes
        assert_eq!(
            restore_decision(true, true, &p.patched, &p.patched),
            LeaveAction::Restore
        );
        // kernel touched byte 1 (OS-owned) during the call: give up and
        // count kernel_changed — comparing against the ORIGINAL report
        // would wrongly restore here
        let mut kernel_touched = p.patched;
        kernel_touched[1] = 0x20;
        assert_ne!(kernel_touched, r);
        assert_eq!(
            restore_decision(true, true, &kernel_touched, &p.patched),
            LeaveAction::KernelChanged
        );
    }
}
