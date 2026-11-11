# Remote Mic Rune

Windows app that turns the Xiaomi RC003 (2 Pro) Bluetooth remote into a fully
programmable remote for your PC.

The RC003 pairs as a BLE keyboard, but Windows silently drops three of its
keys — Back, Volume+ and Volume− never reach any application — and the
remaining buttons arrive as fixed key presses you cannot rebind. Remote Mic
Rune captures keys at the HID report layer, below every user-mode input
channel, and makes all 13 buttons programmable:

- **13 semantic buttons** — Back, OK, TV, Home, Left/Right/Up/Down, Menu,
  Power, Volume mute/+/−.
- **One action per button** — each button fires its action on every press;
  a double press fires it twice and a long press fires it once.
- **Actions** — keyboard shortcuts (full chords with modifiers), media keys
  (volume, play/pause, previous/next), and mouse clicks.
- **The three Windows-invisible keys made usable** — Back, Volume+ and
  Volume− are dropped by the keyboard driver before any user-mode input
  channel sees them; owning them requires the report layer, which is exactly
  where this project works.

## How it works

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

A scheduled task runs the elevated helper with a single UAC prompt. The
helper locates the `WUDFHost.exe` process that hosts the RC003's BLE HID
device, injects `rc_agent.dll` into it, and renews a 2-second lease. The
agent hooks the host→kernel HID report submission at the
`NtDeviceIoControlFile` boundary and selectively clears chosen usages from
keyboard report 0x01 before the kernel copies it — the only place where
Back/Volume+/Volume− still exist. Kept keys flow over a named-pipe bridge to
the app, where the mapping engine fires the button's configured action and
replays it as real system input with `SendInput`.

**Fail-open by design.** The agent only modifies reports while the lease is
alive. If the helper dies, is force-killed, stops renewing, or the app goes
away, the agent stops touching reports within 2 seconds and the remote
behaves like a plain keyboard again — no stuck keys, nothing left armed.

## Workspace

| Crate | Kind | Role |
| ----- | ---- | ---- |
| `crates/rc-protocol` | lib | Single source of truth: button usage table, wire formats, timing/safety constants. No Windows APIs. |
| `crates/rc-agent` | cdylib | Injected into WUDFHost: IOCTL hook, selective usage clearing, lease, voice-key synthesis. |
| `crates/rc-helper` | bin | Elevated helper: locate the exclusive host, inject, renew the lease, bridge agent↔app, scheduled task, device-node recovery. |
| `crates/rc-bridge` | lib | App-side link endpoint: pipe server, token auth, per-generation key ownership, watchdogs. |
| `crates/rc-mapping` | lib | Per-button action model + SendInput injection. |
| `crates/rc-voice` | lib | ATVV voice bridge: BLE GATT session, IMA/DVI ADPCM decode, PCM into a virtual audio device. Windows backend cfg-gated. |
| `crates/rc-app` | bin | Rune UI shell: status, capture control, mapping canvas, voice bridge, helper task, settings. |

## Quick start

1. **Build** (Linux cross-build or native Windows — see
   [docs/building.md](docs/building.md)):

   ```bash
   cargo test                                   # host-runnable logic (all crates)
   cargo build --release --target x86_64-pc-windows-gnu   # mingw cross-build
   ```

   `rc_agent.dll` must sit next to `rc-helper.exe` at runtime. The mingw
   cross-build produces all artifacts but **without enhanced capture** — the
   Frida Gum hook layer only compiles with the MSVC toolchain (production).
   See [docs/building.md](docs/building.md).

2. **Install the helper task** — in the app: *Helper → Install helper task
   (UAC once)*, or on an elevated prompt: `rc-helper --install-task`. One UAC
   prompt; afterwards the helper starts and stops without prompts.

3. **Enable capture** — start the helper (*Helper → Start helper*), map
   buttons in the *Buttons* tab, then flip *Capture → Enhanced capture*.
   Back/Volume+/Volume− answer immediately; other buttons answer as soon as
   they have bindings.

Bringing the whole chain up on real hardware for the first time? Follow
[docs/real-device-testing.md](docs/real-device-testing.md), which verifies
each stage separately before the full UI chain.

## Docs

- [docs/architecture.md](docs/architecture.md) — system overview: from BLE
  HID reports to injected input, process boundaries, the two named pipes,
  workspace layout.
- [docs/mechanisms.md](docs/mechanisms.md) — the design catalogue:
  report-layer capture, lease fail-open, key ownership, safety gates,
  voice-key synthesis, the mapping engine, scheduled task, device recovery,
  token auth.
- [docs/ui.md](docs/ui.md) — the app window: inset-grouped form and the
  spatial mapping canvas.
- [docs/building.md](docs/building.md) — build, test, cross-compile,
  deployment layout.
- [docs/real-device-testing.md](docs/real-device-testing.md) — staged
  bring-up on real hardware, canary verification, troubleshooting, fail-open
  self-check.

## License

Original codebase, MIT. The UI is built on Rune (MIT).

## Acknowledgments

- [remote-mic-app-windows](https://github.com/GetSayAll/remote-mic-app-windows)
  (无线麦 SayAll for Windows) — the idea of turning the Xiaomi Bluetooth
  remotes into a Windows controller, including device bring-up and per-button
  key mapping, was inspired by this project. Its JS agent's
  `NtDeviceIoControlFile` hook (`rc003_agent.js`, `installHook()`) is the
  proven reference shape our Frida Gum hook follows.
- [Frida](https://frida.re) / [frida-gum](https://github.com/frida/frida-gum)
  — the `Interceptor` engine behind the agent's report-layer hook (crate
  `frida-gum` 0.17.2, wxWindows licence). See
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
