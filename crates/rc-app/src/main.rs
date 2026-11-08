//! rc-app entry point: load settings, start the bridge (or detect another
//! instance), spawn the engine pump, then hand everything to the Rune app.
//!
//! Everything before `app.run()` is plain host code; the UI logic lives in
//! `ui.rs` and the seam in `bridge_api.rs`.

mod bridge_api;
mod chord;
mod engine;
mod mappings;
mod real;
mod service;
mod settings;
mod status;
mod task;
mod tray;
mod ui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

use rune::{App, Tx};

use bridge_api::{EngineEvent, SharedBridge, SharedEngine, UiNotice};
use ui::{Services, Startup};

fn main() {
    let demo = std::env::args().any(|arg| arg == "--demo");

    // 1. Settings (corrupt files back up to settings.json.bak automatically).
    let settings_dir = settings::app_dir();
    let loaded = settings_dir
        .as_deref()
        .map(settings::load)
        .unwrap_or(settings::Loaded { settings: Default::default(), warning: None });

    // 2. Bridge + engine through the seam. Real crates by default; --demo
    //    keeps the deterministic stubs (host demos and UI tests use them).
    let (engine_tx, engine_rx) = mpsc::channel::<EngineEvent>();
    let (launch_error, bridge, engine) = if demo {
        match service::StubBridge::start(engine_tx, true) {
            Ok(stub) => (
                None,
                Some(Arc::clone(&stub) as SharedBridge),
                Some(service::StubEngine::new() as SharedEngine),
            ),
            Err(err) => (Some(err.message()), None, None),
        }
    } else {
        match real::start(engine_tx) {
            Ok((real_bridge, real_engine)) => (
                None,
                Some(Arc::clone(&real_bridge) as SharedBridge),
                Some(real_engine as SharedEngine),
            ),
            Err(err) => (Some(err.message()), None, None),
        }
    };

    // The pump installs this once before its loop starts.
    let pump_init = engine::PumpInit {
        mappings: loaded.settings.mappings.clone(),
        capture_enabled: loaded.settings.capture_enabled,
        voice_synth_to: loaded.settings.valid_voice_synth(),
    };

    // 3. The Rune app and its handle (background threads talk to the UI
    //    only through it).
    let app = App::new(rune::AppSpec::<ui::AppState> {
        ..rune::AppSpec::new("Remote Mic Rune", (780.0, 720.0), ui::AppState::from_startup, ui::view)
    });
    let handle = app.handle();

    // Startup data for `init` (fn() -> S cannot take arguments).
    ui::set_startup(Startup {
        settings: loaded.settings,
        settings_dir: settings_dir.clone(),
        load_warning: loaded.warning,
        launch_error,
        bridge_status: bridge
            .as_ref()
            .map(|b| b.snapshot())
            .unwrap_or_default(),
        services: Services { bridge: bridge.clone(), engine: engine.clone() },
        ui: Some(handle.clone()),
    });

    // 4. Engine pump: bridge events -> engine, 25 ms ticks, status -> UI.
    if let (Some(bridge), Some(engine)) = (bridge, engine) {
        let pump = engine::spawn_pump(
            engine_rx,
            bridge,
            engine,
            pump_init,
            engine::PumpTimings::default(),
            Box::new({
                let handle = handle.clone();
                move |notice: UiNotice| {
                    handle.update(move |s: &mut ui::AppState, _: &mut Tx| s.apply_notice(notice));
                }
            }),
            Arc::new(AtomicBool::new(false)),
        );
        // The pump runs for the app's lifetime; park its joiner so the
        // handle never complains about a dangling thread.
        std::thread::spawn(move || {
            let _ = pump.join();
        });
    }

    // 5. Tray icon (best effort; unsupported platforms just skip it).
    tray::spawn(handle);

    app.run();
}
