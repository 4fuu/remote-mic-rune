//! The voice bridge runtime: one OS thread hosting the tokio loop that owns
//! the audio output and the BLE session task, adapted from the
//! hardware-verified `xiaomi-remote-mic` runtime (voice path only).
//!
//! The app talks to it through [`VoiceRuntime`]: commands in, a callback
//! and a snapshot out. Enabling the bridge spawns the BLE session task;
//! disabling terminates it. The BLE event receiver is created up front and
//! lives for the whole runtime, so a disabled bridge simply never receives
//! events. Non-Windows hosts keep the same API with the audio stub and no
//! session loop, so the app code is identical everywhere.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc, watch};

use crate::audio::{AudioOutput, AudioQueue};
use crate::ble::{BleEvent, BleStatus, StreamConfig};

/// One command from the app to the runtime.
#[derive(Debug)]
pub enum VoiceCommand {
    SetEnabled(bool),
    SelectDevice(Option<String>),
    SetGain(f32),
    TestTone,
    Reconnect,
    RefreshDevices,
    Shutdown,
}

/// Everything the UI shows about the voice bridge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceSnapshot {
    /// Whether the BLE session task is (requested) running.
    pub enabled: bool,
    pub status: BleStatus,
    pub streaming: bool,
    pub battery: Option<u8>,
    pub samples: u64,
    pub devices: Vec<String>,
    pub selected: Option<String>,
    pub last_error: Option<String>,
}

/// The one event the runtime pushes to the app: the fresh snapshot.
#[derive(Clone, Debug)]
pub struct VoiceEvent(pub VoiceSnapshot);

/// Startup configuration from the app's settings.
#[derive(Clone, Debug, Default)]
pub struct VoiceStart {
    pub output_device: Option<String>,
    pub gain_db: f32,
    pub enabled: bool,
}

pub struct VoiceRuntime {
    commands: mpsc::UnboundedSender<VoiceCommand>,
    snapshot: Arc<Mutex<VoiceSnapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceRuntime {
    /// Start the runtime thread. `on_event` fires from that thread on every
    /// state change; [`VoiceRuntime::snapshot`] is safe to poll anywhere.
    pub fn start(initial: VoiceStart, on_event: Box<dyn Fn(VoiceEvent) + Send>) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(VoiceSnapshot::default()));
        let wants_enabled = initial.enabled;
        let thread = std::thread::Builder::new()
            .name("rc-voice-runtime".into())
            .spawn({
                let snapshot = Arc::clone(&snapshot);
                let initial = initial.clone();
                move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("voice runtime: tokio start");
                    runtime.block_on(run_runtime(initial, command_rx, on_event, snapshot));
                }
            })
            .expect("voice runtime: thread spawn");
        let runtime = Arc::new(Self {
            commands: command_tx,
            snapshot,
            thread: Some(thread),
        });
        if wants_enabled {
            let _ = runtime.commands.send(VoiceCommand::SetEnabled(true));
        }
        runtime
    }

    pub fn send(&self, command: VoiceCommand) {
        let _ = self.commands.send(command);
    }

    pub fn snapshot(&self) -> VoiceSnapshot {
        self.snapshot.lock().clone()
    }
}

impl Drop for VoiceRuntime {
    fn drop(&mut self) {
        let _ = self.commands.send(VoiceCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The running BLE session task, when enabled.
struct Session {
    shutdown: Option<watch::Sender<bool>>,
    reconnect: watch::Sender<u64>,
    generation: u64,
}

impl Session {
    fn new() -> Self {
        Self {
            shutdown: None,
            reconnect: watch::channel(0_u64).0,
            generation: 0,
        }
    }

    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            // The task observes the watch and exits on its own; do not abort
            // it mid-write (a GATT write during teardown would leak the
            // subscription handlers).
            let _ = shutdown.send(true);
        }
    }
}

async fn run_runtime(
    initial: VoiceStart,
    mut commands: mpsc::UnboundedReceiver<VoiceCommand>,
    on_event: Box<dyn Fn(VoiceEvent) + Send>,
    snapshot: Arc<Mutex<VoiceSnapshot>>,
) {
    let config = Arc::new(RwLock::new(StreamConfig {
        gain_db: normalize_gain(initial.gain_db),
    }));
    let queue = AudioQueue::default();
    let mut audio = AudioOutput::new(queue.clone());
    let mut session = Session::new();

    // Created once: the runtime keeps the sender, so the receiver never
    // closes and a disabled bridge simply has no events.
    let ble_events = mpsc::unbounded_channel();
    let mut ble_rx = ble_events.1;

    {
        let mut snap = snapshot.lock();
        snap.selected = initial.output_device.clone();
    }
    if let Err(error) = audio.select(initial.output_device.as_deref()) {
        publish_error(&snapshot, &on_event, format!("音频设备初始化失败：{error:#}"));
    }
    refresh_devices(&snapshot, &on_event, &audio);

    let mut audio_health = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    VoiceCommand::SetEnabled(enabled) => {
                        if enabled && session.shutdown.is_none() {
                            #[cfg(target_os = "windows")]
                            {
                                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                                let (reconnect_tx, reconnect_rx) = watch::channel(session.generation);
                                let config = Arc::clone(&config);
                                let queue = queue.clone();
                                let events = ble_events.0.clone();
                                tokio::spawn(async move {
                                    let mut cached_id = None;
                                    crate::ble::run(config, &mut cached_id, queue, events, reconnect_rx, shutdown_rx).await;
                                });
                                session.shutdown = Some(shutdown_tx);
                                session.reconnect = reconnect_tx;
                                snapshot.lock().enabled = true;
                                publish(&snapshot, &on_event);
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                publish_error(&snapshot, &on_event, "语音桥接仅支持 Windows".to_string());
                            }
                        }
                        if !enabled && session.shutdown.is_some() {
                            session.stop();
                            let mut snap = snapshot.lock();
                            snap.enabled = false;
                            snap.streaming = false;
                            snap.status = BleStatus::Stopped;
                            drop(snap);
                            publish(&snapshot, &on_event);
                        }
                    }
                    VoiceCommand::SelectDevice(name) => {
                        match audio.select(name.as_deref()) {
                            Ok(()) => {
                                let mut snap = snapshot.lock();
                                snap.selected = name;
                                snap.last_error = None;
                                drop(snap);
                                publish(&snapshot, &on_event);
                            }
                            Err(error) => {
                                publish_error(&snapshot, &on_event, format!("切换音频设备失败：{error:#}"));
                                refresh_devices(&snapshot, &on_event, &audio);
                            }
                        }
                    }
                    VoiceCommand::SetGain(gain_db) => {
                        config.write().gain_db = normalize_gain(gain_db);
                    }
                    VoiceCommand::TestTone => {
                        if let Err(error) = audio.test_tone() {
                            publish_error(&snapshot, &on_event, error.to_string());
                        }
                    }
                    VoiceCommand::Reconnect => {
                        if session.shutdown.is_some() {
                            session.generation = session.generation.wrapping_add(1);
                            let _ = session.reconnect.send(session.generation);
                        }
                    }
                    VoiceCommand::RefreshDevices => {
                        refresh_devices(&snapshot, &on_event, &audio);
                    }
                    VoiceCommand::Shutdown => break,
                }
            }
            event = ble_rx.recv() => {
                if let Some(event) = event {
                    let mut snap = snapshot.lock();
                    match event {
                        BleEvent::Status(status) => snap.status = status,
                        BleEvent::VoiceStreaming(streaming) => snap.streaming = streaming,
                        BleEvent::Battery(battery) => snap.battery = battery,
                        BleEvent::Samples(samples) => snap.samples = samples,
                        BleEvent::Error(message) => snap.last_error = Some(message),
                    }
                    drop(snap);
                    publish(&snapshot, &on_event);
                }
            }
            _ = audio_health.tick() => {
                if let Some(message) = audio.take_stream_error() {
                    publish_error(&snapshot, &on_event, format!("音频输出流错误：{message}"));
                }
            }
        }
    }
    session.stop();
    let mut snap = snapshot.lock();
    snap.enabled = false;
    snap.streaming = false;
    snap.status = BleStatus::Stopped;
    drop(snap);
    publish(&snapshot, &on_event);
}

fn normalize_gain(gain_db: f32) -> f32 {
    if gain_db.is_finite() {
        gain_db
    } else {
        10.0
    }
    .clamp(-24.0, 24.0)
}

fn publish(snapshot: &Arc<Mutex<VoiceSnapshot>>, on_event: &Box<dyn Fn(VoiceEvent) + Send>) {
    let snap = snapshot.lock().clone();
    on_event(VoiceEvent(snap));
}

fn publish_error(
    snapshot: &Arc<Mutex<VoiceSnapshot>>,
    on_event: &Box<dyn Fn(VoiceEvent) + Send>,
    message: String,
) {
    snapshot.lock().last_error = Some(message);
    publish(snapshot, on_event);
}

fn refresh_devices(
    snapshot: &Arc<Mutex<VoiceSnapshot>>,
    on_event: &Box<dyn Fn(VoiceEvent) + Send>,
    audio: &AudioOutput,
) {
    match AudioOutput::device_names() {
        Ok(names) => {
            let mut snap = snapshot.lock();
            snap.devices = names;
            if let Some(selected) = audio.selected_name() {
                snap.selected = Some(selected.to_owned());
            }
            drop(snap);
            publish(snapshot, on_event);
        }
        Err(error) => {
            publish_error(snapshot, on_event, format!("无法枚举音频设备：{error:#}"));
        }
    }
}
