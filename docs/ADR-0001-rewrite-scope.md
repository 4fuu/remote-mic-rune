# ADR 0001 — Rewrite scope and mechanism parity

Date: 2026-11-08
Status: Accepted

## Context

The reference product (GetSayAll/remote-mic-app-windows, "无线麦 SayAll") is a
Tauri 2 + Vue 3 app with two capture tracks:

1. **Basic track** (no admin): Raw Input observer + `WH_KEYBOARD_LL` gate in a
   separate host process + F5 suppressor. Works for RC001 remotes and for the
   RC003 keys Windows maps natively.
2. **Enhanced track** (one UAC): elevated helper injects into the RC003's
   `WUDFHost.exe`, intercepts the UMDF host→kernel report submission IOCTL
   (`0x80018483`) at the `NtDeviceIoControlFile` boundary, selectively clears
   mapped usages from keyboard report 0x01, reports absolute-state edges, and
   remaps through a gesture engine + `SendInput`.

Reference investigations established (docs/decisions + investigations in that
repo): kernel drivers need attestation signing; every injection API carries
`LLKHF_INJECTED`; RC003's Back/VolumeUp/VolumeDown usages (0x00F1/0x0080/0x0081)
are dropped by `kbdhid`'s HID→VK table, so only the report layer can see them.

## Decision

remote-mic-rune keeps **only the enhanced track** and rewrites it as original
Rust code, UI on the Rune framework.

### In scope (mechanism parity required)

| # | Reference mechanism | This repo |
| - | ------------------- | --------- |
| 1 | Locate exclusive `WUDFHost.exe` via `Device Parameters\WUDFDiagnosticInfo\HostPid` (type/width-aware read, exclusive-host verdict) | `rc-helper::registry`, `rc-helper::host` |
| 2 | DLL injection `VirtualAllocEx`/`WriteProcessMemory`/`CreateRemoteThread` (Frida Gadget in reference) | `rc-helper::inject` + our own `rc-agent` DLL |
| 3 | `ntdll!NtDeviceIoControlFile` hook: IOCTL `0x80018483`, in[4]==2/in[5]==1, report id 0x01, three LE u16 usage slots at offsets 3/5/7, selective slot zeroing, pre-call patch + post-call restore | `rc-agent::hook`, `rc-agent::report` |
| 4 | Lease fail-open: renew 500 ms, lease 2 s, no renew → stop clearing | both ends via `rc-protocol::consts` |
| 5 | Safety gates: 8 preconditions before any buffer write, MAX_CLEARS cap, verify-after-write, kernel-change detection | `rc-agent::report` |
| 6 | Dynamic targets with per-key ownership: T line + generation + agent ACK (O line), ownership timeout 1500 ms < lease 2000 ms, resend schedule | `rc-bridge` ↔ `rc-helper` ↔ `rc-agent` |
| 7 | Absolute-state edges (full pressed set, empty = released) → self-healing | `rc-agent` → `rc-bridge` |
| 8 | Canary key (`--canary-usage`, clear-only, e.g. Home 0x4A) so interception is externally observable | `rc-helper` |
| 9 | Report-layer voice-key synthesis (0x003E → 0x00E6/0x00E2), whitelist-only, press gate delay 150 ms, release hold 200 ms | `rc-agent`, `S/A/F/W` lines |
| 10 | Gesture engine: click/double(300 ms)/hold(550 ms)/repeat(350 ms+interval), immediate single when isolated | `rc-mapping::gestures` |
| 11 | `SendInput` injection (chords, media keys, mouse) | `rc-mapping::inject` |
| 12 | One-UAC enable via scheduled task (XML registration, `/RL HIGHEST`, manual trigger, `schtasks /run`) | `rc-helper::task` |
| 13 | Device-node recovery: `pnputil /remove-device` + `/scan-devices`, R-line window, cooldown | `rc-helper::recovery` |
| 14 | Token auth on both links, deny limit, single-instance via first-pipe-instance | `rc-bridge`, `rc-helper` |

### Out of scope (deliberate drops)

- Basic track: Raw Input observer, LL-hook gate/key host process, F5
  suppressor. Single-track design: **helper off = remote behaves natively**.
- ATVV voice sessions, WinRT BLE/GATT stack, VB-CABLE integration (voice input
  path). The synthesis mechanism (9) is kept so a future voice track can use
  it.
- Auto-updater, diagnostics UI beyond a status panel, RC001 adaptation.

### Deviations from the reference (all fail-open preserving)

1. **Own DLL instead of Frida Gadget**: the reference embeds Frida Gadget
   (GPL/commercial license) + a JS agent. We inject our own MIT-licensed Rust
   DLL hooking the same function. Removes the JS runtime and the license
   issue; the hook is a hand-rolled validated-pattern trampoline (see
   `rc-agent::hook`) because no maintained pure-Rust detour library
   cross-compiles cleanly; unsupported ntdll prologue → no hook, native
   behavior, log line (`unsupported_prologue`).
2. **Named pipes for both links** (agent↔helper used loopback TCP in the
   reference because Frida's socket API required it; helper↔app moved to a
   pipe in the reference anyway after Clash/Meta TUN redirected loopback SYN).
   Same line protocols, same token checks, `Everyone` DACL on loopback pipes
   (local-only transport, token still enforced).
3. **Bridge protocol version 1**: line grammar identical to the reference's
   (`HELLO/OK/T/O/S/A/E/P/R/F/BYE/DENY/W`), but a new version number — the two
   products are not wire-compatible by design.
4. **Gate declaration (`W 1`) default off** at launch, matching the
   reference's own `VOICE_GATE_DECLARE = false` rollback state.

## Licensing

- Reference repo: GPL-3.0. We read it for mechanism facts (IOCTL numbers,
  report layout, timings, protocol semantics — uncopyrightable facts) and
  write original code. No file, function body, or comment is copied. Every
  crate states this in its module docs.
- Frida Gadget: not used, not embedded.
- Rune: MIT. This repo: MIT.

## Risks / open items

- Unsigned `rc-helper.exe` + `CreateRemoteThread` into a system process may
  trip AV/Defender heuristics on real machines (same as reference). Expect to
  add an exclusion or sign the binary later.
- The ntdll prologue pattern table is validated against Win10/Win11 x64
  syscall stubs (`4C 8B D1 B8 … F6 04 25 … 01`); unknown layouts fail open.
  Real-device bring-up may need the observed bytes added to the table.
- Report-layer capture is only validated on RC003; RC001 uses a different
  report format (deferred, same as reference).
