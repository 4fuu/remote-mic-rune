# Mechanisms

The design catalogue of Remote Mic Rune: what each mechanism does, why it
exists, and the constants that govern it. All timings and caps live in
`rc_protocol::consts` — both ends of every link read the same values, so
none of these budgets can drift between components.

## Report-layer capture

The RC003's BLE reports are submitted from its UMDF host process
(`WUDFHost.exe`) to the kernel through one specific device I/O control:

- **Target IOCTL**: `0x80018483` (UMDF host→kernel HID report submission),
  filtered further by input-buffer bytes 4/5 == `0x02/0x01`.
- **Report**: keyboard report id `0x01`, 9 bytes: report id, modifier byte,
  reserved byte, then three 16-bit usage slots.
- **Slots**: three little-endian `u16` usages at byte offsets **3 / 5 / 7**;
  usage 0 means "empty slot".
- **Selective clearing**: the agent builds a patched copy of the report in
  which only the slots whose usage is in the current clear set are zeroed.
  Bytes 0..3 (report id / modifiers / reserved) are never touched, and
  slots holding unrelated keys pass through unchanged. The patch is written
  to the output buffer *before* calling the original function, so the
  kernel copies the patched report and the cleared usages never become
  keystrokes.
- **Restore after**: once the original call returns, the agent writes the
  original bytes back. If a readback shows the kernel changed the buffer
  meanwhile, the restore is skipped and counted (`kernel_changed`) — the
  buffer is never assumed to still be ours.

Why this layer: Back (`0x00F1`), VolumeUp (`0x0080`) and VolumeDown
(`0x0081`) are dropped by `kbdhid`'s HID→VK mapping before any user-mode
input channel exists. No hook, window message, or input-echo trick can ever
see them; only a report submitted to the kernel still carries them. Captured
keys (from the app's target set) and optional canary keys are cleared here;
the report layer is also the only place the voice key can be intercepted and
re-synthesized.

The hook itself is a hand-rolled validated-pattern inline trampoline on
`ntdll!NtDeviceIoControlFile`. The prologue pattern table is validated
against Win10/Win11 x64 syscall stubs; an unrecognized prologue means no
hook is installed at all and the process log gets an `unsupported_prologue`
line — capture stays off, the remote stays native.

## Lease fail-open

The agent writes report bytes only while a lease from the helper is alive:

- The helper renews every **500 ms** (`LEASE_RENEW_INTERVAL_MS`).
- The lease expires after **2 s** without a renew (`LEASE_MS`).
- Renewals are authenticated: a forged renew without the correct token never
  arms the agent.
- Expiry is detected by time only. Pipe read/write errors are not reliable
  liveness signals on Windows, so "no renew within the window" is the one
  truth, checked on every frame.

When the lease expires (helper killed, hung, or gone), the agent stops
clearing and synthesizing immediately; the remote behaves like a plain
keyboard again within 2 s. There is no code path that leaves a report
modified across a lost lease.

## Per-generation key ownership

The app decides which keys are captured, and the agent acknowledges exactly
what it owns:

- The app sends `T <generation> <usages>` — an absolute target set with a
  monotonically increasing generation number; it resends until acked
  (0.5 / 1.5 / 3 s, then every 5 s).
- The agent applies a retarget only after its own guardrails pass and
  replies with an ownership ack `O <generation> <usages>` (relayed to the
  app).
- The app treats keys as *owned* only while the current generation was acked
  within **1.5 s** (`OWNERSHIP_TIMEOUT_MS`). That timeout is deliberately
  shorter than the agent's 2 s lease, so the native keyboard path always
  recovers before the report layer could still be holding keys.
- Losing ownership releases every pressed key in the gesture engine but
  keeps the link: a stale helper can never keep phantom keys alive.

## Absolute-state edges (self-healing)

The agent never sends press/release deltas. Every change emits `E
<unix_ms> <usages>` carrying the **full pressed set** (empty set = all
released). The app side derives press/release transitions itself by diffing
consecutive sets:

- A lost or delayed line self-heals on the next one — the next absolute set
  replaces all prior state instead of accumulating against it.
- Stuck keys are structurally impossible in the common case: if the link
  dies, the bridge's 3 s silence watchdog releases everything; if the helper
  dies, the lease expires; if the remote reconnects, the next frame restates
  the truth.
- Edges are computed before every gate, so pressed-state truth flows even
  when the lease is expired and nothing may be cleared.

## Safety gates before any buffer write

A report is only ever touched when it is actually relevant: the **union
gate** requires one of the three views to be present — the report set (a
captured button), the clear set (which includes canary keys), or the
synthesis voice key. Gating on the report set alone would silently disable
canary keys and synthesis; the gate is the union of all three, and tests
pin it.

Even with the gate open, four more checks stand between the plan and the
buffer write, each failing open:

1. Lease valid and armed (`lease_ok`) — else count `lease_expired`, write
   nothing.
2. Mode is clear, not observe (`mode_clear`) — observe mode never writes.
3. Per-instance clear cap: at most `MAX_CLEARS` (500 000) successful
   writes; a runaway loop cannot clear reports forever.
4. The patched report must actually differ (`nochange` skip otherwise).

Two post-write checks complete the contract: the agent verifies the buffer
content by readback before counting success (a failed verify counts as a
failed clear), and the post-call restore refuses to stomp on a
kernel-modified buffer.

The whole detour runs inside `catch_unwind`: on any panic the original
function is still called untouched, and an unrecognized ntdll prologue
means the hook is never installed.

## Voice-key synthesis

The remote's voice key (HID usage `0x003E`) is not a mappable button. At
the report layer the agent can replace it with a real keyboard usage so
voice-assistant presses become an ordinary Alt keystroke an application can
listen for:

- **Targets are whitelisted**: only `0x00E2` (left Alt) and `0x00E6` (right
  Alt), both validated on real hardware. Any other target usage is rejected
  at every layer; extending the list requires per-key verification first.
- **In-slot replacement**: the voice key's slot is overwritten with the target
  usage, so the physical release naturally produces the synthesized key's
  UP — a stuck synthesized key is structurally impossible.
- **Gate delay 150 ms**: the press frame is presented this long after the
  edge so the target application has time to finish switching IMEs
  (~53 ms measured on device) before the key lands. 0 disables the gate.
- **Release hold 200 ms**: on physical release the synthesized key's UP is
  held back this long so the application can drain trailing audio.
- Synthesis runs only while capture is armed and the lease is alive, and
  each frame's outcome is traced (rate-limited) and reported to the app.

## Gesture engine timings

One independent state machine per button in `rc-mapping`, ticked at a fixed
25 ms cadence:

- **Double-click window: 300 ms** between two press onsets.
- **Long press: 550 ms** held.
- **Repeat start delay: 350 ms** before a hold-to-repeat stream begins;
  per-button repeat interval afterwards (100 ms for arrows and volume,
  150 ms otherwise).
- If only a single action is configured, it fires immediately on the press
  edge (zero latency); if a double or long binding exists for the button,
  the single waits out its window and confirms on expiry.
- A hold that outlasts the double window concludes as a single (and may
  enter repeat); a second press held past the window cancels a pending
  double.
- Firing actions desktop-wide goes through `SendInput` — chords with
  modifiers, media keys, mouse clicks — with desktop-switching singles
  deferred to physical release so the raw DOWN/UP pair settles first.

## Scheduled-task one-UAC enable

Everything elevated happens in one scheduled task, so the user answers UAC
exactly once per install:

- The app (or `rc-helper --install-task`) registers a task via
  `schtasks /create /f /tn RemoteMicRuneHelper /xml <file>` with
  `<RunLevel>HighestAvailable</RunLevel>` and **no triggers** — Windows
  never starts it on its own; only `schtasks /run` does, which the app
  issues to start/stop the helper without further prompts.
- The task runs the helper with `--follow-app`, so the helper exits when the
  app link goes away and never outlives the session.
- Task state (running/ready/missing) is parsed from `schtasks /query` and
  shown in the UI's Helper tab; `--remove-task` deletes it.

## Device-node recovery with cooldown

When Bluetooth re-enumerates the remote (sleep/wake, toggle, interference),
Windows may spawn a fresh `WUDFHost.exe` — the old host is gone, and with it
the injected agent. The helper detects the loss and repairs it:

- Recovery runs `pnputil /remove-device <instance-id>` followed by
  `pnputil /scan-devices` on the RC003's device node, which forces Windows
  to re-enumerate the device into a fresh host the helper can inject into.
- Every pnputil invocation runs with a hard timeout so it can never hang
  unattended (bounded budgets: remove 120 s + scan 30 s + wait 90 s).
- A **60 s cooldown** (`RECOVERY_COOLDOWN_MS`) sits between attempts; the
  helper reports the recovery window to the app (`R` lines) and the UI
  surfaces "Device-node recovery in progress".

## Token auth

Both links authenticate with a random per-session token, enforced at every
line, not just at handshake:

- The app generates the bridge token and publishes it with the descriptor
  (`bridge.ini`); the helper generates the agent token and passes it in the
  injection handoff config.
- Every authenticated command must carry the token before dispatch — a
  forged lease renewal or retarget without the correct token is rejected.
  This is what makes the fail-open guarantee mean something: only the
  legitimate peer can keep the agent armed.
- The bridge drops a connection after 3 auth failures (`BRIDGE_MAX_DENY`)
  and answers with `DENY <reason>`.
- Single-instance is structural: the app creates the first bridge pipe
  instance with `FILE_FLAG_FIRST_PIPE_INSTANCE`; a second app fails with
  `already_running`. A stale agent pipe cannot outlive its helper because
  the agent pipe name carries the helper's pid.

Every connection is additionally sized and bounded: max line 4096 bytes,
pipe buffers 4096, serialized single-writer output queues with caps, and
watchdogs (auth before rx-silence) that treat "no valid bytes" as a dead
peer.
