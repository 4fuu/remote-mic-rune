# Real-Device Testing

This project is developed in an environment without Windows, and the final
binaries are produced by mingw cross-compilation (see
[building.md](building.md)). The mechanism chain can only be validated on
real hardware: Windows 10/11 x64 with a paired Xiaomi RC003 remote. This
guide brings the chain up in stages, verifying each one before trusting the
next, and finishes with the fail-open self-check.

## 0. Prerequisites

- Windows 10/11 x64, administrator account.
- Xiaomi RC003 (2 Pro) remote paired and connected (model R861,
  VID `0x2717` / PID `0x32B8`).
- Recommended: add an antivirus exclusion for the build output directory.
  The unsigned helper injects into a system process, so Defender
  heuristics may intercept it; whitelist the directory before the first
  run.

## 1. Build

On Windows (recommended, MSVC):

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

Or use the mingw cross-build artifacts (functionally equivalent):

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Artifacts: `rc-helper.exe`, `rc_agent.dll`, `rc-app.exe`.
**`rc_agent.dll` must sit in the same directory as `rc-helper.exe`.**

Deployment location matters on real hardware: the helper injects the DLL
into a session-0 `WUDFHost.exe` whose restricted service token cannot read
per-user directories. Building and running straight from a user Temp
directory makes the remote `LoadLibraryW` fail silently (returns NULL).
Use a machine-readable location such as `C:\ProgramData\RemoteMicRune\`
(pass `--agent-dll`, or place the whole artifact set there).

## 2. Staged verification without the UI

Run this line first on a new machine. Open an elevated PowerShell in the
artifact directory:

```powershell
# 1) Scan-only: should print the Exclusive verdict and the WUDFHost pid,
#    inject nothing, write no process memory.
.\rc-helper.exe --dry-run

# 2) Observe mode: injects but only reports edges, never clears.
#    Press remote buttons and watch the edge lines in the log.
.\rc-helper.exe --observe --duration 60 --log con

# 3) Canary verification (the key one): clear one key Windows can already
#    see (Home, 0x4A). Expected: Home is dead while the helper runs;
#    Ctrl+C the helper and Home recovers natively within 2 s.
#    This is direct proof of both interception and lease fail-open.
.\rc-helper.exe --canary-usage 0x4A --duration 60 --log con
```

How to read the results:

- `--dry-run` reports `Shared` or `Ambiguous` → this WUDFHost also carries
  other BLE devices, and injection is refused by default (safety gate).
  Only if you confirm the target host is the right one, continue with
  `--force --target-pid <pid>` — a shared host carries the user's other BLE
  devices too.
- `edge usages=[0x28]`-style lines in observe mode (pressing OK) → the
  report-layer read path works.
- Canary key dead while running and recovered after exit → the clear
  write-back and the lease mechanism are both correct.
- A `hook:failed unsupported_toolchain` line → the agent was built with a
  toolchain the Frida Gum devkit does not support (expected fail-open
  behavior: no hook, native behavior). Rebuild with the MSVC toolchain.
- Any other `hook:failed <reason>` line → the hook installation was
  refused (missing ntdll export, Gum attach error). Capture stays off and
  the remote stays native; record the reason.

## 3. Full chain with the UI

```powershell
.\rc-app.exe
```

1. In the app: **Helper → Install helper task (UAC once)** — one UAC
   prompt; then **Start helper**.
2. Flip **Capture → Enhanced capture** — the status badge should turn
   Connected and the generation counter grow.
3. Map the Windows-invisible keys — the hardest evidence there is. Bind
   VolumeUp's single press to the Volume Up media key, then press the
   remote's volume button: the system volume must change. (These three keys
   have no native path at all; only the report layer sees them.)
4. Turn capture off or quit the app — the helper fails open within 2 s and
   every other key returns to native behavior.

## 4. Troubleshooting

| Symptom | Check |
| ------- | ----- |
| Helper cannot connect to the app | Does `%LOCALAPPDATA%\RemoteMicRune\bridge.ini` carry the pid/token of this run? |
| Agent never comes online | Agent log: `%TEMP%\rcr-agent-<WUDFHost pid>.log`, falling back to `%ProgramData%\RemoteMicRune\` when the host token cannot write TEMP; helper logs under `%ProgramData%\RemoteMicRune\` |
| Key stuck pressed | Not expected by design (absolute-state edges + lease fail-open). If it ever reproduces, record the helper log's `lease_expired` counter |
| Volume keys do nothing | Confirm the remote is an RC003; the RC001 report format differs and is not supported by this version |
| Defender blocks or deletes the helper | Re-add the exclusion for the artifact directory; re-build if the binary was quarantined |

## 5. Fail-open self-check

Mechanism-level acceptance list:

- [ ] `taskkill /f` the helper → within ≤2 s every cleared key recovers
      native behavior (lease fail-open).
- [ ] Quitting the app makes the helper exit itself (`--follow-app` under
      the scheduled task); the agent receives Disarm/Bye.
- [ ] Toggling Bluetooth off/on: the helper walks the recovery window →
      `pnputil` remove/scan → re-injection (60 s cooldown between
      attempts).
- [ ] A shared host is refused; `--dry-run` writes no process memory.
- [ ] `--observe` mode clears nothing (edges only).
- [ ] The clear cap exists (`MAX_CLEARS` in `rc_protocol::consts`; a code
      constant, not expected to trigger in the field).

## 6. Win11 26100 blocker (2026-11-09 field test) and the Frida rewrite

The original hand-written inline hook on the shared
`ntdll!NtDeviceIoControlFile` syscall stub was **not viable**: with the hook
installed, any non-target ioctl that completes through a kernel user
callback (observed with console-driver ioctls, `0x00500016`; the agent's own
pipe polling is on the same path) faults forever in
`KiUserCallbackDispatcher` - processes without user32 (WUDFHost included)
have a NULL `KernelCallbackTable`. Three trampoline designs (direct Rust
detour, Detours-style asm relay with a pristine pass-through, and a
self-contained `syscall; ret` trampoline) all reproduce the identical
crash, while the un-hooked build passes the whole observe chain.

**Resolved in code:** the hand-written trampoline (prologue pattern table,
trampoline allocation, thread suspension) has been removed and replaced with
Frida Gum's `Interceptor` (`frida-gum` crate 0.17.2, MSVC builds only).
This is the same hook shape proven in the same WUDFHost by the reference
ecosystem's JS agent (`rc003_agent.js`, `installHook()`): Gum relocates the
prologue with full instruction awareness instead of fixed 16-byte
assumptions, and invokes the original function through its own trampoline.

**Not yet validated on a device.** The next field test must re-run the full
observe chain first (section 2), then the full chain with the UI (section
3) — the hook layer is new code even though the decision path is unchanged.
Everything else verified in the 2026-11-09 field test still applies: BLE
device discovery, SeDebugPrivilege, DLL deployment location, module
verification, and the pipe writer fix documented in this guide.
