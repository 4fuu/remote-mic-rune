# Architecture

Remote Mic Rune is a Windows app that turns the Xiaomi RC003 (2 Pro)
Bluetooth remote into a programmable remote. The interesting part of the
system lives where the remote's keys are most fragile: between the BLE HID
report arriving in the device's UMDF host process and the kernel keyboard
driver deciding what to do with it. This page describes the whole chain and
the process boundaries it crosses.

## The chain, end to end

```
RC003 (BLE HID) ──► WUDFHost.exe ──► kbdhid ──► Windows input
                        │  ▲
             (1) inject │  │ (2) hook NtDeviceIoControlFile:
             rc_agent   │  │    clear mapped usages in keyboard
             .dll       │  │    report 0x01 before the kernel copies it
                        ▼  │
                  rc-helper.exe (elevated, scheduled task)
                        │  ▲ lease renew 500 ms / lease 2 s (fail-open)
                        │  │ JSON lines, per-pid named pipe
                        ▼  │
                   rc-app.exe (Rune UI)
                        │
                   rc-bridge (pipe server, token auth, per-generation
                              key ownership, watchdogs)
                        │
                   rc-mapping (button action → SendInput)
```

1. The RC003 pairs as a BLE keyboard. Windows routes its HID reports through
   a UMDF host process (`WUDFHost.exe`); when the report is submitted from
   there to the kernel, `kbdhid` maps HID usages to virtual keys and drops
   Back (0x00F1), VolumeUp (0x0080) and VolumeDown (0x0081), which have no
   entry in its HID→VK table. Anything a normal application could observe —
   window messages, `SendInput` echoes, low-level hooks — only ever sees
   what `kbdhid` let through.

2. The elevated helper (`rc-helper.exe`) scans the registry for every device
   instance in the PnP `Enum` tree whose `Device Parameters\WUDFDiagnosticInfo`
   carries a `HostPid` value (a BLE-paired RC003 shows up as a
   `BTHLEDEVICE\{00001812-...}` HID-over-GATT instance, not under `Enum\HID`),
   decides which `WUDFHost.exe` exclusively hosts the
   RC003, and injects `rc_agent.dll` into that process. Injection uses
   `VirtualAllocEx` / `WriteProcessMemory` / `CreateRemoteThread`; the agent
   bootstraps itself from a handoff config the helper passes in.

3. Inside `WUDFHost.exe`, the agent installs a manual inline hook on
   `ntdll!NtDeviceIoControlFile` and watches for the UMDF host→kernel HID
   report submission IOCTL (`0x80018483`). For matching submissions carrying
   keyboard report 0x01, it clears the usages the app asked for (three
   little-endian 16-bit slots at offsets 3/5/7) before calling the original
   function — the kernel copies the patched report, so the cleared keys
   never become keystrokes. The agent also reports the full pressed set on
   every change, so the app side can re-inject what it wants, and it can
   synthesize the voice key (0x003E) into a real Alt usage at the same
   layer.

4. The helper relays between the agent link and the app link, renews the
   agent's lease every 500 ms, and owns the scheduled task, logging, and
   device-node recovery (when Bluetooth re-enumerates the remote and the
   host process is replaced, the helper re-injects).

5. The app (`rc-app.exe`) hosts the bridge: a named-pipe server the helper
   connects to, token-authenticated, with per-generation key ownership and
   watchdogs. Every press edge the bridge produces goes into the mapping
   engine, which fires that button's one configured action with `SendInput`
   — chords, media keys, mouse clicks.

## Process and component boundaries

Three OS processes, each with a strict role:

| Process | Elevated | Contains | Trust boundary |
| ------- | -------- | -------- | -------------- |
| `WUDFHost.exe` (the RC003's host) | system | `rc_agent.dll` (injected) | Modifies HID reports at the syscall boundary; acts only while its lease lives. |
| `rc-helper.exe` | yes (scheduled task, RunLevel Highest) | locate + inject + lease + relay + recovery | The only component that writes into another process; refuses shared/ambiguous hosts. |
| `rc-app.exe` | no | Rune UI, bridge server, mapping engine, SendInput | The only component that talks to the desktop; everything user-visible happens here. |

Pure logic is kept out of the Windows-only processes on purpose: `rc-agent`'s
decision layer (`report`, `state`, `clock`) and `rc-bridge`'s state machine
are host-testable libraries, exercised by `cargo test` on any OS; only the
thin glue (`hook`, `transport`, `pipe_windows`, injection) is `cfg(windows)`.

## The two transports

Both links are named pipes; no TCP anywhere (loopback TCP is at the mercy of
VPN/TUN drivers redirecting loopback SYN packets).

- **agent ↔ helper** — `\\.\pipe\RemoteMicRune.Agent.<helper-pid>`. The
  helper passes the full name plus the auth token to the agent at injection
  time (handoff config). JSON lines (`rc_protocol::agent_wire`): config and
  retargets downlink; absolute pressed sets, stats heartbeats, lease renew
  acknowledgements, synthesis frame results uplink. A per-pid pipe name
  means a stale pipe from a dead helper can never be confused with the
  current one.
- **helper ↔ app** — `\\.\pipe\RemoteMicRune.Bridge.v1`. The app creates the
  first pipe instance (which also enforces single-instance), publishes
  `bridge.ini` under `%LOCALAPPDATA%\RemoteMicRune\` with pipe name, token
  and pid; the helper reads the descriptor, connects, and speaks the ASCII
  line protocol (`rc_protocol::bridge_wire`): `HELLO/OK` auth, absolute
  pressed sets (`E`), ping (`P`), retargets (`T`), ownership acks (`O`),
  synthesis control (`S`/`A`/`F`), recovery state (`R`), goodbye (`BYE`).

Both pipes carry a token that must match on every authenticated line; both
use a `Everyone` DACL because they are strictly local — the token, not the
DACL, is the access control.

## Workspace layout

| Crate | Kind | Role |
| ----- | ---- | ---- |
| `crates/rc-protocol` | lib | Button usage table, both wire formats, every timing/safety constant. No Windows APIs, no unsafe; builds and tests on any host. |
| `crates/rc-agent` | cdylib (+rlib for tests) | The injected agent: IOCTL hook, report patch planning, lease state machine, voice-key synthesis. Windows-only glue is cfg-gated; the decision layer is pure. |
| `crates/rc-helper` | bin | Registry scan and exclusive-host verdict, DLL injection, lease renewal, agent↔app relay, scheduled task management, device-node recovery, CLI for staged verification. |
| `crates/rc-bridge` | lib | App-side link endpoint: pipe server, token auth, per-generation ownership, watchdogs, snapshot for the UI. |
| `crates/rc-mapping` | lib | Per-button action model (shortcuts/media/mouse), SendInput injector. |
| `crates/rc-app` | bin | Rune UI shell: five sections (Status/Capture/Buttons/Helper/Settings), settings persistence, engine pump, tray. |

Dependency direction is strict: `rc-protocol` depends on nothing internal;
`rc-agent`, `rc-bridge`, `rc-mapping`, `rc-helper` depend on `rc-protocol`;
`rc-app` depends on `rc-bridge` + `rc-mapping` + `rc-protocol` (and the Rune
UI framework). The wire and timing contracts live only in `rc-protocol`, so
the two ends of a link cannot drift apart.

## Failure model

Every component assumes every other component can vanish:

- The agent acts only while the 500 ms-renewed lease is alive; no renew for
  2 s → stop touching reports, the remote is native again.
- The helper exits when the app link goes away (`--follow-app` under the
  scheduled task); the agent's lease then expires on its own.
- The bridge releases all pressed state after 3 s of helper silence and
  keeps listening; a new helper connection always wins.
- Key ownership is tracked per generation with a 1.5 s ack timeout — it
  expires *before* the agent's 2 s lease so the native keyboard path always
  recovers first.
- Any panic inside the agent unwinds into calling the original function
  untouched, and a failed hook installation leaves the process unhooked;
  reports are patched only after an 8-point safety gate passes.

See [mechanisms.md](mechanisms.md) for each of these in detail.
