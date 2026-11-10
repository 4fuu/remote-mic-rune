# Release process

Remote Mic Rune uses a calendar version in the form `YYYY.MDD.REVISION`,
using the date in `Asia/Hong_Kong`. The month is not padded, the day is two
digits, and the first release of a day uses revision zero. For example,
`2026.1110.0` is the first release on 2026-11-10.

The version lives in `[workspace.package]` in `Cargo.toml`; every crate
inherits it, so the app's titlebar and every binary report the same version.
`Cargo.lock` is tracked so release builds are reproducible and can use
`--locked`.

## Prepare a release

On `main`, with a clean worktree:

```bash
python3 scripts/set-version.py 2026.1110.0
cargo test --locked
```

Add the user-facing notes at `docs/releases/v<version>.md` (for example
`docs/releases/v2026.1110.0.md`). The release workflow uses that file as the
GitHub release body; without it, GitHub's generated notes are used.

Commit the version, lockfile, and notes together:

```bash
git add Cargo.toml Cargo.lock docs/releases/v2026.1110.0.md
git commit -m "Release 2026.1110.0"
git push origin main
```

## Run the workflow

Open **Actions → Release → Run workflow**, choose `main`, and run it. From a
terminal the same trigger is:

```bash
gh workflow run release.yml --ref main
```

The workflow is manual only; pushing a tag does not start it.

It performs these jobs in order:

1. **validate** — refuses a version that is not `YYYY.MDD.REVISION`, whose
   date prefix is not today in `Asia/Hong_Kong`, or whose tag `v<version>`
   already points at a different commit. A rerun is allowed when the tag
   points at the current `main` commit.
2. **verify** — installs the Linux UI test dependencies and runs
   `cargo test --locked` on Ubuntu.
3. **build** — builds on `windows-2025` with the MSVC toolchain, locating
   LLVM/libclang for `bindgen` (installing LLVM if the runner lacks it),
   downloading the pinned Frida Gum devkit on first use, and producing two
   packages from the same staged files:
   - `remote-mic-rune-<version>-x86_64-pc-windows-msvc.zip` — portable copy
     with `rc-app.exe`, `rc-helper.exe`, `rc_agent.dll`, `README.md`,
     `LICENSE`, and `THIRD-PARTY-NOTICES.md`;
   - `remote-mic-rune-<version>-x86_64-pc-windows-msvc-setup.exe` — the
     Inno Setup installer built from `installer/remote-mic-rune.iss`.
4. **release** — aborts if `main` moved during the run, writes
   `SHA256SUMS`, and creates the GitHub release `v<version>` with the zip,
   installer, and checksums. If the release already exists, it uploads the
   assets again with `--clobber`, which makes a failed release resumable.

The workflow does not publish crates or commit package-manager metadata; the
zip and installer are the supported distribution units.

## Installer

The installer is per-machine: it installs under
`{autopf}\Remote Mic Rune` (Program Files) so the session-0 `WUDFHost` token
can read `rc_agent.dll`, creates Start Menu and optional desktop shortcuts,
and registers an uninstaller. It stops a running helper task before replacing
files and removes the task on uninstall. The helper task itself is still
registered from the app's **Helper** tab (one UAC), because that registration
must point at the installed `rc-helper.exe`.

The installer is built with Inno Setup 6. A local build needs `ISCC.exe`; the
release workflow uses the runner's Inno Setup or installs it with Chocolatey.
`installer/remote-mic-rune.iss` is the source of truth and reads the staged
files from `staging/` (gitignored) and the version from `/DVersion=`.
Both the installer and the binaries are unsigned; SmartScreen may warn.

## Build prerequisites (same as a local release build)

- Rust stable with the `x86_64-pc-windows-msvc` target.
- Visual Studio Build Tools with the C++ workload (`cl.exe`, `lib.exe`,
  `rc.exe` for the embedded icon).
- LLVM / libclang for `bindgen`; set `LIBCLANG_PATH` if it is not on `PATH`.
- Network access on the first build: `frida-gum-sys` downloads the pinned
  Frida Gum devkit. Enhanced capture exists only in the MSVC build; the GNU
  cross-build deliberately produces a fail-open stub (see
  [building.md](building.md)).

## After the release

- Confirm the zip contains all three executables and that `rc_agent.dll` sits
  next to `rc-helper.exe`.
- Install from the zip, run `rc-app.exe`, install/start the helper task, and
  verify enhanced capture on an RC003 before announcing the release.
- If a release must be withdrawn, mark the GitHub release as a draft or delete
  it; do not reuse a version for different contents.
