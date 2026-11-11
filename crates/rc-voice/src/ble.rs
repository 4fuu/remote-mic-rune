//! The ATVV voice session: BLE connection lifecycle, GATT notification
//! handling and the streaming decode pipeline.
//!
//! Ported from the hardware-verified `xiaomi-remote-mic` implementation.
//! The session is driven entirely by the remote over GATT: the remote asks
//! to open the microphone (control opcode 0x08) when its voice key is
//! held, streams ADPCM audio notifications, and signals stop (0x00) on
//! release. The HID voice key itself is not needed here — remote-mic-rune
//! keeps handling it at the report layer (see voice-key synthesis).
//!
//! Pure pieces (status, events, reconnect policy, the decode session) are
//! host-tested; the connection loop and the WinRT GATT backend compile
//! only on Windows, mirroring rc-agent's hook gating.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};

use crate::atvv::{Capabilities, FrameAccumulator, ImaAdpcmDecoder, process_pcm};
use crate::audio::AudioQueue;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const STALE_AUDIO_AFTER_STOP: Duration = Duration::from_millis(300);
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const AUDIO_DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BleStatus {
    Stopped,
    Scanning,
    Connecting(String),
    Initializing(String),
    Ready(String),
    WaitingReconnect { seconds: u64 },
    Failed(String),
}

impl Default for BleStatus {
    fn default() -> Self {
        BleStatus::Stopped
    }
}

#[derive(Clone, Debug)]
pub enum BleEvent {
    Status(BleStatus),
    VoiceStreaming(bool),
    Battery(Option<u8>),
    Samples(u64),
    Error(String),
}

/// What the streaming session reads from shared app settings while running.
#[derive(Debug, Default)]
pub struct StreamConfig {
    /// Post-decode gain in dB, clamped to ±24 by the postprocessor.
    pub gain_db: f32,
}

#[derive(Debug, Default)]
pub struct ReconnectPolicy {
    attempt: u32,
}

impl ReconnectPolicy {
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        let exponent = self.attempt.min(5);
        self.attempt = self.attempt.saturating_add(1);
        let base = (3_u64.saturating_mul(1_u64 << exponent)).min(60) as f64;
        Duration::from_secs_f64((base * (1.0 + jitter.clamp(-0.1, 0.1))).min(60.0))
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct VoiceSession {
    capabilities: Capabilities,
    decoder: ImaAdpcmDecoder,
    accumulator: FrameAccumulator,
    pending_sync: Option<(i32, i32)>,
    streaming: bool,
    session_id: u8,
    last_stop: Option<Instant>,
    decoded_samples: u64,
    audio_unavailable_reported: bool,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl VoiceSession {
    fn new(capabilities: Capabilities, low_nibble_first: bool) -> Self {
        let mut decoder = ImaAdpcmDecoder::default();
        decoder.set_low_nibble_first(low_nibble_first);
        Self {
            capabilities,
            decoder,
            accumulator: FrameAccumulator::default(),
            pending_sync: None,
            streaming: false,
            session_id: 0,
            last_stop: None,
            decoded_samples: 0,
            audio_unavailable_reported: false,
        }
    }

    fn start(&mut self, events: &mpsc::UnboundedSender<BleEvent>) {
        self.accumulator.reset();
        self.pending_sync = None;
        self.decoder.reset(0, 0);
        self.last_stop = None;
        if !self.streaming {
            self.streaming = true;
            let _ = events.send(BleEvent::VoiceStreaming(true));
        }
    }

    fn audio(
        &mut self,
        bytes: &[u8],
        gain_db: f32,
        queue: &AudioQueue,
        events: &mpsc::UnboundedSender<BleEvent>,
    ) {
        if !queue.is_active() {
            if !self.audio_unavailable_reported {
                let _ = events.send(BleEvent::Error("音频桥接目标不可用，语音数据已丢弃".into()));
                self.audio_unavailable_reported = true;
            }
            return;
        }
        self.audio_unavailable_reported = false;
        if !self.streaming {
            if self
                .last_stop
                .is_some_and(|stopped| stopped.elapsed() < STALE_AUDIO_AFTER_STOP)
            {
                return;
            }
            self.start(events);
        }
        for frame in self.accumulator.append(bytes, self.capabilities.frame_size) {
            if let Some((predictor, index)) = self.pending_sync.take() {
                self.decoder.reset(predictor, index);
            }
            let samples = process_pcm(&self.decoder.decode(&frame), gain_db);
            self.decoded_samples += samples.len() as u64;
            queue.push(&samples);
            let _ = events.send(BleEvent::Samples(self.decoded_samples));
        }
    }

    async fn stop(&mut self, queue: &AudioQueue, events: &mpsc::UnboundedSender<BleEvent>) {
        if !self.streaming {
            return;
        }
        self.streaming = false;
        self.accumulator.reset();
        self.pending_sync = None;
        self.last_stop = Some(Instant::now());
        let _ = events.send(BleEvent::VoiceStreaming(false));
        let deadline = Instant::now() + AUDIO_DRAIN_TIMEOUT;
        while !queue.is_empty() && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn abort(&mut self, queue: &AudioQueue, events: &mpsc::UnboundedSender<BleEvent>) {
        queue.clear();
        self.accumulator.reset();
        self.pending_sync = None;
        self.streaming = false;
        let _ = events.send(BleEvent::VoiceStreaming(false));
    }
}

// ---------------------------------------------------------------------------
// Windows session loop (WinRT BLE GATT backend below)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod imp {
    use super::*;

    use std::sync::Arc;

    use anyhow::{Context as _, Result, bail};
    use parking_lot::RwLock;
    use rand::RngExt;
    use tokio::sync::watch;
    use tokio::time::{sleep, timeout};

    use crate::atvv::{
        AUDIO_UUID, BATTERY_LEVEL_UUID, CONTROL_UUID, GET_CAPABILITIES_V1, MODEL_NUMBER_UUID,
        TRANSMIT_UUID, microphone_close, microphone_open,
    };
    use super::windows::{Characteristic, Peripheral, ValueNotification, WriteType};

    /// Run the connection lifecycle until `shutdown` flips true. `cached_id`
    /// remembers the last connected remote's device id for the next cycle
    /// (in-memory only; one remote is the supported shape).
    pub async fn run(
        config: Arc<RwLock<StreamConfig>>,
        cached_id: &mut Option<String>,
        queue: AudioQueue,
        events: mpsc::UnboundedSender<BleEvent>,
        mut reconnect: watch::Receiver<u64>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut policy = ReconnectPolicy::default();
        let mut bypass_cached = false;
        loop {
            if *shutdown.borrow() {
                let _ = events.send(BleEvent::Status(BleStatus::Stopped));
                break;
            }
            let generation = *reconnect.borrow();
            let result = connection_cycle(
                &config,
                cached_id,
                &queue,
                &events,
                &mut reconnect,
                &mut shutdown,
                generation,
                bypass_cached,
            )
            .await;
            match result {
                Ok(CycleEnd::Shutdown) => {
                    let _ = events.send(BleEvent::Status(BleStatus::Stopped));
                    break;
                }
                Ok(CycleEnd::ManualReconnect) => {
                    policy.reset();
                    bypass_cached = false;
                    continue;
                }
                Ok(CycleEnd::Disconnected) => {
                    bypass_cached = false;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let _ = events.send(BleEvent::Status(BleStatus::Failed(message)));
                    bypass_cached = true;
                }
            }

            let jitter = rand::rng().random_range(-0.1..=0.1);
            let delay = policy.next_delay(jitter);
            let _ = events.send(BleEvent::Status(BleStatus::WaitingReconnect {
                seconds: delay.as_secs().max(1),
            }));
            tokio::select! {
                _ = sleep(delay) => {}
                _ = reconnect.changed() => {
                    policy.reset();
                    bypass_cached = false;
                }
                _ = shutdown.changed() => {}
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CycleEnd {
        ManualReconnect,
        Disconnected,
        Shutdown,
    }

    #[allow(clippy::too_many_arguments)]
    async fn connection_cycle(
        config: &Arc<RwLock<StreamConfig>>,
        cached_id: &mut Option<String>,
        queue: &AudioQueue,
        events: &mpsc::UnboundedSender<BleEvent>,
        reconnect: &mut watch::Receiver<u64>,
        shutdown: &mut watch::Receiver<bool>,
        generation: u64,
        bypass_cached: bool,
    ) -> Result<CycleEnd> {
        let _ = events.send(BleEvent::Status(BleStatus::Scanning));
        let peripheral = match find_peripheral(cached_id, reconnect, shutdown, bypass_cached).await?
        {
            Discovery::Found(peripheral) => peripheral,
            Discovery::ManualReconnect => return Ok(CycleEnd::ManualReconnect),
            Discovery::Shutdown => return Ok(CycleEnd::Shutdown),
        };
        let name = peripheral.name().to_owned();
        let _ = events.send(BleEvent::Status(BleStatus::Connecting(name.clone())));

        let connect_result: Result<()> = tokio::select! {
            result = timeout(CONNECTION_TIMEOUT, peripheral.connect()) => {
                result
                    .context("连接遥控器超时")
                    .and_then(|result| result.context("连接遥控器失败"))
            }
            _ = reconnect.changed() => {
                let _ = peripheral.disconnect().await;
                return Ok(CycleEnd::ManualReconnect);
            },
            _ = shutdown.changed() => {
                let _ = peripheral.disconnect().await;
                return Ok(CycleEnd::Shutdown);
            },
        };
        if let Err(error) = connect_result {
            let _ = peripheral.disconnect().await;
            return Err(error);
        }
        let _ = events.send(BleEvent::Status(BleStatus::Initializing(name.clone())));
        let channels_result: Result<InitializedChannels> = tokio::select! {
            result = timeout(INITIALIZATION_TIMEOUT, initialize(&peripheral)) => {
                result.context("初始化语音通道超时").and_then(|result| result)
            }
            _ = reconnect.changed() => {
                let _ = peripheral.disconnect().await;
                return Ok(CycleEnd::ManualReconnect);
            },
            _ = shutdown.changed() => {
                let _ = peripheral.disconnect().await;
                return Ok(CycleEnd::Shutdown);
            },
        };
        let channels = match channels_result {
            Ok(channels) => channels,
            Err(error) => {
                let _ = peripheral.disconnect().await;
                return Err(error);
            }
        };
        run_ready(
            peripheral,
            name,
            channels,
            config,
            cached_id,
            queue,
            events,
            reconnect,
            shutdown,
            generation,
        )
        .await
    }

    enum Discovery {
        Found(Peripheral),
        ManualReconnect,
        Shutdown,
    }

    async fn find_peripheral(
        cached_id: &mut Option<String>,
        reconnect: &mut watch::Receiver<u64>,
        shutdown: &mut watch::Receiver<bool>,
        bypass_cached: bool,
    ) -> Result<Discovery> {
        let cached = (!bypass_cached)
            .then(|| cached_id.clone())
            .flatten();
        tokio::select! {
            result = timeout(CONNECTION_TIMEOUT, Peripheral::discover(cached.as_deref())) => {
                let peripheral = result
                    .context("枚举已配对的语音遥控器超时")??
                    .context("未找到已配对且包含 ATVV 服务的小米语音遥控器")?;
                Ok(Discovery::Found(peripheral))
            }
            _ = reconnect.changed() => {
                Ok(Discovery::ManualReconnect)
            }
            _ = shutdown.changed() => {
                Ok(Discovery::Shutdown)
            }
        }
    }

    struct Channels {
        transmit: Characteristic,
        audio: Characteristic,
        control: Characteristic,
        battery: Option<Characteristic>,
        capabilities: Capabilities,
        low_nibble_first: bool,
    }

    struct InitializedChannels {
        channels: Channels,
        notifications: mpsc::UnboundedReceiver<ValueNotification>,
    }

    async fn initialize(peripheral: &Peripheral) -> Result<InitializedChannels> {
        peripheral
            .discover_services()
            .await
            .context("发现 GATT 服务失败")?;
        let characteristics = peripheral.characteristics();
        let transmit =
            characteristic(&characteristics, TRANSMIT_UUID).context("遥控器缺少 ATVV 发送通道")?;
        let audio = characteristic(&characteristics, AUDIO_UUID).context("遥控器缺少 ATVV 音频通道")?;
        let control =
            characteristic(&characteristics, CONTROL_UUID).context("遥控器缺少 ATVV 控制通道")?;
        peripheral
            .subscribe(&audio)
            .await
            .context("订阅 ATVV 音频失败")?;
        peripheral
            .subscribe(&control)
            .await
            .context("订阅 ATVV 控制失败")?;

        let model = if let Some(model) = characteristic(&characteristics, MODEL_NUMBER_UUID) {
            peripheral
                .read(&model)
                .await
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let battery = characteristic(&characteristics, BATTERY_LEVEL_UUID);
        let write_type = transmit.preferred_write_type();
        let mut notifications = peripheral.notifications().context("无法读取 GATT 通知")?;
        peripheral
            .write(&transmit, GET_CAPABILITIES_V1, write_type)
            .await
            .context("发送 ATVV 能力请求失败")?;

        let capabilities = loop {
            let notification = timeout(INITIALIZATION_TIMEOUT, notifications.recv())
                .await
                .context("等待 ATVV 能力响应超时")?
                .context("ATVV 通知流提前关闭")?;
            if notification.uuid == CONTROL_UUID && notification.value.first() == Some(&0x0b) {
                break Capabilities::parse(&notification.value).context("ATVV 能力响应格式无效")?;
            }
        };
        if capabilities.sample_rate != 16_000 {
            bail!(
                "遥控器仅提供 {} Hz 音频，当前仅支持 16000 Hz",
                capabilities.sample_rate
            );
        }
        Ok(InitializedChannels {
            channels: Channels {
                transmit,
                audio,
                control,
                battery,
                capabilities,
                low_nibble_first: model.trim().to_uppercase().contains("ARN9"),
            },
            notifications,
        })
    }

    fn characteristic(characteristics: &[Characteristic], uuid: uuid::Uuid) -> Option<Characteristic> {
        characteristics
            .iter()
            .find(|item| item.uuid == uuid)
            .cloned()
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_ready(
        peripheral: Peripheral,
        name: String,
        initialized: InitializedChannels,
        config: &Arc<RwLock<StreamConfig>>,
        cached_id: &mut Option<String>,
        queue: &AudioQueue,
        events: &mpsc::UnboundedSender<BleEvent>,
        reconnect: &mut watch::Receiver<u64>,
        shutdown: &mut watch::Receiver<bool>,
        generation: u64,
    ) -> Result<CycleEnd> {
        let InitializedChannels {
            channels,
            mut notifications,
        } = initialized;
        *cached_id = Some(peripheral.id().to_owned());
        let _ = events.send(BleEvent::Status(BleStatus::Ready(name)));
        if let Some(battery) = &channels.battery {
            if let Ok(value) = peripheral.read(battery).await {
                let _ = events.send(BleEvent::Battery(value.first().copied()));
            }
        }

        let write_type = channels.transmit.preferred_write_type();
        let mut session = VoiceSession::new(channels.capabilities, channels.low_nibble_first);
        let mut health_tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                notification = notifications.recv() => {
                    let Some(notification) = notification else {
                        terminate_session(
                            &peripheral,
                            &channels,
                            write_type,
                            &mut session,
                            queue,
                            events,
                        ).await;
                        return Ok(CycleEnd::Disconnected);
                    };
                    if let Err(error) = handle_notification(
                        &peripheral,
                        &channels,
                        write_type,
                        notification,
                        &mut session,
                        config,
                        queue,
                        events,
                    ).await {
                        terminate_session(
                            &peripheral,
                            &channels,
                            write_type,
                            &mut session,
                            queue,
                            events,
                        ).await;
                        let _ = peripheral.disconnect().await;
                        return Err(error);
                    }
                }
                _ = health_tick.tick() => {
                    if !peripheral.is_connected().await.unwrap_or(false) {
                        terminate_session(
                            &peripheral,
                            &channels,
                            write_type,
                            &mut session,
                            queue,
                            events,
                        ).await;
                        return Ok(CycleEnd::Disconnected);
                    }
                }
                _ = reconnect.changed() => {
                    if *reconnect.borrow() != generation {
                        terminate_session(
                            &peripheral,
                            &channels,
                            write_type,
                            &mut session,
                            queue,
                            events,
                        ).await;
                        let _ = peripheral.disconnect().await;
                        return Ok(CycleEnd::ManualReconnect);
                    }
                }
                _ = shutdown.changed() => {
                    terminate_session(
                        &peripheral,
                        &channels,
                        write_type,
                        &mut session,
                        queue,
                        events,
                    ).await;
                    let _ = peripheral.disconnect().await;
                    return Ok(CycleEnd::Shutdown);
                }
            }
        }
    }

    async fn terminate_session(
        peripheral: &Peripheral,
        channels: &Channels,
        write_type: WriteType,
        session: &mut VoiceSession,
        queue: &AudioQueue,
        events: &mpsc::UnboundedSender<BleEvent>,
    ) {
        if session.streaming {
            let _ = peripheral
                .write(
                    &channels.transmit,
                    &microphone_close(session.capabilities.version, session.session_id),
                    write_type,
                )
                .await;
        }
        session.abort(queue, events);
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_notification(
        peripheral: &Peripheral,
        channels: &Channels,
        write_type: WriteType,
        notification: ValueNotification,
        session: &mut VoiceSession,
        config: &Arc<RwLock<StreamConfig>>,
        queue: &AudioQueue,
        events: &mpsc::UnboundedSender<BleEvent>,
    ) -> Result<()> {
        if notification.uuid == channels.audio.uuid {
            let gain = config.read().gain_db;
            session.audio(&notification.value, gain, queue, events);
            return Ok(());
        }
        if notification.uuid
            == channels
                .battery
                .as_ref()
                .map(|value| value.uuid)
                .unwrap_or_default()
        {
            let _ = events.send(BleEvent::Battery(notification.value.first().copied()));
            return Ok(());
        }
        if notification.uuid != channels.control.uuid {
            return Ok(());
        }
        let Some(opcode) = notification.value.first().copied() else {
            return Ok(());
        };
        match opcode {
            0x08 => {
                peripheral
                    .write(
                        &channels.transmit,
                        &microphone_open(
                            session.capabilities.version,
                            session.capabilities.selected_codec,
                        ),
                        write_type,
                    )
                    .await
                    .context("回应遥控器麦克风开启请求失败")?;
            }
            0x04 => {
                if notification.value.len() >= 3 && notification.value[2] != 0x02 {
                    bail!("语音流切换到不支持的 8 kHz 编解码器");
                }
                session.session_id = notification.value.get(3).copied().unwrap_or(0);
                if queue.is_active() {
                    session.start(events);
                } else {
                    let _ = peripheral
                        .write(
                            &channels.transmit,
                            &microphone_close(session.capabilities.version, session.session_id),
                            write_type,
                        )
                        .await;
                    let _ = events.send(BleEvent::Error(
                        "尚未选择可用的音频桥接目标，已拒绝本次语音流".into(),
                    ));
                }
            }
            0x00 => session.stop(queue, events).await,
            0x0a if notification.value.len() >= 7 => {
                let predictor =
                    i16::from_be_bytes([notification.value[4], notification.value[5]]) as i32;
                session.pending_sync = Some((predictor, notification.value[6] as i32));
                session.accumulator.reset();
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use imp::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_doubles_caps_and_bounds_jitter() {
        let mut policy = ReconnectPolicy::default();
        let seconds: Vec<_> = (0..7).map(|_| policy.next_delay(0.0).as_secs()).collect();
        assert_eq!(seconds, [3, 6, 12, 24, 48, 60, 60]);
        policy.reset();
        assert_eq!(policy.next_delay(-0.1).as_millis(), 2700);
        policy.reset();
        assert_eq!(policy.next_delay(0.1).as_millis(), 3300);
    }

    #[test]
    fn voice_session_only_publishes_atvv_stream_state() {
        use crate::audio::AudioQueue;
        let queue = AudioQueue::default();
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut session = VoiceSession::new(Capabilities::DEFAULT, false);

        session.start(&events);
        assert!(session.streaming);
        assert!(matches!(
            receiver.try_recv(),
            Ok(BleEvent::VoiceStreaming(true))
        ));

        session.abort(&queue, &events);
        assert!(!session.streaming);
        assert!(matches!(
            receiver.try_recv(),
            Ok(BleEvent::VoiceStreaming(false))
        ));
    }
}
