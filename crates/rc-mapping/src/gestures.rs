//! Gesture recognition: single / double / long press / hold-to-repeat from
//! raw press and release edges, one independent state machine per button.
//!
//! Timings follow `rc_protocol::consts`:
//! double window 300 ms between press onsets, long press at 550 ms held,
//! repeat start delay 350 ms, per-button repeat interval from
//! [`RemoteButton::repeat_interval`].
//!
//! Semantics:
//! - If only `single` is configured (no double, no long, not deferred), the
//!   single action fires IMMEDIATELY on the press edge (zero latency).
//! - If double is configured, a single click waits out the double window
//!   after release and is confirmed when the window expires.
//! - Long fires at the threshold while held; with hold-to-repeat it
//!   re-fires at the interval.
//! - A hold that outlasts the double window counts as a single (it may then
//!   enter single-repeat); a second press held past the window cancels the
//!   pending double and concludes as single.
//!
//! Pure state machine: no locks, no IO, all time injected by the caller.
//! The engine calls [`GestureRecognizer::advance`] on its tick (every 25 ms
//! in the current driver model) and dispatches the fired gestures.

use std::collections::BTreeMap;
use std::time::Duration;

use rc_protocol::consts;
use rc_protocol::RemoteButton;

use crate::actions::{ButtonAction, ButtonMappings, ButtonTrigger};

/// Maximum gap between two press onsets that still counts as a double click.
pub const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(consts::DOUBLE_CLICK_WINDOW_MS);
/// How long a button must be held before the long press fires.
pub const LONG_PRESS_THRESHOLD: Duration = Duration::from_millis(consts::LONG_PRESS_THRESHOLD_MS);
/// Hold time before the repeat stream of a hold-to-repeat binding starts.
pub const REPEAT_START_DELAY: Duration = Duration::from_millis(consts::REPEAT_START_DELAY_MS);

/// Per-button gesture configuration, derived from the button mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GestureConfig {
    /// A single action is configured (otherwise single firing is a no-op).
    pub single_configured: bool,
    /// The double column is configured, so single waits out the double window.
    pub double_enabled: bool,
    /// The long column is configured, so holding 550 ms fires long.
    pub long_enabled: bool,
    /// Hold-to-repeat targets the single action (fires every interval while
    /// held). Only present when the long column is NOT configured.
    pub repeat_single: Option<Duration>,
    /// Hold-to-repeat targets the long action (continues firing after the
    /// threshold).
    pub repeat_long: Option<Duration>,
    /// Desktop-switching single actions (lock workstation style) must wait
    /// for the full physical release so the raw DOWN/UP pair settles first.
    pub defer_single_until_release: bool,
}

impl GestureConfig {
    /// Derive the gesture config for one button. `None` when the mappings
    /// are disabled or the button has no configured action at all.
    pub fn for_button(mappings: &ButtonMappings, button: RemoteButton) -> Option<Self> {
        let actions = mappings.actions(button);
        if !mappings.enabled || !actions.any_configured() {
            return None;
        }
        let single_configured = actions.single.is_configured();
        let double_enabled = actions.double.is_configured();
        let long_enabled = actions.long.is_configured();
        let defer_single_until_release = !double_enabled
            && !long_enabled
            && matches!(&actions.single, ButtonAction::Shortcut { chord }
                if ButtonAction::shortcut_locks_workstation(chord));
        let interval = button.repeat_interval();
        // Defensive degradation for hand-edited configs: hold-repeat on the
        // single slot is ignored while a long action is configured or the
        // single is deferred, mirroring the UI's mutual-exclusion rule.
        let repeat_single = match actions.hold_repeat {
            Some(ButtonTrigger::Single)
                if !long_enabled
                    && !defer_single_until_release
                    && actions.single.allows_repeat() =>
            {
                Some(interval)
            }
            _ => None,
        };
        let repeat_long = match actions.hold_repeat {
            Some(ButtonTrigger::Long) if actions.long.allows_repeat() => Some(interval),
            _ => None,
        };
        Some(Self {
            single_configured,
            double_enabled,
            long_enabled,
            repeat_single,
            repeat_long,
            defer_single_until_release,
        })
    }

    /// Raw single path: fires on the press edge with zero latency.
    fn raw_path(&self) -> bool {
        self.single_configured
            && !self.double_enabled
            && !self.long_enabled
            && !self.defer_single_until_release
    }
}

/// Which gesture fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureKind {
    Single,
    Double,
    Long,
}

/// A gesture the recognizer decided to fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiredGesture {
    pub button: RemoteButton,
    pub kind: GestureKind,
}

/// Derived configurations per button.
pub type GestureConfigs = BTreeMap<RemoteButton, GestureConfig>;

/// Build the per-button config table from the mappings (buttons without a
/// config are simply absent).
pub fn configs_from_mappings(mappings: &ButtonMappings) -> GestureConfigs {
    let mut configs = GestureConfigs::new();
    for button in rc_protocol::ALL_BUTTONS {
        if let Some(config) = GestureConfig::for_button(mappings, button) {
            configs.insert(button, config);
        }
    }
    configs
}

#[derive(Debug, Clone, Copy, Default)]
struct ButtonGestureState {
    pressed: bool,
    is_second_press: bool,
    waiting_for_second: bool,
    long_fired: bool,
    /// This hold already concluded as a single (past the double window): no
    /// extra single on release and no double window entry.
    hold_concluded: bool,
    long_deadline: Option<std::time::Instant>,
    double_deadline: Option<std::time::Instant>,
    /// Hold-confirmation timer for the double + repeat-single combination.
    single_conclude_deadline: Option<std::time::Instant>,
    /// Repeat timer of the single path.
    repeat_deadline: Option<std::time::Instant>,
    /// Repeat timer of the long path.
    long_repeat_deadline: Option<std::time::Instant>,
    /// Context click took over this press: ignore everything until release.
    swallowing: bool,
}

/// The recognizer. Unconfigured buttons are ignored; no method panics.
#[derive(Debug, Default)]
pub struct GestureRecognizer {
    buttons: GestureConfigsStates,
}

type GestureConfigsStates = BTreeMap<RemoteButton, (GestureConfig, ButtonGestureState)>;

impl GestureRecognizer {
    pub fn new(configs: &GestureConfigs) -> Self {
        Self {
            buttons: fresh_states(configs),
        }
    }

    /// Replace the configuration, resetting all in-flight gesture state
    /// (pending double windows, long timers and repeat streams are dropped
    /// without firing anything).
    pub fn reconfigure(&mut self, configs: &GestureConfigs) {
        self.buttons = fresh_states(configs);
    }

    /// Press edge: returns gestures that fire immediately (raw single path).
    pub fn press(&mut self, button: RemoteButton, now: std::time::Instant) -> Vec<FiredGesture> {
        let Some((config, state)) = self.buttons.get_mut(&button) else {
            return Vec::new();
        };
        if state.swallowing {
            // A context click owns this press: ignore until release.
            return Vec::new();
        }
        if state.waiting_for_second {
            // Second press: close the double window, flag the second press
            // and restart the long timer.
            state.waiting_for_second = false;
            state.is_second_press = true;
            state.double_deadline = None;
        }
        state.pressed = true;
        state.hold_concluded = false;
        if config.long_enabled {
            state.long_deadline = Some(now + LONG_PRESS_THRESHOLD);
        }
        if config.raw_path() {
            // Immediate single on press; hold-to-repeat starts after the
            // start delay.
            if config.repeat_single.is_some() {
                state.repeat_deadline = Some(now + REPEAT_START_DELAY);
            }
            return vec![FiredGesture {
                button,
                kind: GestureKind::Single,
            }];
        }
        if config.double_enabled && config.repeat_single.is_some() {
            // Hold past the double window concludes as single (no extra
            // single for the first click) and then enters repeat.
            state.single_conclude_deadline = Some(now + DOUBLE_CLICK_WINDOW);
            state.repeat_deadline = Some(now + REPEAT_START_DELAY);
        }
        Vec::new()
    }

    /// Release edge: returns gestures that fire immediately (double, or
    /// deferred/window-path singles).
    pub fn release(&mut self, button: RemoteButton, now: std::time::Instant) -> Vec<FiredGesture> {
        let Some((config, state)) = self.buttons.get_mut(&button) else {
            return Vec::new();
        };
        if state.swallowing {
            // The swallowed press's release: finish silently.
            *state = ButtonGestureState::default();
            return Vec::new();
        }
        state.pressed = false;
        state.long_deadline = None;
        state.repeat_deadline = None;
        state.single_conclude_deadline = None;
        state.long_repeat_deadline = None;
        if state.hold_concluded {
            // Hold already concluded as single: finish silently.
            state.hold_concluded = false;
            state.is_second_press = false;
            return Vec::new();
        }
        if state.long_fired {
            // Long already fired: finish silently.
            state.long_fired = false;
            state.is_second_press = false;
            return Vec::new();
        }
        if state.is_second_press {
            state.is_second_press = false;
            return vec![FiredGesture {
                button,
                kind: GestureKind::Double,
            }];
        }
        if config.double_enabled {
            // First click of a possible double: open the window, fire
            // nothing yet.
            state.waiting_for_second = true;
            state.double_deadline = Some(now + DOUBLE_CLICK_WINDOW);
            return Vec::new();
        }
        if config.raw_path() {
            // Raw path already fired on press; release only cancels repeat.
            return Vec::new();
        }
        // Gesture path without double: single fires on release.
        vec![FiredGesture {
            button,
            kind: GestureKind::Single,
        }]
    }

    /// Fire whatever deadlines expired by `now` (double-window timeout,
    /// hold conclusion, long press, repeat beats).
    pub fn advance(&mut self, now: std::time::Instant) -> Vec<FiredGesture> {
        let mut fired = Vec::new();
        for (button, (config, state)) in &mut self.buttons {
            if let Some(deadline) = state.double_deadline {
                if deadline <= now {
                    state.double_deadline = None;
                    state.waiting_for_second = false;
                    fired.push(FiredGesture {
                        button: *button,
                        kind: GestureKind::Single,
                    });
                }
            }
            if let Some(deadline) = state.single_conclude_deadline {
                if deadline <= now {
                    state.single_conclude_deadline = None;
                    if state.pressed && config.repeat_single.is_some() && !state.hold_concluded {
                        // Held past the double window: not a double (and not
                        // a quick tap), conclude as single and enter repeat.
                        // A held second press cancels its pending double.
                        state.hold_concluded = true;
                        state.is_second_press = false;
                        state.waiting_for_second = false;
                        fired.push(FiredGesture {
                            button: *button,
                            kind: GestureKind::Single,
                        });
                    }
                }
            }
            if let Some(deadline) = state.long_deadline {
                if deadline <= now {
                    state.long_deadline = None;
                    if state.pressed {
                        state.long_fired = true;
                        fired.push(FiredGesture {
                            button: *button,
                            kind: GestureKind::Long,
                        });
                        if let Some(interval) = config.repeat_long {
                            state.long_repeat_deadline = Some(now + interval);
                        }
                    }
                }
            }
            if let Some(interval) = config.repeat_long {
                if let Some(deadline) = state.long_repeat_deadline {
                    if deadline <= now {
                        if state.pressed {
                            state.long_repeat_deadline = Some(now + interval);
                            fired.push(FiredGesture {
                                button: *button,
                                kind: GestureKind::Long,
                            });
                        } else {
                            state.long_repeat_deadline = None;
                        }
                    }
                }
            }
            if let Some(interval) = config.repeat_single {
                if let Some(deadline) = state.repeat_deadline {
                    if deadline <= now {
                        // With a double column configured, the hold must
                        // first have been concluded as single at the window.
                        let ready =
                            state.pressed && (!config.double_enabled || state.hold_concluded);
                        // While not ready, drop the stale deadline instead
                        // of leaving the engine spinning on past deadlines.
                        state.repeat_deadline = if ready { Some(now + interval) } else { None };
                        if ready {
                            fired.push(FiredGesture {
                                button: *button,
                                kind: GestureKind::Single,
                            });
                        }
                    }
                }
            }
        }
        fired
    }

    /// Earliest pending deadline, for tick scheduling.
    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        self.buttons
            .values()
            .flat_map(|(_, state)| {
                [
                    state.long_deadline,
                    state.double_deadline,
                    state.single_conclude_deadline,
                    state.repeat_deadline,
                    state.long_repeat_deadline,
                ]
            })
            .flatten()
            .min()
    }

    /// Context click swallows this press: pending state is cleared and the
    /// following release finishes silently. Currently unused by the engine;
    /// kept for API completeness.
    pub fn swallow_press(&mut self, button: RemoteButton) {
        if let Some((_, state)) = self.buttons.get_mut(&button) {
            *state = ButtonGestureState {
                swallowing: true,
                ..ButtonGestureState::default()
            };
        }
    }

    /// Reset every button's gesture state without firing anything (link
    /// teardown / engine release-all).
    pub fn release_all(&mut self) {
        for (_, state) in self.buttons.values_mut() {
            *state = ButtonGestureState::default();
        }
    }
}

fn fresh_states(configs: &GestureConfigs) -> GestureConfigsStates {
    configs
        .iter()
        .map(|(button, config)| (*button, (*config, ButtonGestureState::default())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{
        ButtonAction, ButtonActions, ButtonMap, MediaKey, MouseClickButton, VKey,
    };

    fn instant_ms(ms: u64) -> std::time::Instant {
        // Any instant works: the recognizer only ever sees caller-injected
        // time. Anchoring on now keeps arithmetic small and realistic.
        std::time::Instant::now() + Duration::from_millis(ms)
    }

    fn mappings_single(button: RemoteButton, action: ButtonAction) -> ButtonMappings {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            button,
            ButtonActions {
                single: action,
                ..ButtonActions::default()
            },
        );
        mappings
    }

    fn mappings_columns(
        button: RemoteButton,
        single: ButtonAction,
        double: ButtonAction,
        long: ButtonAction,
        hold_repeat: Option<ButtonTrigger>,
    ) -> ButtonMappings {
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings.buttons.0.insert(
            button,
            ButtonActions {
                single,
                double,
                long,
                hold_repeat,
            },
        );
        mappings
    }

    fn single_chord(key: VKey) -> ButtonAction {
        ButtonAction::Shortcut { chord: vec![key] }
    }

    fn fired_kind(gestures: &[FiredGesture]) -> Vec<GestureKind> {
        gestures.iter().map(|g| g.kind).collect()
    }

    #[test]
    fn immediate_single_when_only_single_is_configured() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_single(
            RemoteButton::Ok,
            single_chord(VKey::RETURN),
        )));
        let fired = recognizer.press(RemoteButton::Ok, t0);
        assert_eq!(fired_kind(&fired), vec![GestureKind::Single]);
        // Held without hold-to-repeat: nothing further fires.
        assert!(recognizer.advance(t0 + Duration::from_secs(3)).is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t0 + Duration::from_secs(3))
            .is_empty());
        // Next press fires again immediately.
        assert_eq!(
            fired_kind(&recognizer.press(RemoteButton::Ok, t0 + Duration::from_secs(4))),
            vec![GestureKind::Single]
        );
    }

    #[test]
    fn double_window_both_outcomes() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Ok,
            single_chord(VKey::RETURN),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            None,
        )));

        // First click: no immediate single, window opens on release.
        assert!(recognizer.press(RemoteButton::Ok, t0).is_empty());
        let released = t0 + Duration::from_millis(60);
        assert!(recognizer.release(RemoteButton::Ok, released).is_empty());

        // Outcome 1: window expires -> single fires at the deadline edge.
        assert!(recognizer.advance(released + DOUBLE_CLICK_WINDOW - Duration::from_millis(1)).is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(released + DOUBLE_CLICK_WINDOW)),
            vec![GestureKind::Single]
        );
        // Window consumed: no repeat firing.
        assert!(recognizer
            .advance(released + DOUBLE_CLICK_WINDOW * 2)
            .is_empty());

        // Outcome 2: second press inside the window -> release fires double.
        let t2 = t0 + Duration::from_secs(1);
        assert!(recognizer.press(RemoteButton::Ok, t2).is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t2 + Duration::from_millis(50))
            .is_empty());
        let second = t2 + Duration::from_millis(120); // within 300 ms of first release
        assert!(recognizer.press(RemoteButton::Ok, second).is_empty());
        assert_eq!(
            fired_kind(&recognizer.release(
                RemoteButton::Ok,
                second + Duration::from_millis(40)
            )),
            vec![GestureKind::Double]
        );
    }

    #[test]
    fn long_fires_at_threshold_boundary() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Menu,
            single_chord(VKey::RETURN),
            ButtonAction::Disabled,
            single_chord(VKey::ESCAPE),
            None,
        )));

        // Quick tap with long configured: single fires on release.
        assert!(recognizer.press(RemoteButton::Menu, t0).is_empty());
        assert_eq!(
            fired_kind(&recognizer.release(
                RemoteButton::Menu,
                t0 + Duration::from_millis(120)
            )),
            vec![GestureKind::Single]
        );

        // Hold: nothing at 549 ms, long at exactly 550 ms.
        let t1 = t0 + Duration::from_secs(1);
        assert!(recognizer.press(RemoteButton::Menu, t1).is_empty());
        assert!(recognizer
            .advance(t1 + LONG_PRESS_THRESHOLD - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t1 + LONG_PRESS_THRESHOLD)),
            vec![GestureKind::Long]
        );
        // Release after long: silent.
        assert!(recognizer
            .release(
                RemoteButton::Menu,
                t1 + LONG_PRESS_THRESHOLD + Duration::from_millis(100)
            )
            .is_empty());
    }

    #[test]
    fn repeat_starts_after_delay_and_beats_at_interval() {
        let t0 = instant_ms(0);
        let mut mappings = mappings_columns(
            RemoteButton::Up,
            single_chord(VKey::UP),
            ButtonAction::Disabled,
            ButtonAction::Disabled,
            Some(ButtonTrigger::Single),
        );
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        let interval = RemoteButton::Up.repeat_interval(); // 100 ms

        assert_eq!(
            fired_kind(&recognizer.press(RemoteButton::Up, t0)),
            vec![GestureKind::Single] // immediate first shot
        );
        assert!(recognizer
            .advance(t0 + REPEAT_START_DELAY - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + REPEAT_START_DELAY)),
            vec![GestureKind::Single]
        );
        assert!(recognizer
            .advance(t0 + REPEAT_START_DELAY + interval - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + REPEAT_START_DELAY + interval)),
            vec![GestureKind::Single]
        );
        // Release cancels the stream.
        assert!(recognizer
            .release(RemoteButton::Up, t0 + REPEAT_START_DELAY + interval * 2)
            .is_empty());
        assert!(recognizer
            .advance(t0 + REPEAT_START_DELAY + interval * 5)
            .is_empty());

        // Repeat-single is only derived when long is NOT configured.
        mappings.buttons.0.get_mut(&RemoteButton::Up).unwrap().long =
            single_chord(VKey::ESCAPE);
        let recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        let config = recognizer.buttons.get(&RemoteButton::Up).unwrap().0;
        assert!(config.repeat_single.is_none(), "long configured wins over single repeat");
        assert!(config.repeat_long.is_none(), "hold_repeat targets single, not long");
    }

    #[test]
    fn long_repeat_fires_then_beats_until_release() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Down,
            ButtonAction::Disabled,
            ButtonAction::Disabled,
            ButtonAction::MediaKey {
                key: MediaKey::VolumeDown,
            },
            Some(ButtonTrigger::Long),
        )));
        let interval = RemoteButton::Down.repeat_interval(); // 100 ms

        assert!(recognizer.press(RemoteButton::Down, t0).is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + LONG_PRESS_THRESHOLD)),
            vec![GestureKind::Long]
        );
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + LONG_PRESS_THRESHOLD + interval)),
            vec![GestureKind::Long]
        );
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + LONG_PRESS_THRESHOLD + interval * 2)),
            vec![GestureKind::Long]
        );
        assert!(recognizer
            .release(
                RemoteButton::Down,
                t0 + LONG_PRESS_THRESHOLD + interval * 2 + Duration::from_millis(10)
            )
            .is_empty());
        assert!(recognizer
            .advance(t0 + LONG_PRESS_THRESHOLD + interval * 10)
            .is_empty());
    }

    #[test]
    fn hold_with_double_and_repeat_concludes_at_the_window() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Back,
            single_chord(VKey::ESCAPE),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            Some(ButtonTrigger::Single),
        )));
        let interval = RemoteButton::Back.repeat_interval(); // 100 ms

        // Held: nothing on press; the window concludes it as single.
        assert!(recognizer.press(RemoteButton::Back, t0).is_empty());
        assert!(recognizer.advance(t0 + DOUBLE_CLICK_WINDOW - Duration::from_millis(1)).is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + DOUBLE_CLICK_WINDOW)),
            vec![GestureKind::Single]
        );
        // Repeat starts at the start delay after press.
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + REPEAT_START_DELAY)),
            vec![GestureKind::Single]
        );
        assert_eq!(
            fired_kind(&recognizer.advance(t0 + REPEAT_START_DELAY + interval)),
            vec![GestureKind::Single]
        );
        // Release after hold_concluded: silent, no double window.
        assert!(recognizer
            .release(RemoteButton::Back, t0 + Duration::from_millis(500))
            .is_empty());
        assert!(recognizer.advance(t0 + Duration::from_secs(2)).is_empty());

        // Quick tap still routes through the double window (single after
        // timeout), not through the hold path.
        let t2 = t0 + Duration::from_secs(3);
        assert!(recognizer.press(RemoteButton::Back, t2).is_empty());
        assert!(recognizer
            .release(RemoteButton::Back, t2 + Duration::from_millis(80))
            .is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(
                t2 + Duration::from_millis(80) + DOUBLE_CLICK_WINDOW
            )),
            vec![GestureKind::Single]
        );
    }

    #[test]
    fn held_second_press_cancels_double_and_concludes_single() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Back,
            single_chord(VKey::ESCAPE),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            Some(ButtonTrigger::Single),
        )));

        assert!(recognizer.press(RemoteButton::Back, t0).is_empty());
        assert!(recognizer
            .release(RemoteButton::Back, t0 + Duration::from_millis(80))
            .is_empty());
        let t1 = t0 + Duration::from_millis(150);
        assert!(recognizer.press(RemoteButton::Back, t1).is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(t1 + DOUBLE_CLICK_WINDOW)),
            vec![GestureKind::Single]
        );
        assert_eq!(
            fired_kind(&recognizer.advance(t1 + REPEAT_START_DELAY)),
            vec![GestureKind::Single]
        );
        // No double from the held second press.
        assert!(recognizer
            .release(RemoteButton::Back, t1 + Duration::from_millis(600))
            .is_empty());
    }

    #[test]
    fn second_press_can_still_long_press() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Home,
            single_chord(VKey::RETURN),
            single_chord(VKey::SPACE),
            single_chord(VKey::ESCAPE),
            None,
        )));

        assert!(recognizer.press(RemoteButton::Home, t0).is_empty());
        assert!(recognizer
            .release(RemoteButton::Home, t0 + Duration::from_millis(60))
            .is_empty());
        let t1 = t0 + Duration::from_millis(200);
        assert!(recognizer.press(RemoteButton::Home, t1).is_empty());
        // Long timer restarts on the second press.
        assert_eq!(
            fired_kind(&recognizer.advance(t1 + LONG_PRESS_THRESHOLD)),
            vec![GestureKind::Long]
        );
        assert!(recognizer
            .release(
                RemoteButton::Home,
                t1 + LONG_PRESS_THRESHOLD + Duration::from_millis(50)
            )
            .is_empty());
    }

    #[test]
    fn release_all_cancels_pending_gestures_without_firing() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Back,
            single_chord(VKey::ESCAPE),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            None,
        )));

        // Pending double window discarded: no late single.
        assert!(recognizer.press(RemoteButton::Back, t0).is_empty());
        assert!(recognizer
            .release(RemoteButton::Back, t0 + Duration::from_millis(40))
            .is_empty());
        recognizer.release_all();
        assert!(recognizer.advance(t0 + Duration::from_secs(2)).is_empty());

        // Held long pending discarded: no late long.
        let t1 = t0 + Duration::from_secs(3);
        assert!(recognizer.press(RemoteButton::Back, t1).is_empty());
        recognizer.release_all();
        assert!(recognizer
            .advance(t1 + LONG_PRESS_THRESHOLD + Duration::from_millis(10))
            .is_empty());
        assert!(recognizer.next_deadline().is_none());
    }

    #[test]
    fn reconfigure_resets_state_and_applies_new_configs() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Ok,
            single_chord(VKey::RETURN),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            None,
        )));

        // Open a double window, then reconfigure to double-only removal.
        assert!(recognizer.press(RemoteButton::Ok, t0).is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t0 + Duration::from_millis(50))
            .is_empty());
        recognizer.reconfigure(&configs_from_mappings(&mappings_single(
            RemoteButton::Ok,
            single_chord(VKey::RETURN),
        )));
        // The stale window must not fire the old behavior.
        assert!(recognizer
            .advance(t0 + Duration::from_millis(50) + DOUBLE_CLICK_WINDOW)
            .is_empty());
        // New config: raw path, immediate single on press.
        assert_eq!(
            fired_kind(&recognizer.press(RemoteButton::Ok, t0 + Duration::from_secs(1))),
            vec![GestureKind::Single]
        );

        // Reconfiguring to disabled removes the button entirely.
        recognizer.reconfigure(&configs_from_mappings(&ButtonMappings::default()));
        assert!(recognizer
            .press(RemoteButton::Ok, t0 + Duration::from_secs(2))
            .is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t0 + Duration::from_secs(2))
            .is_empty());
    }

    #[test]
    fn disabled_or_unconfigured_buttons_are_ignored() {
        let t0 = instant_ms(0);
        // Disabled mappings: nothing fires even for inserted actions.
        let mut mappings = mappings_single(RemoteButton::Ok, single_chord(VKey::RETURN));
        mappings.enabled = false;
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        assert!(recognizer.press(RemoteButton::Ok, t0).is_empty());
        assert!(recognizer.release(RemoteButton::Ok, t0).is_empty());
        assert!(recognizer.advance(t0 + Duration::from_secs(5)).is_empty());
        assert_eq!(recognizer.next_deadline(), None);

        // A button entry with only Disabled actions is equally ignored.
        let mut mappings = ButtonMappings {
            enabled: true,
            buttons: ButtonMap::default(),
        };
        mappings
            .buttons
            .0
            .insert(RemoteButton::Tv, ButtonActions::default());
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        assert!(recognizer.press(RemoteButton::Tv, t0).is_empty());
        assert!(recognizer.next_deadline().is_none());
    }

    #[test]
    fn deferred_single_lock_workstation_waits_for_release() {
        let t0 = instant_ms(0);
        let mut mappings = mappings_single(
            RemoteButton::Power,
            ButtonAction::Shortcut {
                chord: vec![VKey::LWIN, VKey(0x4C)],
            },
        );
        mappings.enabled = true;
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        let config = recognizer.buttons.get(&RemoteButton::Power).unwrap().0;
        assert!(config.defer_single_until_release);

        assert!(recognizer.press(RemoteButton::Power, t0).is_empty());
        assert!(recognizer.next_deadline().is_none());
        assert_eq!(
            fired_kind(&recognizer.release(
                RemoteButton::Power,
                t0 + Duration::from_millis(80)
            )),
            vec![GestureKind::Single]
        );
    }

    #[test]
    fn non_repeatable_action_ignores_hold_repeat_request() {
        let mut mappings = mappings_columns(
            RemoteButton::Ok,
            ButtonAction::MouseClick {
                button: MouseClickButton::Left,
            },
            ButtonAction::Disabled,
            ButtonAction::Disabled,
            Some(ButtonTrigger::Single),
        );
        let recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        let (config, _) = recognizer.buttons.get(&RemoteButton::Ok).unwrap();
        assert!(
            config.repeat_single.is_none(),
            "mouse click must not repeat"
        );

        // Same for a long column whose action cannot repeat.
        let entry = mappings.buttons.0.get_mut(&RemoteButton::Ok).unwrap();
        entry.long = ButtonAction::MouseClick {
            button: MouseClickButton::Right,
        };
        entry.hold_repeat = Some(ButtonTrigger::Long);
        let recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings));
        let (config, _) = recognizer.buttons.get(&RemoteButton::Ok).unwrap();
        assert!(config.repeat_long.is_none());
    }

    #[test]
    fn next_deadline_reports_the_earliest_timer() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Up,
            single_chord(VKey::UP),
            ButtonAction::Disabled,
            ButtonAction::Disabled,
            Some(ButtonTrigger::Single),
        )));

        recognizer.press(RemoteButton::Up, t0);
        // Raw-path single repeat deadline sits at the start delay.
        assert_eq!(recognizer.next_deadline(), Some(t0 + REPEAT_START_DELAY));

        // A double window opened earlier must win over the repeat deadline.
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Down,
            single_chord(VKey::DOWN),
            single_chord(VKey::SPACE),
            ButtonAction::Disabled,
            None,
        )));
        recognizer.press(RemoteButton::Down, t0);
        let released = t0 + Duration::from_millis(30);
        recognizer.release(RemoteButton::Down, released);
        assert_eq!(
            recognizer.next_deadline(),
            Some(released + DOUBLE_CLICK_WINDOW)
        );
    }

    #[test]
    fn swallow_press_stays_silent_until_release() {
        let t0 = instant_ms(0);
        let mut recognizer = GestureRecognizer::new(&configs_from_mappings(&mappings_columns(
            RemoteButton::Ok,
            single_chord(VKey::RETURN),
            single_chord(VKey::SPACE),
            single_chord(VKey::ESCAPE),
            None,
        )));

        // Pending double window from a first tap.
        assert!(recognizer.press(RemoteButton::Ok, t0).is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t0 + Duration::from_millis(60))
            .is_empty());

        recognizer.swallow_press(RemoteButton::Ok);
        assert!(recognizer
            .press(RemoteButton::Ok, t0 + Duration::from_millis(200))
            .is_empty());
        assert!(recognizer
            .advance(t0 + Duration::from_millis(200) + LONG_PRESS_THRESHOLD)
            .is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t0 + Duration::from_millis(500))
            .is_empty());
        assert!(
            recognizer.advance(t0 + Duration::from_secs(1)).is_empty(),
            "swallowed press must not leave late singles/doubles"
        );

        // After the swallowing release the button behaves normally again.
        let t1 = t0 + Duration::from_secs(2);
        assert!(recognizer.press(RemoteButton::Ok, t1).is_empty());
        assert!(recognizer
            .release(RemoteButton::Ok, t1 + Duration::from_millis(50))
            .is_empty());
        assert_eq!(
            fired_kind(&recognizer.advance(
                t1 + Duration::from_millis(50) + DOUBLE_CLICK_WINDOW
            )),
            vec![GestureKind::Single]
        );
    }
}
