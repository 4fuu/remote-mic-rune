# Building

The workspace builds and tests on any host: the pure crates (`rc-protocol`,
`rc-bridge`, `rc-mapping`, and the pure layers of `rc-agent`) have no
Windows dependencies. Only the Windows-only glue (injection, the IOCTL
hook, pipes, `SendInput`, scheduled tasks) needs a Windows toolchain or a
mingw cross-compiler.

## Host tests

```bash
cargo test
```

Runs every host-runnable test across all crates: protocol parsing, the
agent's decision layer (gates, lease, synthesis frames), the bridge state
machine, the gesture engine, mapping persistence, and the app's headless UI
state logic. This is the fast, complete regression net; everything here
also runs on Linux/macOS.

## Rendering tests (GPU required)

The app additionally has headless render tests that rasterize the real
window through Rune's offscreen GPU renderer. They need a GPU (a software
lavapipe device is enough) and are skipped unless enabled:

```bash
RC_APP_UI_TESTS=1 cargo test -p rc-app
```

Snapshot captures write PNGs when a directory is provided:

```bash
RC_APP_UI_TESTS=1 RC_APP_SNAP_DIR=/tmp/rc-app-snaps cargo test -p rc-app section_snapshots
```

The snapshot test renders all five sections at a 780-pt window plus the
Buttons canvas at 1180 pt (the wide regression case) at 2× scale, so layout
changes are directly reviewable as images.

## Windows type check

Fast verification that everything Windows-only still compiles, without
needing a full link:

```bash
cargo check --target x86_64-pc-windows-msvc
```

This needs the MSVC target installed (`rustup target add
x86_64-pc-windows-msvc`); it checks all crates including the Windows glue.
Checking the host target alone (`cargo check`) is not enough for the
Windows-only modules — they are `cfg`-gated and invisible to a host build.

On Linux hosts, `cargo check --target x86_64-pc-windows-msvc` cannot fully
complete: the `frida-gum-sys` build script needs libclang (bindgen) plus an
MSVC C toolchain (`lib.exe`) for its glue code. To type-check the hook code
on Linux instead, temporarily flip the hook gates in
`crates/rc-agent/src/hook.rs` and `crates/rc-agent/Cargo.toml` from
`target_env = "msvc"` to a GNU-target gate (revert afterwards), then:

```bash
sudo apt-get install -y libclang-dev
LIBCLANG_PATH=/usr/lib/llvm-14/lib \
BINDGEN_EXTRA_CLANG_ARGS='--target=x86_64-w64-windows-gnu -isystem /usr/lib/llvm-14/lib/clang/14.0.6/include' \
cargo check --target x86_64-pc-windows-gnu
```

This compiles the real `frida-gum` bindings and the devkit's C glue with
mingw gcc; `cargo check` never links, so the MSVC-only devkit library does
not matter. The authoritative MSVC build check is the native Windows build
below.

## Cross-build (mingw, Linux → Windows)

```bash
cargo build --release --target x86_64-pc-windows-gnu -j 1
```

Notes:

- Requires the mingw toolchain (`x86_64-w64-mingw32-gcc` and friends) and
  the target's std (`rustup target add x86_64-pc-windows-gnu`).
- `-j 1` (single build job) avoids OOM on small machines — the final
  `rc-app` link with LTO enabled is memory-hungry; on larger machines the
  flag can be dropped.
- **Enhanced capture requires the MSVC build.** The GNU cross-build
  compiles a fail-open stub in place of the Frida Gum hook layer (the
  Frida devkit ships as an MSVC static library and cannot be linked by the
  GNU toolchain). The stub installs no hook and logs
  `hook:unsupported_toolchain` at agent boot; the produced
  `rc-helper.exe`, `rc-app.exe`, and `rc_agent.dll` run with the remote
  fully native — the UI, helper, and app work, capture stays off.

## Native Windows build

Prerequisites beyond the Rust toolchain:

- **Visual Studio Build Tools** with the C++ workload (`cl.exe`, `lib.exe`) —
  required by the `frida-gum-sys` build script.
- **LLVM / libclang** for `bindgen`. Install LLVM from
  <https://releases.llvm.org/>; the installer normally adds
  `C:\Program Files\LLVM\bin` to `PATH`. If `cargo build` reports
  `Unable to find libclang`, set `LIBCLANG_PATH` to that `bin` directory.
- Network access on the first build: `frida-gum-sys` downloads the pinned
  Frida Gum devkit (17.9.5, x86_64) from GitHub and caches it under the
  target directory.

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

Produces `rc-helper.exe`, `rc-app.exe`, and `rc_agent.dll` under
`target/x86_64-pc-windows-msvc/release/` (or `target/release/`). This is
the production toolchain and the only one whose `rc_agent.dll` contains
the Frida Gum hook (enhanced capture). Expect a longer first build and a
larger `rc_agent.dll` than the GNU stub, because Gum is linked in.

## Deployment layout

Three artifacts run on the Windows machine:

| Artifact | Role |
| -------- | ---- |
| `rc-app.exe` | The UI app; user-session process. |
| `rc-helper.exe` | The elevated helper, launched by the scheduled task. |
| `rc_agent.dll` | The injected agent. |

**`rc_agent.dll` must sit next to `rc-helper.exe`** — the helper resolves
the DLL relative to its own executable by default (`--agent-dll` overrides).
Without it, injection fails with the "agent DLL missing" exit code before
anything touches a process.
