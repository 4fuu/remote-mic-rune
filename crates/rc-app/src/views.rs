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
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, Kit, SegmentedControl, Select, SelectAppearance, Switch, TextInput, root};

use rc_protocol::{ALL_BUTTONS, RemoteButton};
use rc_protocol::buttons::ALWAYS_CAPTURED_BUTTONS;

use crate::bridge_api::{BridgePhase, BridgeStatus};
use crate::i18n::{tr, tr_fmt, Lang, Language};
use crate::mappings::{ButtonAction, MediaKey, MouseButton};
use crate::task::TaskStatus;
use crate::ui::{picker_options, AppState, HelperCommand, Section};

// -- SwiftUI-flavoured type scale -------------------------------------------

/// The titlebar band's height, in points, the app draws itself: `main.rs`
/// declares the same through `App::titlebar`, so Rune takes the native
/// Windows title bar away and keeps its caption buttons over this band's
/// trailing edge (Linux keeps the native frame; the band reads as a toolbar).
pub(crate) const TITLEBAR_HEIGHT: f32 = 46.0;

/// The app title inside the custom titlebar band (15 pt semibold): window
/// chrome, a step below the pages' own 16 pt titles.
const TITLEBAR_TITLE: TextStyle = TextStyle::new(15.0, 650.0);
/// Card headlines / row labels (13 pt semibold).
const HEADLINE: TextStyle = TextStyle::new(13.0, 600.0);
/// The launch-error card's headline (20 pt bold).
const ERROR_TITLE: TextStyle = TextStyle::new(20.0, 700.0);
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

/// The whole window: a self-drawn titlebar band (Rune replaces the native
/// Windows title bar through `App::titlebar` in `main.rs`), then the page.
pub fn view(s: &AppState, cx: &mut Cx<AppState>) -> El {
    let dark = s.settings.window.dark.unwrap_or(cx.system_dark());
    let k = swiftui_kit(dark);
    // The Windows caption buttons' glyphs follow the app's theme rather than
    // the system's, so Auto/Light/Dark keeps them readable over the band
    // (inert on Linux and macOS, which draw no caption buttons).
    cx.caption_color(k.c.text_base);
    // The band stays clear of the window's own controls: macOS traffic lights
    // at the leading edge, the Windows caption buttons at the trailing edge
    // (zero here — Linux keeps the native frame, headless runs report that).
    let insets = cx.chrome_insets();
    let mut layer = Vec::new();

    let titlebar = row()
        .key("titlebar")
        .role("toolbar")
        .w_full()
        .h(TITLEBAR_HEIGHT)
        .center_y()
        .gap(space::S2)
        .pl(insets.leading + 16.0)
        .pr(insets.trailing + 12.0)
        .window_drag()
        .child(
            text("Remote Mic Rune")
                .key("title")
                .font(TITLEBAR_TITLE)
                .color(k.c.text_base),
        )
        .child(
            text(format!("v{}", env!("CARGO_PKG_VERSION")))
                .font(FOOTNOTE)
                .color(k.c.text_faint),
        )
        .child(el().grow(1.0))
        .child(badge_for_phase(&s.bridge, &k, s.lang));

    let mut window = col().key("window").w_full().h_full().fill(k.c.page);
    window = window.child(titlebar);

    if let Some(warning) = &s.warning {
        window = window.child(callout(warning, &k, k.c.warning.bg, k.c.warning.fg));
    }

    window = window.child(
        row()
            .key("tabs-row")
            .w_full()
            .justify_center()
            .px(space::S6)
            .pb(space::S3)
            .child(
                SegmentedControl::new(&Section::labels(s.lang), Some(s.section.index()))
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
        layer.push(error_overlay(error, &k, s.lang));
    }
    root(window, layer)
}

// -- shared pieces -----------------------------------------------------------

fn badge_for_phase(status: &BridgeStatus, k: &Kit, lang: Lang) -> El {
    let (label, variant) = match status.phase {
        BridgePhase::Stopped => (tr(lang, "Bridge stopped"), BadgeVariant::Neutral),
        BridgePhase::Listening => (tr(lang, "Listening for helper"), BadgeVariant::Neutral),
        BridgePhase::Connected => (tr(lang, "Connected"), BadgeVariant::Accent),
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

fn error_overlay(message: &str, k: &Kit, lang: Lang) -> El {
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
                    text(tr(lang, "Cannot start"))
                        .key("error-title")
                        .font(ERROR_TITLE)
                        .color(k.c.text_base),
                )
                .child(text(message.to_string()).wrap().font(HEADLINE).color(k.c.text_muted))
                .child(
                    Button::new(tr(lang, "Exit"))
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
            text(crate::status::phase_label(s.lang, s.bridge.phase))
                .font(type_::BODY)
                .color(k.c.text_muted),
        )
}

fn status_section(s: &AppState, k: &Kit) -> El {
    let link = group(
        "status-link",
        k,
        vec![
            form_row("kv-phase", k, s.tr("Bridge phase"), phase_value(s, k)),
            kv_row(
                "pid",
                k,
                s.tr("Helper pid"),
                s.bridge
                    .helper_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| s.tr("none").to_string()),
            ),
            kv_row(
                "ownership",
                k,
                s.tr("Ownership"),
                crate::status::ownership_line(s.lang, &s.bridge),
            ),
            kv_row(
                "acked",
                k,
                s.tr("Agent acked"),
                crate::status::acked_buttons_line(s.lang, &s.bridge.acked_usages),
            ),
        ],
    );
    let capture = group(
        "status-capture",
        k,
        vec![
            kv_row(
                "capture",
                k,
                s.tr("Capture set"),
                crate::status::acked_buttons_line(s.lang, &s.bridge.captured_usages),
            ),
            kv_row(
                "configured",
                k,
                s.tr("Buttons configured"),
                tr_fmt!(
                    s.lang,
                    "{} of {}",
                    s.settings.mappings.configured_buttons(),
                    ALL_BUTTONS.len()
                ),
            ),
            col()
                .key("row-kv-counters")
                .w_full()
                .gap(space::S1)
                .px(16.0)
                .py(12.0)
                .child(
                    text(s.tr("Counters").to_string())
                        .font(HEADLINE)
                        .color(k.c.text_base),
                )
                .child(
                    text(crate::status::counters_line(s.lang, &s.bridge.counters))
                        .key("kv-counters-value")
                        .wrap()
                        .font(type_::BODY)
                        .color(k.c.text_muted),
                ),
            kv_row(
                "recovering",
                k,
                s.tr("Recovery"),
                if s.bridge.recovering {
                    s.tr("Device-node recovery in progress")
                } else {
                    s.tr("Idle")
                }
                .to_string(),
            ),
        ],
    );
    let mut children = vec![
        group_header(s.tr("Link"), k),
        link,
        group_header(s.tr("Capture"), k),
        capture,
    ];
    // Before the bridge is connected the numbers above barely move; fill the
    // rest of the screen with the three setup steps instead of empty space.
    if s.bridge.phase != BridgePhase::Connected {
        children.push(group_header(s.tr("Getting started"), k));
        children.push(getting_started_card(k, s.lang));
    }
    children.push(group_footer(
        s.tr("Counters update every 500 ms from the bridge snapshot."),
        k,
    ));
    form("status", s.tr("Status"), "", k, children)
}

/// The idle Status screen's next-steps card: one numbered row per setup
/// step. Footnote-sized and tint-quiet, so it reads as guidance, not content.
fn getting_started_card(k: &Kit, lang: Lang) -> El {
    let steps = [
        tr(lang, "Install the helper, then start it — Helper tab."),
        tr(lang, "Map what each button press does — Buttons tab."),
        tr(lang, "Turn on enhanced capture — Capture tab."),
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

/// The voice-key synthesis picker's options, in stable order (the choice's
/// index picks the target usage, whatever the wording).
fn synth_options(lang: Lang) -> [&'static str; 3] {
    [
        tr(lang, "Off"),
        tr(lang, "Left Alt (0x00E2)"),
        tr(lang, "Right Alt (0x00E6)"),
    ]
}

fn capture_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mapped = s.settings.mappings.mapped_mask();
    let synth = s.settings.valid_voice_synth();
    let synth_label = match synth {
        Some(0x00E2) => tr(s.lang, "Left Alt (0x00E2)"),
        Some(0x00E6) => tr(s.lang, "Right Alt (0x00E6)"),
        _ => tr(s.lang, "Off"),
    };
    let options = synth_options(s.lang);

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
            .child(
                text(s.tr("Enhanced capture").to_string())
                    .grow(1.0)
                    .font(HEADLINE)
                    .color(k.c.text_base),
            )
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
            kv_row(
                "mask",
                k,
                s.tr("Mapped buttons"),
                format!("{}/13", mapped.count_ones()),
            ),
            kv_row(
                "set",
                k,
                s.tr("Requested set"),
                crate::status::acked_buttons_line(s.lang, &s.bridge.captured_usages),
            ),
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
                    .child(
                        text(s.tr("Voice key synthesis").to_string())
                            .font(HEADLINE)
                            .color(k.c.text_base),
                    )
                    .child(
                        text(s.tr("Report-layer replacement of usage 0x003E"))
                            .font(FOOTNOTE)
                            .color(k.c.text_faint),
                    ),
            )
            .child(
                Select::new("synth-picker", &options)
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
                        let to = match synth_options(s.lang).iter().position(|o| *o == choice) {
                            Some(1) => Some(0x00E2),
                            Some(2) => Some(0x00E6),
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
        s.tr("Capture"),
        s.tr(
            "Enhanced capture owns the remote's keys at the HID report layer. Turn it off and the \
         remote behaves like a plain keyboard.",
        ),
        k,
        vec![
            master,
            group_footer(
                &tr_fmt!(
                    s.lang,
                    "While enabled, {} are always captured, even with no mappings configured.",
                    always_names.join(", ")
                ),
                k,
            ),
            group_header(s.tr("Coverage"), k),
            coverage,
            group_header(s.tr("Voice key synthesis"), k),
            synth_card,
            group_footer(
                s.tr(
                    "Synthesizes the voice key (0x003E) into an Alt usage at the report layer; \
                 targets are hardware-validated.",
                ),
                k,
            ),
        ]
        .into_iter()
        .chain(voice_section(s, k, layer))
        .collect(),
    )
}

/// The voice bridge section on the Capture page: master switch, output
/// endpoint + gain pickers, test tone, and the live session status.
fn voice_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    use crate::bridge_api::VoicePhase;

    let master = group(
        "voice-master",
        k,
        vec![row()
            .key("voice-switch-row")
            .w_full()
            .center_y()
            .gap(space::S3)
            .px(16.0)
            .py(space::S3)
            .child(
                col()
                    .grow(1.0)
                    .gap(space::S1)
                    .child(
                        text(s.tr("Voice bridge").to_string())
                            .font(HEADLINE)
                            .color(k.c.text_base),
                    )
                    .child(
                        text(s.tr("Streams the remote's microphone into Windows").to_string())
                            .font(FOOTNOTE)
                            .color(k.c.text_faint),
                    ),
            )
            .child(
                Switch::new(s.settings.voice.enabled)
                    .on_toggle(|s: &mut AppState| {
                        s.set_voice_enabled(!s.settings.voice.enabled);
                    })
                    .build(k)
                    .key("voice-switch"),
            )],
    );

    // The device picker re-enumerates endpoints every time it opens, so a
    // device plugged in after launch still shows up.
    let none_label = s.tr("Not selected").to_string();
    let mut device_options = vec![none_label.clone()];
    device_options.extend(s.voice.devices.iter().cloned());
    let current_device = s
        .voice
        .selected
        .clone()
        .unwrap_or_else(|| none_label.clone());
    let device_row = row()
        .key("voice-device-row")
        .w_full()
        .center_y()
        .gap(space::S3)
        .px(16.0)
        .py(space::S3)
        .child(
            text(s.tr("Output device").to_string())
                .grow(1.0)
                .font(HEADLINE)
                .color(k.c.text_base),
        )
        .child(
            Select::new("voice-device-picker", &device_options)
                .width(260.0)
                .current(Some(current_device.as_str()))
                .open(s.open_picker.as_deref() == Some("voice-device-picker"))
                .on_toggle(|s: &mut AppState| {
                    if let Some(voice) = &s.services.voice {
                        voice.refresh_devices();
                    }
                    s.open_picker = match &s.open_picker {
                        Some(key) if key == "voice-device-picker" => None,
                        _ => Some("voice-device-picker".to_string()),
                    }
                })
                .on_close(|s: &mut AppState| s.open_picker = None)
                .on_choose(|s: &mut AppState, choice: &str, _: &mut Tx| {
                    s.open_picker = None;
                    let name = if choice == s.tr("Not selected") {
                        None
                    } else {
                        Some(choice.to_string())
                    };
                    s.set_voice_output(name);
                })
                .build(k, layer),
        );

    const GAIN_OPTIONS: [f32; 5] = [0.0, 6.0, 10.0, 14.0, 20.0];
    let gain_labels: Vec<String> = GAIN_OPTIONS.iter().map(|g| format!("{g} dB")).collect();
    let current_gain = GAIN_OPTIONS
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - s.settings.voice.gain_db)
                .abs()
                .partial_cmp(&(b - s.settings.voice.gain_db).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|g| format!("{g} dB"))
        .unwrap_or_else(|| "10 dB".to_string());
    let gain_refs: Vec<&str> = gain_labels.iter().map(String::as_str).collect();
    let gain_row = row()
        .key("voice-gain-row")
        .w_full()
        .center_y()
        .gap(space::S3)
        .px(16.0)
        .py(space::S3)
        .child(
            text(s.tr("Microphone gain").to_string())
                .grow(1.0)
                .font(HEADLINE)
                .color(k.c.text_base),
        )
        .child(
            Select::new("voice-gain-picker", &gain_refs)
                .width(120.0)
                .current(Some(current_gain.as_str()))
                .open(s.open_picker.as_deref() == Some("voice-gain-picker"))
                .on_toggle(|s: &mut AppState| {
                    s.open_picker = match &s.open_picker {
                        Some(key) if key == "voice-gain-picker" => None,
                        _ => Some("voice-gain-picker".to_string()),
                    }
                })
                .on_close(|s: &mut AppState| s.open_picker = None)
                .on_choose(|s: &mut AppState, choice: &str, _: &mut Tx| {
                    s.open_picker = None;
                    if let Ok(value) = choice.trim_end_matches(" dB").parse::<f32>() {
                        s.set_voice_gain(value);
                    }
                })
                .build(k, layer),
        );

    let test_row = row()
        .key("voice-test-row")
        .w_full()
        .center_y()
        .gap(space::S3)
        .px(16.0)
        .py(space::S3)
        .child(
            text(s.tr("Route check").to_string())
                .grow(1.0)
                .font(HEADLINE)
                .color(k.c.text_base),
        )
        .child(
            Button::new(s.tr("Play test tone"))
                .on_press(|s: &mut AppState| {
                    if let Some(voice) = &s.services.voice {
                        voice.test_tone();
                    }
                })
                .build(k)
                .key("voice-test-tone"),
        )
        .child(
            Button::new(s.tr("Reconnect"))
                .on_press(|s: &mut AppState| {
                    if let Some(voice) = &s.services.voice {
                        voice.reconnect();
                    }
                })
                .build(k)
                .key("voice-reconnect"),
        );

    let mut status_value = match s.voice.phase {
        VoicePhase::Stopped => s.tr("Stopped").to_string(),
        VoicePhase::Scanning => s.tr("Scanning for the remote").to_string(),
        VoicePhase::Connecting => tr_fmt!(s.lang, "Connecting ({})", s.voice.detail),
        VoicePhase::Initializing => tr_fmt!(s.lang, "Initializing ({})", s.voice.detail),
        VoicePhase::Ready => tr_fmt!(s.lang, "Ready ({})", s.voice.detail),
        VoicePhase::WaitingReconnect => {
            tr_fmt!(s.lang, "Reconnecting in {} s", s.voice.detail)
        }
        VoicePhase::Failed => tr_fmt!(s.lang, "Failed: {}", s.voice.detail),
    };
    if s.voice.streaming {
        status_value = format!("{} · {}", status_value, s.tr("streaming"));
    }
    let battery = s
        .voice
        .battery
        .map(|b| format!("{b} %"))
        .unwrap_or_else(|| s.tr("Unknown").to_string());

    let mut status_rows = vec![
        kv_row("voice-status", k, s.tr("Status"), status_value),
        kv_row("voice-battery", k, s.tr("Battery"), battery),
        kv_row(
            "voice-samples",
            k,
            s.tr("Decoded samples"),
            s.voice.samples.to_string(),
        ),
    ];
    if let Some(error) = s.voice.last_error.clone() {
        status_rows.push(kv_row("voice-error", k, s.tr("Last error"), error));
    }
    let status = group("voice-status", k, status_rows);

    vec![
        group_header(s.tr("Voice bridge"), k),
        master,
        group("voice-settings", k, vec![device_row, gain_row, test_row]),
        status,
        group_footer(
            s.tr(
                "Hold the remote's voice key to talk: audio flows through the virtual device's \
             play endpoint; the voice application reads the matching recording endpoint.",
            ),
            k,
        ),
    ]
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
const CARD_W: f32 = 330.0;
const LINE_W: f32 = 1.5;
/// Air the canvas height keeps between stacked cards and at its edges.
const CANVAS_GAP: f32 = 10.0;
const CANVAS_EDGE: f32 = 8.0;
/// The canvas' design height: the 508-pt photo fills all but a `PHOTO_BAND_Y`
/// margin on each side, and the six compact rows per column span nearly the
/// photo's whole band (see `PLACEMENTS`' `target_y` fractions).
const CANVAS_MIN_H: f32 = 570.0;
/// Air the canvas keeps above and below the photo. The photo tracks the
/// canvas height (it keeps its `PHOTO_W`:`PHOTO_H` aspect), so at the design
/// height it is exactly `PHOTO_W` × `PHOTO_H` and every card's target y sits
/// inside the photo's band — leader lines stay attached to the remote at
/// every window width, where a width-scaled photo would shrink away from the
/// cards and leave their lines dangling in space.
const PHOTO_BAND_Y: f32 = 31.0;
/// Growth cap on the height-tracking photo: past ~1.2x the design size it
/// would crowd the card columns, so canvas growth beyond that (a pile of
/// shortcut drafts on edge cards) adds air above and below instead.
const PHOTO_GROW_MAX: f32 = 1.2;
/// The action picker's trigger width at scale 1.
const PICK_W: f32 = 120.0;
/// Card row metrics measured from the rendered kit (and asserted by the
/// geometry test): the compact card's single row — the 28-pt picker plus
/// 14-pt vertical padding — and a shortcut-draft row (hairline divider
/// above it, 28-pt field, 12-pt bottom padding) added while the button's
/// action is a shortcut. Horizontal metrics scale with the laid-out canvas,
/// vertical ones stay fixed.
const CARD_ROW_H: f32 = 56.0;
const CARD_DRAFT_H: f32 = 41.0;
const CARD_PAD_X: f32 = 12.0;
const CARD_ROW_PY: f32 = 14.0;

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

/// Every placed button, top to bottom per column. Both columns share the
/// same six evenly spaced fractions, spanning the photo's band: with the
/// `PHOTO_BAND_Y` air, a card's target y times the canvas height always
/// lands inside the photo's vertical range, so each leader line's vertical
/// segment runs from its anchor to its card entirely across the photo. The
/// 13th button, `volume_mute`, has no physical spot and renders below the
/// canvas.
pub(crate) static PLACEMENTS: [Placement; 12] = [
    Placement { button: RemoteButton::Power, left: true, anchor: (0.386, 0.099), target_y: 0.0667 },
    Placement { button: RemoteButton::Up, left: true, anchor: (0.502, 0.179), target_y: 0.2400 },
    Placement { button: RemoteButton::Left, left: true, anchor: (0.362, 0.246), target_y: 0.4133 },
    Placement { button: RemoteButton::Back, left: true, anchor: (0.406, 0.389), target_y: 0.5867 },
    Placement { button: RemoteButton::Home, left: true, anchor: (0.406, 0.479), target_y: 0.7600 },
    Placement { button: RemoteButton::Menu, left: true, anchor: (0.406, 0.569), target_y: 0.9333 },
    Placement { button: RemoteButton::Right, left: false, anchor: (0.638, 0.246), target_y: 0.0667 },
    Placement { button: RemoteButton::Ok, left: false, anchor: (0.502, 0.246), target_y: 0.2400 },
    Placement { button: RemoteButton::Down, left: false, anchor: (0.502, 0.317), target_y: 0.4133 },
    Placement { button: RemoteButton::VolumeUp, left: false, anchor: (0.604, 0.390), target_y: 0.5867 },
    Placement { button: RemoteButton::VolumeDown, left: false, anchor: (0.604, 0.480), target_y: 0.7600 },
    Placement { button: RemoteButton::Tv, left: false, anchor: (0.604, 0.569), target_y: 0.9333 },
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
        // The photo fills the canvas height down to the band margins (capped,
        // see `PHOTO_GROW_MAX`) instead of scaling with the width: cards keep
        // their full height at every width, so a width-scaled photo would
        // shrink away from their leader lines.
        let photo_h = (h - 2.0 * PHOTO_BAND_Y).min(PHOTO_H * PHOTO_GROW_MAX);
        let photo_w = photo_h * PHOTO_W / PHOTO_H;
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

/// A card's rendered height from the kit's row metrics: the compact single
/// row, plus the shortcut-draft row while the button's action is a shortcut.
fn card_estimate_h(s: &AppState, button: RemoteButton) -> f32 {
    let drafts = if s.shortcut_edits.contains_key(&button) { 1.0 } else { 0.0 };
    CARD_ROW_H + drafts * CARD_DRAFT_H
}

/// The canvas height that keeps the compact cards apart: cards keep their
/// content-driven height while their centers hold the placement fractions, so
/// the height grows when shortcut drafts make the end cards taller — the
/// fractions resolve to exactly the 570-pt design height with plain rows, and
/// re-grow only when a drafted edge card no longer fits its margin.
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

/// Short label for a picker trigger. The compact row shows one action picker,
/// so the chosen value shrinks to its essence; the picker's option list stays
/// the full [`picker_options`] wording.
fn action_short_label(lang: Lang, action: &ButtonAction) -> &'static str {
    match action {
        ButtonAction::Disabled => "—",
        ButtonAction::Shortcut { .. } => tr(lang, "Shortcut"),
        ButtonAction::MediaKey { key } => match key {
            MediaKey::Mute => tr(lang, "Mute"),
            MediaKey::VolumeDown => tr(lang, "Vol-"),
            MediaKey::VolumeUp => tr(lang, "Vol+"),
            MediaKey::PlayPause => tr(lang, "Play"),
            MediaKey::Prev => tr(lang, "Prev"),
            MediaKey::Next => tr(lang, "Next"),
        },
        ButtonAction::MouseClick { button } => match button {
            MouseButton::Left => tr(lang, "L-Click"),
            MouseButton::Right => tr(lang, "R-Click"),
            MouseButton::Middle => tr(lang, "M-Click"),
        },
        ButtonAction::MouseMove { .. } => tr(lang, "Move"),
        ButtonAction::Scroll { .. } => tr(lang, "Scroll"),
    }
}

/// The name a card shows for its button: the compact row has one line for the
/// dot, the name and the action picker, so the snake-case protocol names
/// become their product labels (the volume rocker reads like the remote's own
/// `+` / `-` glyphs). Element keys stay keyed by the protocol name.
fn button_display_name(lang: Lang, button: RemoteButton) -> &'static str {
    match button {
        RemoteButton::Power => tr(lang, "Power"),
        RemoteButton::Up => tr(lang, "Up"),
        RemoteButton::Down => tr(lang, "Down"),
        RemoteButton::Left => tr(lang, "Left"),
        RemoteButton::Right => tr(lang, "Right"),
        RemoteButton::Ok => tr(lang, "OK"),
        RemoteButton::Back => tr(lang, "Back"),
        RemoteButton::Home => tr(lang, "Home"),
        RemoteButton::Menu => tr(lang, "Menu"),
        RemoteButton::VolumeUp => tr(lang, "Vol +"),
        RemoteButton::VolumeDown => tr(lang, "Vol -"),
        RemoteButton::Tv => tr(lang, "TV"),
        RemoteButton::VolumeMute => tr(lang, "Mute"),
    }
}

/// One button's mapping card, `card_w` wide: a single compact row — pressed
/// dot, button name, and the button's one action picker — plus a
/// shortcut-draft row (text field, and the parse error while it is invalid)
/// when the action is a shortcut. On the canvas the caller positions the
/// returned card absolutely.
fn button_card(s: &AppState, button: RemoteButton, k: &Kit, layer: &mut Vec<El>, card_w: f32) -> El {
    let pressed = s.pressed & (1 << button.ordinal()) != 0;
    // Horizontal metrics shrink with the card so a scaled canvas keeps row
    // proportions; vertical metrics stay fixed (they drive the canvas height).
    let sc = (card_w / CARD_W).min(1.0);
    let pick_w = PICK_W * sc;
    let field_w = card_w - 2.0 * CARD_PAD_X * sc;

    let mut main = row()
        .key(format!("row-main-{}", button.name()))
        .w_full()
        .center_y()
        .gap(space::S1)
        .px(CARD_PAD_X * sc)
        .py(CARD_ROW_PY)
        .child(
            dot(if pressed { k.c.bg_accent } else { k.c.text_faint })
                .key(format!("dot-{}", button.name())),
        )
        .child(
            text(button_display_name(s.lang, button))
                .key(format!("name-{}", button.name()))
                .font(HEADLINE)
                .grow(1.0)
                .truncate()
                .color(k.c.text_base),
        );

    let pick_key = format!("pick-{}", button.name());
    let current = s
        .settings
        .mappings
        .action_for(button)
        .map(|action| action_short_label(s.lang, action))
        .unwrap_or("—");
    let options = picker_options(s.lang);
    main = main.child({
        Select::new(pick_key.clone(), &options)
            .appearance(SelectAppearance::Field)
            .label(format!(
                "{} {}",
                button_display_name(s.lang, button),
                s.tr("Action")
            ))
            .width(pick_w)
            .current(Some(current))
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
                s.choose_action(button, choice);
            })
            .build(k, layer)
            // Mini field: tighten the kit trigger's own trailing padding and
            // gap so the short labels fit the `pick_w` box (the 8-pt leading
            // inset lives on the value row inside and stays).
            .gap(2.0)
            .pr(2.0)
    });

    let mut drafts: Vec<El> = Vec::new();
    // A shortcut action gets its draft row under the compact main row.
    if let Some(edit) = s.shortcut_edits.get(&button) {
        let error = s.chord_errors.get(&button).cloned();
        let mut draft = col()
            .key(format!("shortcut-{pick_key}"))
            .w_full()
            .gap(space::S1)
            .px(CARD_PAD_X * sc)
            .pb(space::S3);
        draft = draft.child(
            TextInput::new(edit)
                .placeholder("Ctrl+Alt+F5")
                .width(Some(field_w))
                .invalid(error.is_some())
                .on_edit(move |s: &mut AppState| match s.shortcut_edits.get_mut(&button) {
                    Some(edit) => edit,
                    None => panic!("shortcut draft exists for edited button"),
                })
                .on_change(move |s: &mut AppState, _: &mut Tx| s.shortcut_edited(button))
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
        drafts.push(draft);
    }

    // The card: the compact row, then a hairline divider and a row per draft.
    let mut card = col()
        .key(format!("row-{}", button.name()))
        .w(card_w)
        .radius(radius::LG)
        .fill(k.c.bg_base)
        .child(main);
    // While the button is held, an accent badge floats on the card's top
    // corner — beside the accent dot and its accent leader line — without
    // competing with the row's layout.
    if pressed {
        card = card.child(
            el().absolute(card_w - 66.0, -9.0).child(
                Badge::new(s.tr("pressed"))
                    .variant(BadgeVariant::Accent)
                    .build(k)
                    .key(format!("pressed-{}", button.name().to_ascii_lowercase())),
            ),
        );
    }
    for draft in drafts {
        card = card
            .child(el().h(1.0).w_full().fill(k.c.border_muted))
            .child(draft);
    }
    card
}

fn buttons_section(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let canvas_h = mapping_canvas_h(s);
    // The canvas reports its laid-out width back into state, so the spatial
    // geometry scales with the window: photo and cards hold the design
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
        s.tr("Buttons"),
        s.tr(
            "Pick what each button does. A double press or long press fires the same action. Changes persist immediately \
         and update the capture set.",
        ),
        k,
        vec![
            canvas,
            // The 13th button has no physical spot on the RC003 body.
            group_header(s.tr("Also on this remote"), k),
            // A little wider than the canvas cards: it stands alone, with no
            // photo between it and the window edge.
            row().w_full().center().child(button_card(s, RemoteButton::VolumeMute, k, layer, 360.0)),
        ],
    )
}

fn helper_section(s: &AppState, k: &Kit) -> El {
    let status_line = s
        .task_status
        .as_ref()
        .map(|st: &TaskStatus| st.label(s.lang))
        .unwrap_or_else(|| s.tr("Not checked yet").to_string());

    let mut actions = row().key("helper-buttons").gap(space::S2);
    for (key, label, variant, command) in [
        (
            "install",
            s.tr("Install helper task (UAC once)"),
            ButtonVariant::Submit,
            HelperCommand::Install,
        ),
        (
            "start",
            s.tr("Start helper"),
            ButtonVariant::Neutral,
            HelperCommand::Start,
        ),
        (
            "stop",
            s.tr("Stop helper"),
            ButtonVariant::Neutral,
            HelperCommand::Stop,
        ),
        (
            "remove",
            s.tr("Remove task"),
            ButtonVariant::Danger,
            HelperCommand::Remove,
        ),
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
                    text(s.tr("Task status").to_string())
                        .grow(1.0)
                        .font(HEADLINE)
                        .color(k.c.text_base),
                )
                // Value first, action last: the row reads label → value →
                // button, like a SwiftUI LabeledContent with a trailing action.
                .child(text(status_line).key("helper-status").font(type_::BODY).color(k.c.text_muted))
                .child(
                    Button::new(s.tr("Check status"))
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
        extras.push(
            text(s.tr("Working…"))
                .key("helper-busy")
                .font(FOOTNOTE)
                .color(k.c.text_faint)
                .pl(16.0),
        );
    }
    if let Some(note) = &s.task_note {
        extras.push(text(note.clone()).key("helper-note").font(FOOTNOTE).color(k.c.text_faint).pl(16.0));
    }

    form(
        "helper",
        s.tr("Helper"),
        "",
        k,
        std::iter::once(group_header(s.tr("Scheduled task"), k))
            .chain(std::iter::once(task_card))
            .chain(extras)
            .chain(std::iter::once(group_footer(
                s.tr(
                    "The helper runs elevated via one scheduled task: install it once (UAC), then \
                 start and stop it without prompts. It connects to the bridge pipe and does all \
                 elevated work. It exits on app disconnect by design; the bridge keeps running.",
                ),
                k,
            )))
            .collect(),
    )
}

fn settings_section(s: &AppState, k: &Kit) -> El {
    let dark_options = [s.tr("Auto"), s.tr("Light"), s.tr("Dark")];
    let selected = match s.settings.window.dark {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let language_options = [s.tr("System"), s.tr("English"), s.tr("简体中文")];
    let language_selected = match s.settings.language {
        Language::System => 0,
        Language::En => 1,
        Language::Zh => 2,
    };
    let path = crate::settings::app_dir()
        .map(|dir| crate::settings::settings_path(&dir).display().to_string())
        .unwrap_or_else(|| {
            s.tr("settings.json (LOCALAPPDATA unavailable: not persisted this run)")
                .to_string()
        });

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
            .child(
                text(s.tr("Appearance").to_string())
                    .grow(1.0)
                    .font(HEADLINE)
                    .color(k.c.text_base),
            )
            .child(
                SegmentedControl::new(&dark_options, Some(selected))
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

    // The language picker: index-based like the tabs, so the option labels
    // ("跟随系统"/"English"/"简体中文") can be any wording. `System` follows
    // the OS on the next run; picking English or Chinese redraws at once.
    let language = group(
        "settings-language",
        k,
        vec![row()
            .key("language-row")
            .w_full()
            .center_y()
            .gap(space::S3)
            .px(16.0)
            .py(space::S3)
            .child(
                text(s.tr("Language").to_string())
                    .grow(1.0)
                    .font(HEADLINE)
                    .color(k.c.text_base),
            )
            .child(
                SegmentedControl::new(&language_options, Some(language_selected))
                    .label("Language")
                    .fit()
                    .on_pick(|s: &mut AppState, index, _: &mut Tx| {
                        s.set_language(match index {
                            1 => Language::En,
                            2 => Language::Zh,
                            _ => Language::System,
                        });
                    })
                    .build(k)
                    .key("language-tabs"),
            )],
    );

    let storage = group(
        "settings-storage",
        k,
        vec![
            kv_row("path", k, s.tr("Settings file"), path),
            row()
                .key("settings-actions")
                .w_full()
                .gap(space::S2)
                .px(16.0)
                .py(space::S3)
                .child(
                    Button::new(s.tr("Save now"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut AppState| {
                            s.persist();
                        })
                        .build(k)
                        .key("settings-save"),
                )
                .child(
                    Button::new(s.tr("Reload from disk"))
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut AppState| s.reload_settings())
                        .build(k)
                        .key("settings-reload"),
                ),
        ],
    );

    let mut children = vec![
        group_header(s.tr("Appearance"), k),
        appearance,
        group_header(s.tr("Language"), k),
        language,
        group_header(s.tr("Storage"), k),
        storage,
    ];
    if let Some(error) = &s.save_error {
        children.push(
            text(error.clone())
                .key("settings-save-error")
                .font(FOOTNOTE)
                .color(k.c.danger.fg)
                .pl(16.0),
        );
    }
    form("settings", s.tr("Settings"), "", k, children)
}

/// The bottom toolbar: live pressed state on the left, capture state on the
/// right, over a hairline top border.
fn bottom_bar(s: &AppState, k: &Kit) -> El {
    let pressed_names = crate::engine::pressed_names(s.pressed).join(", ");
    let any_pressed = !pressed_names.is_empty();
    let pressed = if pressed_names.is_empty() {
        s.tr("none").to_string()
    } else {
        pressed_names
    };
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
                .child(dot(if any_pressed {
                    k.c.bg_accent
                } else {
                    k.c.text_faint
                }))
                .child(
                    text(tr_fmt!(s.lang, "Pressed: {}", pressed))
                        .key("footer-pressed")
                        .font(FOOTNOTE)
                        .color(k.c.text_faint),
                )
                .child(el().grow(1.0))
                .child(dot(if s.settings.capture_enabled {
                    k.c.bg_accent
                } else {
                    k.c.text_faint
                }))
                .child(
                    text(if s.settings.capture_enabled {
                        s.tr("Enhanced capture on")
                    } else {
                        s.tr("Enhanced capture off")
                    })
                    .key("footer-capture")
                    .font(FOOTNOTE)
                    .color(k.c.text_faint),
                ),
        )
}
