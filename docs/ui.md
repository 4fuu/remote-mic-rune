# UI

The app window (`rc-app.exe`, built on the Rune GPU-rendered UI framework)
is a SwiftUI-style **inset-grouped form**: uppercase section headers above
inset cards, footnote captions below them, and one centered segmented
control for navigation. Light and dark appearances follow the system (or the
Settings override), with a system-blue tint approximated in oklch. The UI is
bilingual: English source strings with a complete Simplified Chinese
catalog, following the OS UI language by default (see
[`i18n.rs`](../crates/rc-app/src/i18n.rs)).

## Window anatomy

- **Title bar band** — the app draws its own 46 pt title bar
  (`App::titlebar`): app title, version, and the bridge-phase badge
  (`Connected`, `Listening`, …), plus a callout strip for settings warnings
  when one exists. On Windows Rune removes the native title bar and draws
  the minimize / maximize / close buttons over the band's trailing edge
  (snap layouts included); on Linux the native frame stays and the band
  reads as a toolbar. The band is a `.window_drag()` region: drag to move,
  double-click to maximize/restore, right-click for the window menu.
- **Section tabs** — a centered `SegmentedControl` with the five sections
  (English labels shown here; the UI renders them in the active language):
  **Status · Capture · Buttons · Helper · Settings**.

## Sections and their controls

### Status

Live diagnostics, refreshed from the bridge snapshot every 500 ms.

- **Link** card: bridge phase, helper pid, ownership line (whether the
  current generation is acked inside the 1.5 s window), and the agent-acked
  button set.
- **Capture** card: the active capture set, configured-button count out of
  13, protocol counters (edges, clears, lease expiries, …) and the
  device-node recovery state.
- **Getting started** card: while the link is down, the idle space shows the
  three setup steps (install/start the helper, map buttons, enable
  enhanced capture) as numbered rows.

### Capture

- **Enhanced capture** master switch — arms the whole report-layer chain.
  Off, the remote behaves like a plain keyboard.
- **Coverage** card: mapped buttons (`n/13`) and the requested capture set
  the agent has acked.
- **Voice key synthesis** card: a picker with `Off`,
  `Left Alt (0x00E2)`, `Right Alt (0x00E6)`; footnote explains the
  report-layer replacement of usage `0x003E` and that targets are
  hardware-validated.
- Footnotes state the always-captured keys (Back, Volume+, Volume−) that
  answer even with no mappings configured.

### Buttons — the spatial mapping canvas

The heart of the window. Instead of a table, the 13 buttons are mapped on a
**spatial canvas**:

- The **remote photo** (a photograph of the RC003 hardware) sits at the
  canvas' center, 250 × 508 points at design size.
- Twelve **anchored cards** flank the photo in two columns; each card is one
  button: a live dot that lights up in the accent color while that button is
  physically held, the button name, and three compact pickers —
  **Single / Double / Long** — with actions from the shared picker
  (Disabled, media keys, Shortcut, mouse clicks).
- **Leader lines** connect every card to its physical spot on the photo;
  each line turns accent-colored while its button is held, matching the
  card dot.
- Choosing **Shortcut** opens a draft row on the card: type a chord (e.g.
  `Ctrl+Shift+Left Alt`) into the field, and the parser formats it
  canonically. Changes persist immediately and update the live capture set.
- VolumeMute — the one button with no physical spot on the RC003 body —
  gets its own standalone card under the caption "Also on this remote".
- The canvas is authored at a design width (800 pt) and scales
  proportionally when the window is narrower — photo, cards and lines keep
  their geometry down to ~0.86 scale while card text keeps its size; extra
  height (many shortcut drafts) adds air instead of stretching the photo
  past ~1.2×.

### Helper

One card for the scheduled task that grants elevation:

- Buttons: **Install helper task (UAC once)** (prominent), **Start helper**,
  **Stop helper**, **Remove task** (destructive).
- A **Task status** row in SwiftUI `LabeledContent` style — label, value,
  trailing ghost **Check status** button — parsed from `schtasks /query`.
- Footnotes show busy state and command output; the section footer explains
  the one-UAC model: install once, then start and stop without prompts, and
  the helper exits on app disconnect by design.

### Settings

- **Appearance** segmented control: Auto / Light / Dark.
- **Language** segmented control: System / English / 简体中文. System follows
  the OS UI language (Windows `GetUserDefaultUILanguage`, otherwise
  `LC_ALL`/`LANG`); the choice persists in `settings.json` and applies
  without a restart. A file saved before the field existed loads as System.
- **Storage** card: the resolved `settings.json` path, **Save now** and
  **Reload from disk** buttons. Loading is fault-tolerant: a malformed file
  is backed up to `settings.json.bak` and the app starts fresh; load/save
  warnings are translated at display time.

## Testing the UI

State changes funnel through `AppState` methods, so the form logic is
testable headlessly (no GPU). Rendering tests exist separately and run under
`RC_APP_UI_TESTS=1` with snapshot output to `RC_APP_SNAP_DIR`; set
`RC_APP_SNAP_LANG=zh` or `RC_APP_SNAP_LANG=en` to pin the snapshot language
(the snapshot suite defaults to Chinese). See [building.md](building.md).
