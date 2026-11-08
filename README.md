# remote-mic-rune

Windows app for Xiaomi Bluetooth remotes (RC003): elevated report-layer key
capture rewritten with a Rune UI. Mechanism-equivalent reimplementation of the
enhanced ("全按键支持") track of the reference project
[GetSayAll/remote-mic-app-windows](https://github.com/GetSayAll/remote-mic-app-windows)
(original Rust code; the reference was read for mechanism facts only — see
[docs/ADR-0001](docs/ADR-0001-rewrite-scope.md)).

## How it works

```
RC003 (BLE HID) ──► WUDFHost.exe ──► kbdhid ──► Windows input
                        │  ▲
             (1) inject │  │ (2) hook NtDeviceIoControlFile:
             rc_agent   │  │     zero mapped usages in keyboard
             .dll       │  │     report 0x01 before the kernel copies it
                        ▼  │
                  rc-helper.exe (elevated, scheduled task)
                        │  ▲ lease renew 500 ms / lease 2 s (fail-open)
                        │  │ JSON lines, per-pid named pipe
                        ▼  │
                   rc-app.exe (Rune UI)
                        │
                   rc-bridge (pipe server, token, per-generation
                              key ownership, watchdogs)
                        │
                   rc-mapping (click/double/hold gestures → SendInput)
```

- The remote's Back / Vol+ / Vol− usages are dropped by `kbdhid` before any
  user-mode input channel; owning them requires the report layer.
- **Fail-open everywhere**: if the helper dies, stops renewing the lease, or
  the app disconnects, the agent stops modifying reports within ≤2 s and the
  remote behaves like a plain keyboard again.

## Workspace

| Crate | Kind | Role |
| ----- | ---- | ---- |
| `crates/rc-protocol` | lib | Single source of truth: usage table, wire formats, timing/safety constants. No Windows APIs. |
| `crates/rc-agent` | cdylib | Injected into WUDFHost: IOCTL hook, selective usage clearing, lease, voice-key synthesis. |
| `crates/rc-helper` | bin | Elevated helper: locate exclusive host, inject, renew lease, bridge agent↔app, scheduled task, device-node recovery. |
| `crates/rc-bridge` | lib | App-side link endpoint: pipe server, token auth, generation ownership, watchdogs. |
| `crates/rc-mapping` | lib | Gesture recognizer (click/double/hold/repeat) + action mapping + SendInput. |
| `crates/rc-app` | bin | Rune UI shell. |

Docs: [ADR-0001 scope & mechanism parity](docs/ADR-0001-rewrite-scope.md) ·
[Real-device test guide 真机测试指南](docs/REALDEVICE-TEST.md)

## Build

```bash
cargo test                                   # host-runnable logic (all crates)
cargo check --target x86_64-pc-windows-msvc  # fast Windows type check
cargo build --release --target x86_64-pc-windows-gnu   # cross-build (mingw)
```

`rc_agent.dll` must sit next to `rc-helper.exe` at runtime.

License: MIT.
