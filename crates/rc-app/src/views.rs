//! The view layer, styled after SwiftUI's "Inset Grouped" Form language:
//!
//! - system-blue tint (light/dark variants) for values, switches and the
//!   segmented control,
//! - white cards with 12 px corners on the page background, hairline
//!   dividers between rows,
//! - leading label + trailing secondary value rows,
//! - small section headers above cards and footnote captions below them,
//! - a thin bottom toolbar with live pressed/capture state.
//!
//! Only layout lives here; every state change still goes through
//! [`crate::ui::AppState`] methods. Element keys are stable (tests locate
//! controls by them) — restyling must not rename them.

use rune::{Cx, El, ScrollSpec, TextStyle, Tx, col, el, oklch, row, text};
use rune::tok::{radius, space, type_};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, Kit, SegmentedControl, Select, Switch, TextInput, root};

use rc_protocol::{ALL_BUTTONS, RemoteButton};
use rc_protocol::buttons::ALWAYS_CAPTURED_BUTTONS;

use crate::bridge_api::{BridgePhase, BridgeStatus};
use crate::mappings::Slot;
use crate::task::TaskStatus;
use crate::ui::{AppState, HelperCommand, PICKER_OPTIONS, Section, slot_key};

// -- SwiftUI-flavoured type scale -------------------------------------------

/// Window title (navigation bar, 20 pt bold).
const LARGE_TITLE: TextStyle = TextStyle::new(20.0, 700.0);
/// Card headlines / row labels (13 pt semibold).
const HEADLINE: TextStyle = TextStyle::new(13.0, 600.0);
/// Secondary captions (11 pt regular) — SwiftUI footnote.
const FOOTNOTE: TextStyle = TextStyle::new(11.0, 400.0);
/// Small section headers above cards (SwiftUI uppercase header).
const GROUP_HEADER: TextStyle = TextStyle::new(11.0, 600.0);

/// The neutral kit with a system-blue tint (light `#007AFF`, dark `#0A84FF`
/// approximated in oklch) and iOS-ish radii.
pub fn swiftui_kit(dark: bool) -> Kit {
    let mut k = Kit::neutral(dark);
    let tint = if dark { oklch(0.68, 0.18, 258.0) } else { oklch(0.63, 0.19, 258.0) };
    k.c.text_accent = tint;
    k.c.icon_accent = tint;
    k.c.bg_accent = tint;
    k.c.border_focus = tint;
    k.c.border_active = tint;
    // Submit's filled skin paints from this token (actions.rs); the neutral
    // theme makes it near-black, and SwiftUI's prominent button is the tint.
    k.c.bg_icon_button_contrast = tint;
    k.radius.xs = 4.0;
    k.radius.sm = 6.0;
    k.radius.md = 10.0;
    k.radius.lg = 12.0;
    k.radius.xl = 16.0;
    k
}

// -- Form primitives ---------------------------------------------------------

/// The whole window.
pub fn view(s: &AppState, cx: &mut Cx<AppState>) -> El {
    let dark = s.settings.window.dark.unwrap_or(cx.system_dark());
    let k = swiftui_kit(dark);
    let mut layer = Vec::new();

    let mut window = col().key("window").w_full().h_full().fill(k.c.page);

    let mut head = row()
        .key("nav-bar")
        .w_full()
        .center_y()
        .gap(space::S2)
        .px(space::S6)
        .pt(16.0)
        .pb(space::S2);
    head = head.child(
        text("Remote Mic Rune").key("title").font(LARGE_TITLE).color(k.c.text_base),
    );
    head = head.child(
        text(format!("v{}", env!("CARGO_PKG_VERSION")))
            .font(FOOTNOTE)
            .color(k.c.text_faint),
    );
    head = head.child(el().grow(1.0));
    head = head.child(badge_for_phase(&s.bridge, &k));
    window = window.child(head);

    if let Some(warning) = &s.warning {
        window = window.child(callout(warning, &k, k.c.warning.bg, k.c.warning.fg));
    }

    window = window.child(
        row()
            .key("tabs-row")
            .w_full()
            .px(space::S6)
            .pb(space::S3)
            .child(
                SegmentedControl::new(&Section::LABELS, Some(s.section.index()))
                    .label("Section")
                    .fit()
                    .on_pick(|s: &mut AppState, index, _: &mut Tx| s.section = Section::from_index(index))
                    .build(&k)
                    .key("section-tabs"),
            ),
    );

    let content = match s.section {
        Section::Status => status_section(s, &k),
        Section::Capture => capture_section(s, &k, &mut layer),
        Section::Buttons => buttons_section(s, &k, &mut layer),
        Section::Helper => helper_section(s, &k),
        Section::Settings => settings_section(s, &k),
    };
    window = window.child(content.grow(1.0).min_h(0.0));
    window = window.child(bottom_bar(s, &k));

    if let Some(error) = &s.launch_error {
        layer.push(error_overlay(error, &k));
    }
    root(window, layer)
}

// -- shared pieces -----------------------------------------------------------

fn badge_for_phase(status: &BridgeStatus, k: &Kit) -> El {
    let (label, variant) = match status.phase {
        BridgePhase::Stopped => ("Bridge stopped", BadgeVariant::Neutral),
        BridgePhase::Listening => ("Listening for helper", BadgeVariant::Neutral),
        BridgePhase::Connected => ("Connected", BadgeVariant::Accent),
    };
    Badge::new(label).variant(variant).build(k).key("phase-badge")
}

/// A SwiftUI-style callout: tinted background, leading accent bar, footnote
/// text. Also used for the corrupt-settings warning.
fn callout(message: &str, _k: &Kit, bg: rune::Color, fg: rune::Color) -> El {
    row()
        .key("warning-banner")
        .w_full()
        .gap(space::S2)
        .px(space::S3)
        .py(space::S2)
        .mx(space::S6)
        .radius(radius::MD)
        .fill(bg)
        .child(el().w(3.0).h(16.0).radius(1.5).fill(fg))
        .child(text(message.to_string()).wrap().grow(1.0).font(FOOTNOTE).color(fg))
}

fn error_overlay(message: &str, k: &Kit) -> El {
    col()
        .key("error-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(k.alpha(k.c.black, 0.3))
        .child(
            col()
                .key("error-card")
                .w(420.0)
                .padding(space::S6)
                .gap(space::S3)
                .radius(k.radius.xl)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_muted)
                .child(
                    text("Cannot start")
                        .key("error-title")
                        .font(LARGE_TITLE)
                        .color(k.c.text_base),
                )
                .child(text(message.to_string()).wrap().font(HEADLINE).color(k.c.text_muted))
                .child(
                    Button::new("Exit")
                        .variant(ButtonVariant::Submit)
                        .on_press_tx(|_: &mut AppState, tx: &mut Tx| tx.quit())
                        .build(k)
                        .key("error-exit"),
                ),
        )
}

/// A status dot (7 px circle).
fn dot(color: rune::Color) -> El {
    el().size(7.0, 7.0).radius(3.5).fill(color)
}

/// One form row inside a card: leading label, trailing value/control.
fn form_row(key: &str, k: &Kit, label: &str, trailing: El) -> El {
    row()
        .key(format!("row-{key}"))
        .w_full()
        .center_y()
        .gap(space::S3)
        .px(16.0)
        .py(space::S3)
        .child(text(label.to_string()).grow(1.0).font(HEADLINE).color(k.c.text_base))
        .child(trailing)
}

/// A key/value row (SwiftUI `LabeledContent`): label primary, value
/// secondary, right-aligned. Keys stay `kv-<key>`/`kv-<key>-value`.
fn kv_row(key: &str, k: &Kit, label: &str, value: String) -> El {
    form_row(
        &format!("kv-{key}"),
        k,
        label,
        text(value)
            .key(format!("kv-{key}-value"))
            .wrap()
            .font(type_::BODY)
            .color(k.c.text_muted),
    )
}

/// A card of form rows with hairline dividers between them.
fn group(key: &str, k: &Kit, rows: Vec<El>) -> El {
    let mut card = col()
        .key(format!("group-{key}"))
        .w_full()
        .radius(radius::LG)
        .fill(k.c.bg_base);
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.child(
                el()
                    .key(format!("group-{key}-divider-{i}"))
                    .h(1.0)
                    .w_full()
                    .fill(k.c.border_muted),
            );
        }
        card = card.child(row);
    }
    card
}

/// Small uppercase header above a card (SwiftUI section header).
fn group_header(title: &str, k: &Kit) -> El {
    text(title.to_uppercase())
        .font(GROUP_HEADER)
        .color(k.c.text_faint)
        .pl(16.0)
}

/// Footnote caption below a card (SwiftUI section footer). The bottom
/// padding keeps air between a footer and the next section header.
fn group_footer(footer: &str, k: &Kit) -> El {
    text(footer.to_string())
        .wrap()
        .font(FOOTNOTE)
        .color(k.c.text_faint)
        .pl(16.0)
        .pr(16.0)
        .pb(space::S2)
}

/// A scrollable form body: title, caption, then cards.
fn form(key: &str, title: &str, caption: &str, k: &Kit, children: Vec<El>) -> El {
    let mut list = col()
        .key(format!("form-{key}"))
        .w_full()
        .h_full()
        .px(space::S6)
        .pb(16.0)
        .gap(space::S2)
        .scroll(ScrollSpec { stick_bottom: false })
        .shrink();
    list = list.child(text(title.to_string()).font(TextStyle::new(16.0, 650.0)).color(k.c.text_base));
    if !caption.is_empty() {
        list = list.child(
            text(caption.to_string())
                .wrap()
                .pb(space::S2)
                .font(type_::BODY)
                .color(k.c.text_muted),
        );
    }
    for child in children {
        list = list.child(child);
    }
    list
}

// -- sections ----------------------------------------------------------------

fn phase_value(s: &AppState, k: &Kit) -> El {
    let color = match s.bridge.phase {
        BridgePhase::Connected | BridgePhase::Listening => k.c.success.fg,
        BridgePhase::Stopped => k.c.text_faint,
    };
    row()
        .key("kv-phase-value")
        .center_y()
        .gap(space::S2)
        .child(dot(color))
        .child(
            text(crate::status::phase_label(s.bridge.phase).to_string())
                .font(type_::BODY)
                .color(k.c.text_muted),
        )
}

fn status_section(s: &AppState, k: &Kit) -> El {
    let link = group(
        "status-link",
        k,
        vec![
            form_row("kv-phase", k, "Bridge phase", phase_value(s, k)),
            kv_row("pid", k, "Helper pid", s
                .bridge
                .helper_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string())),
            kv_row("ownership", k, "Ownership", crate::status::ownership_line(&s.bridge)),
            kv_row("acked", k, "Agent acked", crate::status::acked_buttons_line(&s.bridge.acked_usages)),
        ],
    );
    let capture = group(
        "status-capture",
        k,
        vec![
            kv_row("capture", k, "Capture set", crate::status::acked_buttons_line(&s.bridge.captured_usages)),
            kv_row("configured", k, "Buttons configured", format!(
                "{} of {}",
                s.settings.mappings.configured_buttons(),
                ALL_BUTTONS.len()
            )),
            col()
                .key("row-kv-counters")
                .w_full()
                .gap(space::S1)
                .px(16.0)
                .py(12.0)
                .child(text("Counters".to_string()).font(HEADLINE).color(k.c.text_base))
                .child(
                    text(crate::status::counters_line(&s.bridge.counters))
                        .key("kv-counters-value")
                        .wrap()
                        .font(type_::BODY)
                        .color(k.c.text_muted),
                ),
            kv_row(
                "recovering",
                k,
                "Recovery",
                if s.bridge.recovering { "Device-node recovery in progress" } else { "Idle" }.to_string(),
            ),
        ],
    );
    let mut children = vec![group_header("Link", k), link, group_header("Capture", k), capture];
    // Before the bridge is connected the numbers above barely move; fill the
    // rest of the screen with the three setup steps instead of empty space.
    if s.bridge.phase != BridgePhase::Connected {
        children.push(group_header("Getting started", k));
        children.push(getting_started_card(k));
    }
    children.push(group_footer("Counters update every 500 ms from the bridge snapshot.", k));
    form("status", "Status", "", k, children)
}

/// The idle Status screen's next-steps card: one numbered row per setup
/// step. Footnote-sized and tint-quiet, so it reads as guidance, not content.
fn getting_started_card(k: &Kit) -> El {
    let steps = [
        "Install the helper, then start it — Helper tab.",
        "Map what each button press does — Buttons tab.",
        "Turn on enhanced capture — Capture tab.",
    ];
    let rows = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            row()
                .key(format!("getting-started-step-{}", i + 1))
                .w_full()
                .center_y()
                .gap(space::S2)
                .px(16.0)
                .py(space::S2)
                .child(
                    el()
                        .size(18.0, 18.0)
                        .center()
                        .radius(9.0)
                        .fill(k.alpha(k.c.bg_accent, 0.12))
                        .child(
                            text((i + 1).to_string())
                                .font(TextStyle::new(11.0, 600.0))
                                .color(k.c.text_accent),
                        ),
                )
                .child(
                    text(step.to_string())
                        .wrap()
                        .grow(1.0)
                        .font(FOOTNOTE)
                        .color(k.c.text_muted),
                )
        })
        .collect();
    group("getting-started", k, rows)
}

fn capture_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mapped = s.settings.mappings.mapped_mask();
    let synth = s.settings.valid_voice_synth();
    let synth_label = match synth {
        Some(0x00E2) => "Left Alt (0x00E2)",
        Some(0x00E6) => "Right Alt (0x00E6)",
        _ => "Off",
    };
    const SYNTH_OPTIONS: [&str; 3] = ["Off", "Left Alt (0x00E2)", "Right Alt (0x00E6)"];

    let master = group(
        "capture-master",
        k,
        vec![row()
            .key("capture-switch-row")
            .w_full()
            .center_y()
            .gap(space::S3)
            .px(16.0)
            .py(space::S3)
            .child(text("Enhanced capture".to_string()).grow(1.0).font(HEADLINE).color(k.c.text_base))
            .child(
                Switch::new(s.settings.capture_enabled)
                    .on_toggle(|s: &mut AppState| {
                        s.set_capture_enabled(!s.settings.capture_enabled);
                    })
                    .build(k)
                    .key("capture-switch"),
            )],
    );

    let coverage = group(
        "capture-coverage",
        k,
        vec![
            kv_row("mask", k, "Mapped buttons", format!("{}/13", mapped.count_ones())),
            kv_row("set", k, "Requested set", crate::status::acked_buttons_line(&s.bridge.captured_usages)),
        ],
    );

    let synth_card = group(
        "capture-synth",
        k,
        vec![row()
            .key("synth-row")
            .w_full()
            .center_y()
            .gap(space::S3)
            .px(16.0)
            .py(space::S3)
            .child(
                col()
                    .grow(1.0)
                    .gap(space::S1)
                    .child(text("Voice key synthesis".to_string()).font(HEADLINE).color(k.c.text_base))
                    .child(
                        text("Report-layer replacement of usage 0x003E")
                            .font(FOOTNOTE)
                            .color(k.c.text_faint),
                    ),
            )
            .child(
                Select::new("synth-picker", &SYNTH_OPTIONS)
                    .width(180.0)
                    .current(Some(synth_label))
                    .open(s.open_picker.as_deref() == Some("synth-picker"))
                    .on_toggle(|s: &mut AppState| {
                        s.open_picker = match &s.open_picker {
                            Some(key) if key == "synth-picker" => None,
                            _ => Some("synth-picker".to_string()),
                        }
                    })
                    .on_close(|s: &mut AppState| s.open_picker = None)
                    .on_choose(|s: &mut AppState, choice: &str, _: &mut Tx| {
                        s.open_picker = None;
                        let to = match choice {
                            "Left Alt (0x00E2)" => Some(0x00E2),
                            "Right Alt (0x00E6)" => Some(0x00E6),
                            _ => None,
                        };
                        s.set_voice_synth(to);
                    })
                    .build(k, layer),
            )],
    );

    let always_names: Vec<&str> = ALWAYS_CAPTURED_BUTTONS.iter().map(|b| b.name()).collect();
    form(
        "capture",
        "Capture",
        "Enhanced capture owns the remote's keys at the HID report layer. Turn it off and the \
         remote behaves like a plain keyboard.",
        k,
        vec![
            master,
            group_footer(
                &format!(
                    "While enabled, {} are always captured, even with no mappings configured.",
                    always_names.join(", ")
                ),
                k,
            ),
            group_header("Coverage", k),
            coverage,
            group_header("Voice key synthesis", k),
            synth_card,
            group_footer(
                "Synthesizes the voice key (0x003E) into an Alt usage at the report layer; \
                 targets are hardware-validated.",
                k,
            ),
        ],
    )
}

// -- Buttons: the spatial mapping canvas -------------------------------------

/// The mapping canvas' design width: geometry is authored at this width and
/// shrinks proportionally when the laid-out canvas is narrower (photo, cards,
/// lines and offsets all scale; card text keeps its size). The 960-pt window
/// lays the canvas out at 912 pt, so it runs at scale 1 there.
pub(crate) const CANVAS_BASE_W: f32 = 800.0;
/// The remote photo's rect in the canvas at scale 1 (see `remote_photo.rs`).
const PHOTO_W: f32 = 250.0;
const PHOTO_H: f32 = 508.0;
/// A mapping card's width at scale 1, and the leader-line thickness.
const CARD_W: f32 = 280.0;
const LINE_W: f32 = 1.5;
/// Air the canvas height keeps between stacked cards and at its edges.
const CANVAS_GAP: f32 = 12.0;
const CANVAS_EDGE: f32 = 8.0;
/// Height floor (the reference canvas' collapsed-card height); the computed
/// height always exceeds it with expanded cards.
const CANVAS_MIN_H: f32 = 1010.0;
/// Card row metrics measured from the rendered kit (and asserted by the
/// geometry test): header row, one gesture-slot row, one shortcut-draft row
/// (divider included), and the card's hairline dividers between its rows.
const CARD_HEAD_H: f32 = 40.25;
const CARD_SLOT_H: f32 = 48.0;
const CARD_DRAFT_H: f32 = 41.0;
const CARD_DIVIDERS: f32 = 3.0;

/// Where a button sits on the physical RC003 remote and where its card goes:
/// `anchor` is the fractional (x, y) of the button inside the photo rect,
/// `target_y` the fraction of the canvas height the card's center holds, and
/// `left` picks the card column mirroring the button's side of the remote.
pub(crate) struct Placement {
    pub(crate) button: RemoteButton,
    pub(crate) left: bool,
    anchor: (f32, f32),
    target_y: f32,
}

/// Every placed button, top to bottom per column. The 13th button,
/// `volume_mute`, has no physical spot and renders below the canvas.
pub(crate) static PLACEMENTS: [Placement; 12] = [
    Placement { button: RemoteButton::Power, left: true, anchor: (0.386, 0.099), target_y: 0.08 },
    Placement { button: RemoteButton::Up, left: true, anchor: (0.502, 0.179), target_y: 0.23 },
    Placement { button: RemoteButton::Left, left: true, anchor: (0.362, 0.246), target_y: 0.38 },
    Placement { button: RemoteButton::Back, left: true, anchor: (0.406, 0.389), target_y: 0.53 },
    Placement { button: RemoteButton::Home, left: true, anchor: (0.406, 0.479), target_y: 0.68 },
    Placement { button: RemoteButton::Menu, left: true, anchor: (0.406, 0.569), target_y: 0.83 },
    Placement { button: RemoteButton::Right, left: false, anchor: (0.638, 0.246), target_y: 0.215 },
    Placement { button: RemoteButton::Ok, left: false, anchor: (0.502, 0.246), target_y: 0.36 },
    Placement { button: RemoteButton::Down, left: false, anchor: (0.502, 0.317), target_y: 0.505 },
    Placement { button: RemoteButton::VolumeUp, left: false, anchor: (0.604, 0.390), target_y: 0.65 },
    Placement { button: RemoteButton::VolumeDown, left: false, anchor: (0.604, 0.480), target_y: 0.795 },
    Placement { button: RemoteButton::Tv, left: false, anchor: (0.604, 0.569), target_y: 0.94 },
];

/// The canvas' laid-out geometry at the current window width.
struct CanvasGeom {
    w: f32,
    h: f32,
    photo_left: f32,
    photo_top: f32,
    photo_w: f32,
    photo_h: f32,
    card_w: f32,
}

impl CanvasGeom {
    fn for_width(w: f32, h: f32) -> CanvasGeom {
        let sc = (w / CANVAS_BASE_W).min(1.0);
        let photo_w = PHOTO_W * sc;
        let photo_h = PHOTO_H * sc;
        CanvasGeom {
            w,
            h,
            photo_w,
            photo_h,
            photo_left: (w - photo_w) / 2.0,
            photo_top: (h - photo_h) / 2.0,
            card_w: CARD_W * sc,
        }
    }

    fn anchor_x(&self, p: &Placement) -> f32 {
        self.photo_left + self.photo_w * p.anchor.0
    }

    fn anchor_y(&self, p: &Placement) -> f32 {
        self.photo_top + self.photo_h * p.anchor.1
    }

    fn card_x(&self, p: &Placement) -> f32 {
        if p.left { 0.0 } else { self.w - self.card_w }
    }

    fn card_y(&self, p: &Placement, card_h: f32) -> f32 {
        p.target_y * self.h - card_h / 2.0
    }
}

/// A card's rendered height from the kit's row metrics: header, three
/// gesture rows, one shortcut-draft row per slot holding a shortcut, and the
/// hairline dividers between them.
fn card_estimate_h(s: &AppState, button: RemoteButton) -> f32 {
    let drafts = [Slot::Single, Slot::Double, Slot::Long]
        .iter()
        .filter(|slot| s.shortcut_edits.contains_key(&slot_key(button, **slot)))
        .count() as f32;
    CARD_HEAD_H + 3.0 * CARD_SLOT_H + CARD_DIVIDERS + drafts * CARD_DRAFT_H
}

/// The canvas height that keeps the expanded cards apart: cards keep their
/// content-driven height while their centers hold the placement fractions, so
/// the canvas grows until every column fits — neighbors `CANVAS_GAP` apart
/// and the end cards `CANVAS_EDGE` inside the canvas. With the kit's current
/// metrics this resolves to 1693.75 pt (the tv card's 0.94 target_y against
/// its ~187 pt height dominates); it re-grows as shortcut drafts add rows.
pub(crate) fn mapping_canvas_h(s: &AppState) -> f32 {
    let card_h = |b: RemoteButton| card_estimate_h(s, b);
    let mut h = CANVAS_MIN_H;
    for left in [true, false] {
        let mut column: Vec<&Placement> = PLACEMENTS.iter().filter(|p| p.left == left).collect();
        column.sort_by(|a, b| a.target_y.partial_cmp(&b.target_y).expect("target_y is a number"));
        for pair in column.windows(2) {
            let spacing = pair[1].target_y - pair[0].target_y;
            let need = ((card_h(pair[0].button) + card_h(pair[1].button)) / 2.0 + CANVAS_GAP) / spacing;
            h = h.max(need);
        }
        let first = column[0];
        let last = column[column.len() - 1];
        h = h.max((card_h(first.button) / 2.0 + CANVAS_EDGE) / first.target_y);
        h = h.max((card_h(last.button) / 2.0 + CANVAS_EDGE) / (1.0 - last.target_y));
    }
    h
}

/// The elbow from a button's anchor on the photo to its card: one vertical
/// [`LINE_W`] segment at the anchor's x from the anchor to the card's center
/// line, then one horizontal segment into the card's near edge.
fn leader_lines(p: &Placement, g: &CanvasGeom, color: rune::Color) -> Vec<El> {
    let ax = g.anchor_x(p);
    let ay = g.anchor_y(p);
    let cy = p.target_y * g.h;
    let (y, lh) = if cy > ay { (ay, cy - ay) } else { (cy, ay - cy) };
    let vertical = el()
        .key(format!("line-v-{}", p.button.name()))
        .absolute(ax, y)
        .w(LINE_W)
        .h(lh.max(LINE_W))
        .fill(color);
    let (x, lw) = if p.left {
        (g.card_w, ax - g.card_w)
    } else {
        let edge = g.w - g.card_w;
        (ax, edge - ax)
    };
    let horizontal = el()
        .key(format!("line-h-{}", p.button.name()))
        .absolute(x, cy - LINE_W / 2.0)
        .w(lw.max(LINE_W))
        .h(LINE_W)
        .fill(color);
    vec![vertical, horizontal]
}

/// One button's mapping card, `card_w` wide: headline with pressed indicator,
/// then one row per gesture slot with the action picker trailing. A shortcut
/// draft gets its own full-width row with the chord text field. On the canvas
/// the caller positions the returned card absolutely.
fn button_card(s: &AppState, button: RemoteButton, k: &Kit, layer: &mut Vec<El>, card_w: f32) -> El {
    let pressed = s.pressed & (1 << button.ordinal()) != 0;
    // Controls shrink with the card, so a scaled canvas keeps row proportions.
    let sc = (card_w / CARD_W).min(1.0);
    let pick_w = 170.0 * sc;
    let field_w = 200.0 * sc;
    let mut rows: Vec<El> = Vec::new();

    rows.push(
        row()
            .key(format!("row-head-{}", button.name()))
            .w_full()
            .center_y()
            .gap(space::S2)
            .px(16.0)
            .py(space::S3)
            .child(
                dot(if pressed { k.c.bg_accent } else { k.c.text_faint })
                    .key(format!("dot-{}", button.name())),
            )
            .child(
                text(button.name().to_string())
                    .key(format!("name-{}", button.name()))
                    .font(HEADLINE)
                    .grow(1.0)
                    .color(k.c.text_base),
            )
            .child(
                text(if pressed { "pressed" } else { "" })
                    .key(format!("pressed-{}", button.name().to_ascii_lowercase()))
                    .font(FOOTNOTE)
                    .color(k.c.text_accent),
            ),
    );

    for slot in [Slot::Single, Slot::Double, Slot::Long] {
        let pick_key = format!("pick-{}-{}", button.name(), slot.caption().to_ascii_lowercase());
        let current = s
            .settings
            .mappings
            .slot_action(button, slot)
            .map(|a| a.picker_label())
            .unwrap_or_else(|| "Disabled".to_string());
        let set = current != "Disabled";

        rows.push(
            row()
                .key(format!("row-slot-{pick_key}"))
                .w_full()
                .center_y()
                .gap(space::S3)
                .px(16.0)
                .py(space::S3)
                .child(
                    text(slot.caption().to_string())
                        .grow(1.0)
                        .font(type_::BODY)
                        .color(if set { k.c.text_base } else { k.c.text_faint }),
                )
                .child(
                    Select::new(pick_key.clone(), &PICKER_OPTIONS)
                        .width(pick_w)
                        .current(Some(current.as_str()))
                        .open(s.open_picker.as_deref() == Some(pick_key.as_str()))
                        .on_toggle({
                            let pick_key = pick_key.clone();
                            move |s: &mut AppState| {
                                s.open_picker = match &s.open_picker {
                                    Some(open) if *open == pick_key => None,
                                    _ => Some(pick_key.clone()),
                                }
                            }
                        })
                        .on_close(|s: &mut AppState| s.open_picker = None)
                        .on_choose(move |s: &mut AppState, choice: &str, _: &mut Tx| {
                            s.open_picker = None;
                            s.choose_action(button, slot, choice);
                        })
                        .build(k, layer),
                ),
        );

        // Shortcut drafts: their own row under the slot picker.
        let edit_key = slot_key(button, slot);
        if let Some(edit) = s.shortcut_edits.get(&edit_key) {
            let error = s.chord_errors.get(&edit_key).cloned();
            let mut draft = col()
                .key(format!("shortcut-{pick_key}"))
                .w_full()
                .gap(space::S1)
                .px(16.0)
                .pb(space::S3);
            draft = draft.child(
                TextInput::new(edit)
                    .placeholder("Ctrl+Alt+F5")
                    .width(Some(field_w))
                    .invalid(error.is_some())
                    .on_edit(move |s: &mut AppState| match s.shortcut_edits.get_mut(&edit_key) {
                        Some(edit) => edit,
                        None => panic!("shortcut draft exists for edited slot"),
                    })
                    .on_change(move |s: &mut AppState, _: &mut Tx| s.shortcut_edited(button, slot))
                    .build(k),
            );
            if let Some(why) = error {
                draft = draft.child(
                    text(why)
                        .key(format!("chord-error-{pick_key}"))
                        .font(FOOTNOTE)
                        .color(k.c.danger.fg),
                );
            }
            rows.push(draft);
        }
    }

    let mut card = col()
        .key(format!("row-{}", button.name()))
        .w(card_w)
        .radius(radius::LG)
        .fill(k.c.bg_base);
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.child(el().h(1.0).w_full().fill(k.c.border_muted));
        }
        card = card.child(row);
    }
    card
}

fn buttons_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let canvas_h = mapping_canvas_h(s);
    // The canvas reports its laid-out width back into state, so the spatial
    // geometry scales with the window: photo and cards hold the reference
    // canvas' proportions down to ~0.86 scale (the 780-pt snapshot renders
    // ~0.915), and card text keeps its size throughout.
    let g = CanvasGeom::for_width(s.buttons_canvas_w, canvas_h);
    let mut canvas = col()
        .key("mapping-canvas")
        .w_full()
        .h(canvas_h)
        .on_drawn_layout(|s: &mut AppState, rect| s.buttons_canvas_w = rect[2]);
    canvas = canvas.child({
        let photo = crate::remote_photo::remote_photo();
        el()
            .key("remote-photo")
            .absolute(g.photo_left, g.photo_top)
            .size(g.photo_w, g.photo_h)
            .pixels(&photo)
    });
    // Leader lines under the cards; each turns accent while its button is
    // held, matching the card dot.
    for p in &PLACEMENTS {
        let pressed = s.pressed & (1 << p.button.ordinal()) != 0;
        let color = if pressed { k.c.bg_accent } else { k.c.border_muted };
        for line in leader_lines(p, &g, color) {
            canvas = canvas.child(line);
        }
    }
    for p in &PLACEMENTS {
        let card = button_card(s, p.button, k, layer, g.card_w)
            .absolute(g.card_x(p), g.card_y(p, card_estimate_h(s, p.button)));
        canvas = canvas.child(card);
    }

    form(
        "buttons",
        "Buttons",
        "Pick what each press, double press and long press does. Changes persist immediately \
         and update the capture set.",
        k,
        vec![
            canvas,
            // The 13th button has no physical spot on the RC003 body.
            group_header("Also on this remote", k),
            row().w_full().center().child(button_card(s, RemoteButton::VolumeMute, k, layer, 320.0)),
        ],
    )
}

fn helper_section(s: &AppState, k: &Kit) -> El {
    let status_line = s
        .task_status
        .as_ref()
        .map(|st: &TaskStatus| st.label())
        .unwrap_or_else(|| "Not checked yet".to_string());

    let mut actions = row().key("helper-buttons").gap(space::S2);
    for (key, label, variant, command) in [
        ("install", "Install helper task (UAC once)", ButtonVariant::Submit, HelperCommand::Install),
        ("start", "Start helper", ButtonVariant::Neutral, HelperCommand::Start),
        ("stop", "Stop helper", ButtonVariant::Neutral, HelperCommand::Stop),
        ("remove", "Remove task", ButtonVariant::Danger, HelperCommand::Remove),
    ] {
        actions = actions.child(
            Button::new(label)
                .variant(variant)
                .disabled(s.task_busy)
                .on_press(move |s: &mut AppState| s.run_helper_command(command))
                .build(k)
                .key(format!("helper-{key}")),
        );
    }

    let task_card = group(
        "helper-task",
        k,
        vec![
            row().key("helper-actions-row").w_full().px(16.0).py(space::S3).child(actions),
            row()
                .key("helper-status-row")
                .w_full()
                .center_y()
                .gap(space::S3)
                .px(16.0)
                .py(space::S3)
                .child(
                    text("Task status".to_string())
                        .grow(1.0)
                        .font(HEADLINE)
                        .color(k.c.text_base),
                )
                // Value first, action last: the row reads label → value →
                // button, like a SwiftUI LabeledContent with a trailing action.
                .child(text(status_line).key("helper-status").font(type_::BODY).color(k.c.text_muted))
                .child(
                    Button::new("Check status")
                        .variant(ButtonVariant::Ghost)
                        .disabled(s.task_busy)
                        .on_press(|s: &mut AppState| s.run_helper_command(HelperCommand::RefreshStatus))
                        .build(k)
                        .key("helper-refresh"),
                ),
        ],
    );

    let mut extras: Vec<El> = Vec::new();
    if s.task_busy {
        extras.push(text("Working…").key("helper-busy").font(FOOTNOTE).color(k.c.text_faint).pl(16.0));
    }
    if let Some(note) = &s.task_note {
        extras.push(text(note.clone()).key("helper-note").font(FOOTNOTE).color(k.c.text_faint).pl(16.0));
    }

    form(
        "helper",
        "Helper",
        "",
        k,
        std::iter::once(group_header("Scheduled task", k))
            .chain(std::iter::once(task_card))
            .chain(extras)
            .chain(std::iter::once(group_footer(
                "The helper runs elevated via one scheduled task: install it once (UAC), then \
                 start and stop it without prompts. It connects to the bridge pipe and does all \
                 elevated work. It exits on app disconnect by design; the bridge keeps running.",
                k,
            )))
            .collect(),
    )
}

fn settings_section(s: &AppState, k: &Kit) -> El {
    const DARK_OPTIONS: [&str; 3] = ["Auto", "Light", "Dark"];
    let selected = match s.settings.window.dark {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let path = crate::settings::app_dir()
        .map(|dir| crate::settings::settings_path(&dir).display().to_string())
        .unwrap_or_else(|| "settings.json (LOCALAPPDATA unavailable: not persisted this run)".to_string());

    let appearance = group(
        "settings-appearance",
        k,
        vec![row()
            .key("appearance-row")
            .w_full()
            .center_y()
            .gap(space::S3)
            .px(16.0)
            .py(space::S3)
            .child(text("Appearance".to_string()).grow(1.0).font(HEADLINE).color(k.c.text_base))
            .child(
                SegmentedControl::new(&DARK_OPTIONS, Some(selected))
                    .label("Appearance")
                    .fit()
                    .on_pick(|s: &mut AppState, index, _: &mut Tx| {
                        s.set_dark(match index {
                            1 => Some(false),
                            2 => Some(true),
                            _ => None,
                        });
                    })
                    .build(k)
                    .key("appearance-tabs"),
            )],
    );

    let storage = group(
        "settings-storage",
        k,
        vec![
            kv_row("path", k, "Settings file", path),
            row()
                .key("settings-actions")
                .w_full()
                .gap(space::S2)
                .px(16.0)
                .py(space::S3)
                .child(
                    Button::new("Save now")
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut AppState| {
                            s.persist();
                        })
                        .build(k)
                        .key("settings-save"),
                )
                .child(
                    Button::new("Reload from disk")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut AppState| s.reload_settings())
                        .build(k)
                        .key("settings-reload"),
                ),
        ],
    );

    let mut children = vec![group_header("Appearance", k), appearance, group_header("Storage", k), storage];
    if let Some(error) = &s.save_error {
        children.push(
            text(error.clone())
                .key("settings-save-error")
                .font(FOOTNOTE)
                .color(k.c.danger.fg)
                .pl(16.0),
        );
    }
    form("settings", "Settings", "", k, children)
}

/// The bottom toolbar: live pressed state on the left, capture state on the
/// right, over a hairline top border.
fn bottom_bar(s: &AppState, k: &Kit) -> El {
    let pressed = crate::engine::pressed_names(s.pressed).join(", ");
    let any_pressed = !pressed.is_empty();
    let pressed = if pressed.is_empty() { "none".to_string() } else { pressed };
    col()
        .key("footer-wrap")
        .w_full()
        .child(el().key("footer-top-line").h(1.0).w_full().fill(k.c.border_muted))
        .child(
            row()
                .key("footer")
                .w_full()
                .center_y()
                .gap(space::S2)
                .px(space::S6)
                .py(space::S2)
                .fill(k.c.bg_base)
        .child(dot(if any_pressed { k.c.bg_accent } else { k.c.text_faint }))
        .child(
            text(format!("Pressed: {pressed}"))
                .key("footer-pressed")
                .font(FOOTNOTE)
                .color(k.c.text_faint),
        )
        .child(el().grow(1.0))
        .child(dot(if s.settings.capture_enabled { k.c.bg_accent } else { k.c.text_faint }))
        .child(
            text(if s.settings.capture_enabled {
                "Enhanced capture on"
            } else {
                "Enhanced capture off"
            })
            .key("footer-capture")
            .font(FOOTNOTE)
            .color(k.c.text_faint),
            ),
        )
}
