//! Audio output: a bounded PCM sample queue plus the cpal output stream
//! that drains it into the user-selected virtual audio device (VB-CABLE /
//! Voicemeeter play endpoint; the voice application reads the matching
//! recording endpoint).
//!
//! Ported from the hardware-verified `xiaomi-remote-mic` implementation.
//! The queue, channel duplication, 16 kHz -> device rate resampling and
//! test tone are pure and host-tested; the cpal stream itself is
//! Windows-only (the voice bridge targets Windows, and cpal would pull
//! Linux audio system libraries into the CI test job). Non-Windows hosts
//! compile a stub with the same shape so the runtime code is identical.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_queue::ArrayQueue;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const QUEUE_CAPACITY_SAMPLES: usize = 16_000 * 3;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const INPUT_SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug)]
pub struct AudioQueue {
    samples: Arc<ArrayQueue<i16>>,
    active: Arc<AtomicBool>,
}

impl Default for AudioQueue {
    fn default() -> Self {
        Self {
            samples: Arc::new(ArrayQueue::new(QUEUE_CAPACITY_SAMPLES)),
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AudioQueue {
    pub fn push(&self, samples: &[i16]) {
        for sample in samples {
            if let Err(sample) = self.samples.push(*sample) {
                let _ = self.samples.pop();
                let _ = self.samples.push(sample);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn clear(&self) {
        while self.samples.pop().is_some() {}
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct PlaybackState {
    output_sample_rate: u32,
    accumulator: u32,
    current: i16,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl PlaybackState {
    fn new(output_sample_rate: u32) -> Self {
        Self {
            output_sample_rate,
            accumulator: output_sample_rate,
            current: 0,
        }
    }

    fn next(&mut self, queue: &ArrayQueue<i16>) -> i16 {
        while self.accumulator >= self.output_sample_rate {
            self.current = queue.pop().unwrap_or(0);
            self.accumulator -= self.output_sample_rate;
        }
        self.accumulator += INPUT_SAMPLE_RATE;
        self.current
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn fill_i16(
    output: &mut [i16],
    channels: usize,
    queue: &ArrayQueue<i16>,
    playback: &mut PlaybackState,
) {
    for frame in output.chunks_mut(channels) {
        let sample = playback.next(queue);
        frame.fill(sample);
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn fill_u16(
    output: &mut [u16],
    channels: usize,
    queue: &ArrayQueue<i16>,
    playback: &mut PlaybackState,
) {
    for frame in output.chunks_mut(channels) {
        let sample = (playback.next(queue) as i32 + 32_768) as u16;
        frame.fill(sample);
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn fill_f32(
    output: &mut [f32],
    channels: usize,
    queue: &ArrayQueue<i16>,
    playback: &mut PlaybackState,
) {
    for frame in output.chunks_mut(channels) {
        let sample = playback.next(queue) as f32 / 32_767.0;
        frame.fill(sample);
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn test_tone_samples() -> Vec<i16> {
    const DURATION_SAMPLES: usize = INPUT_SAMPLE_RATE as usize / 4;
    const FADE_SAMPLES: usize = INPUT_SAMPLE_RATE as usize / 100;
    (0..DURATION_SAMPLES)
        .map(|index| {
            let fade_in = (index as f32 / FADE_SAMPLES as f32).min(1.0);
            let fade_out = ((DURATION_SAMPLES - index) as f32 / FADE_SAMPLES as f32).min(1.0);
            let envelope = fade_in.min(fade_out);
            let phase = index as f32 * 440.0 * std::f32::consts::TAU / INPUT_SAMPLE_RATE as f32;
            (phase.sin() * envelope * i16::MAX as f32 * 0.12) as i16
        })
        .collect()
}


#[cfg(target_os = "windows")]
mod imp {
    use super::*;

    use anyhow::{Context as _, Result, bail};
    use cpal::{
        Device, SampleFormat, Stream, StreamConfig,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };

    pub struct AudioOutput {
        queue: AudioQueue,
        stream: Option<Stream>,
        selected_name: Option<String>,
        stream_error: Arc<parking_lot::Mutex<Option<String>>>,
    }

    impl AudioOutput {
        pub fn new(queue: AudioQueue) -> Self {
            Self {
                queue,
                stream: None,
                selected_name: None,
                stream_error: Arc::new(parking_lot::Mutex::new(None)),
            }
        }

        pub fn device_names() -> Result<Vec<String>> {
            let mut names: Vec<_> = cpal::default_host()
                .output_devices()
                .context("无法枚举音频输出设备")?
                .filter_map(|device| {
                    device
                        .description()
                        .ok()
                        .map(|description| description.name().to_owned())
                })
                .collect();
            names.sort();
            names.dedup();
            Ok(names)
        }

        pub fn select(&mut self, name: Option<&str>) -> Result<()> {
            self.queue.active.store(false, Ordering::Release);
            self.stream = None;
            self.selected_name = None;
            self.stream_error.lock().take();
            self.queue.clear();
            let Some(name) = name else {
                return Ok(());
            };
            let device = cpal::default_host()
                .output_devices()
                .context("无法枚举音频输出设备")?
                .find(|device| {
                    device
                        .description()
                        .ok()
                        .is_some_and(|description| description.name() == name)
                })
                .with_context(|| format!("找不到音频输出设备：{name}"))?;
            let stream = self.build_stream(&device)?;
            stream.play().context("无法启动音频输出流")?;
            self.stream = Some(stream);
            self.selected_name = Some(name.to_owned());
            self.queue.active.store(true, Ordering::Release);
            Ok(())
        }

        pub fn selected_name(&self) -> Option<&str> {
            self.selected_name.as_deref()
        }

        pub fn take_stream_error(&self) -> Option<String> {
            self.stream_error.lock().take()
        }

        pub fn test_tone(&self) -> Result<()> {
            if self.selected_name.is_none() || !self.queue.is_active() {
                bail!("请先选择并启用一个音频输出设备");
            }
            self.queue.clear();
            self.queue.push(&test_tone_samples());
            Ok(())
        }

        fn build_stream(&self, device: &Device) -> Result<Stream> {
            let supported = device
                .default_output_config()
                .context("无法读取音频设备默认格式")?;
            let channels = supported.channels();
            if channels == 0 {
                bail!("音频设备没有输出声道");
            }
            let config = StreamConfig {
                channels,
                sample_rate: supported.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            };
            let output_sample_rate = config.sample_rate;
            let queue = self.queue.samples.clone();
            let active = self.queue.active.clone();
            let stream_error = self.stream_error.clone();
            let error_callback = move |error: cpal::Error| {
                active.store(false, Ordering::Release);
                let message = error.to_string();
                *stream_error.lock() = Some(message);
            };
            let stream = match supported.sample_format() {
                SampleFormat::I16 => {
                    let mut playback = PlaybackState::new(output_sample_rate);
                    device.build_output_stream(
                        config,
                        move |output: &mut [i16], _| {
                            fill_i16(output, channels as usize, &queue, &mut playback)
                        },
                        error_callback,
                        None,
                    )?
                }
                SampleFormat::U16 => {
                    let mut playback = PlaybackState::new(output_sample_rate);
                    device.build_output_stream(
                        config,
                        move |output: &mut [u16], _| {
                            fill_u16(output, channels as usize, &queue, &mut playback)
                        },
                        error_callback,
                        None,
                    )?
                }
                SampleFormat::F32 => {
                    let mut playback = PlaybackState::new(output_sample_rate);
                    device.build_output_stream(
                        config,
                        move |output: &mut [f32], _| {
                            fill_f32(output, channels as usize, &queue, &mut playback)
                        },
                        error_callback,
                        None,
                    )?
                }
                format => bail!("暂不支持音频设备采样格式 {format:?}"),
            };
            Ok(stream)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::*;
    use anyhow::Result;

    /// Non-Windows stub: no audio backend exists here. The runtime and UI
    /// code paths stay identical; every operation reports the platform gap.
    pub struct AudioOutput {
        selected_name: Option<String>,
    }

    impl AudioOutput {
        pub fn new(_queue: AudioQueue) -> Self {
            Self { selected_name: None }
        }

        pub fn device_names() -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        pub fn select(&mut self, name: Option<&str>) -> Result<()> {
            self.selected_name = name.map(str::to_owned);
            Ok(())
        }

        pub fn selected_name(&self) -> Option<&str> {
            self.selected_name.as_deref()
        }

        pub fn take_stream_error(&self) -> Option<String> {
            None
        }

        pub fn test_tone(&self) -> Result<()> {
            anyhow::bail!("语音桥接仅支持 Windows")
        }
    }
}

pub use imp::AudioOutput;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_keeps_latest_samples() {
        let queue = AudioQueue {
            samples: Arc::new(ArrayQueue::new(3)),
            active: Arc::new(AtomicBool::new(false)),
        };
        queue.push(&[1, 2, 3, 4]);
        assert_eq!(queue.len(), 3);
        let mut output = [0; 3];
        fill_i16(
            &mut output,
            1,
            &queue.samples,
            &mut PlaybackState::new(INPUT_SAMPLE_RATE),
        );
        assert_eq!(output, [2, 3, 4]);
    }

    #[test]
    fn output_duplicates_mono_across_channels() {
        let queue = ArrayQueue::new(2);
        queue.push(1_000).unwrap();
        queue.push(-1_000).unwrap();
        let mut output = [0.0; 4];
        fill_f32(
            &mut output,
            2,
            &queue,
            &mut PlaybackState::new(INPUT_SAMPLE_RATE),
        );
        assert_eq!(output[0], output[1]);
        assert_eq!(output[2], output[3]);
        assert!(output[0] > 0.0 && output[2] < 0.0);
    }

    #[test]
    fn resamples_voice_for_standard_48_khz_endpoints() {
        let queue = ArrayQueue::new(2);
        queue.push(1_000).unwrap();
        queue.push(2_000).unwrap();
        let mut output = [0; 6];
        fill_i16(&mut output, 1, &queue, &mut PlaybackState::new(48_000));
        assert_eq!(output, [1_000, 1_000, 1_000, 2_000, 2_000, 2_000]);
    }

    #[test]
    fn test_tone_is_short_and_faded() {
        let samples = test_tone_samples();
        assert_eq!(samples.len(), 4_000);
        assert_eq!(samples[0], 0);
        assert!(samples.iter().any(|sample| sample.unsigned_abs() > 3_000));
        assert!(samples.iter().all(|sample| sample.unsigned_abs() < 4_000));
    }
}
