# Documentation

- [Architecture](architecture.md) — system overview: from BLE HID reports to
  injected input, process/component boundaries, the two named pipes, and the
  workspace layout.
- [Mechanisms](mechanisms.md) — the design catalogue: report-layer capture,
  lease fail-open, per-generation key ownership, absolute-state edges,
  safety gates, voice-key synthesis, the mapping engine, the one-UAC
  scheduled task, device-node recovery, and token auth.
- [UI](ui.md) — the app window: SwiftUI-style inset-grouped form and the
  spatial mapping canvas.
- [Building](building.md) — host tests, rendering tests, Windows checks,
  mingw cross-build, native build, deployment layout.
- [Real-device testing](real-device-testing.md) — staged bring-up on real
  hardware: prerequisites, per-stage verification, troubleshooting, and the
  fail-open self-check list.
