//! ATVV protocol layer: the Xiaomi voice remote's GATT audio service.
//!
//! Ported unchanged from the hardware-verified implementation in
//! `xiaomi-remote-mic` (2026-08-25, real `2717:32B8` device): service and
//! characteristic UUIDs, wire commands, capability parsing, the IMA/DVI
//! ADPCM decoder, byte-stream frame reassembly and the PCM postprocess
//! (smoothing + gain). Pure so `cargo test` covers it on any host.

use std::collections::VecDeque;

pub const SERVICE_UUID: uuid::Uuid = uuid::uuid!("ab5e0001-5a21-4f05-bc7d-af01f617b664");
pub const TRANSMIT_UUID: uuid::Uuid = uuid::uuid!("ab5e0002-5a21-4f05-bc7d-af01f617b664");
pub const AUDIO_UUID: uuid::Uuid = uuid::uuid!("ab5e0003-5a21-4f05-bc7d-af01f617b664");
pub const CONTROL_UUID: uuid::Uuid = uuid::uuid!("ab5e0004-5a21-4f05-bc7d-af01f617b664");
pub const BATTERY_LEVEL_UUID: uuid::Uuid = uuid::uuid!("00002a19-0000-1000-8000-00805f9b34fb");
pub const MODEL_NUMBER_UUID: uuid::Uuid = uuid::uuid!("00002a24-0000-1000-8000-00805f9b34fb");

pub const GET_CAPABILITIES_V1: &[u8] = &[0x0a, 0x01, 0x00, 0x00, 0x03, 0x03];

pub fn microphone_open(version: u16, codec: u8) -> Vec<u8> {
    if version >= 0x0100 {
        vec![0x0c, 0x00]
    } else {
        vec![0x0c, 0x00, codec]
    }
}

pub fn microphone_close(version: u16, session_id: u8) -> Vec<u8> {
    if version >= 0x0100 {
        vec![0x0d, session_id]
    } else {
        vec![0x0d]
    }
}

pub fn microphone_extend(version: u16, session_id: u8) -> Option<Vec<u8>> {
    (version >= 0x0100).then(|| vec![0x0e, session_id])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub version: u16,
    pub codecs: u8,
    pub interaction: u8,
    pub frame_size: usize,
    pub selected_codec: u8,
    pub sample_rate: u32,
}

impl Capabilities {
    pub const DEFAULT: Self = Self {
        version: 0x0100,
        codecs: 0x02,
        interaction: 0x03,
        frame_size: 120,
        selected_codec: 0x02,
        sample_rate: 16_000,
    };

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 7 || bytes[0] != 0x0b {
            return None;
        }

        let version = u16::from_be_bytes([bytes[1], bytes[2]]);
        let (mut codecs, mut interaction) = if version >= 0x0100 {
            (bytes[3], bytes[4])
        } else {
            if bytes.len() < 9 {
                return None;
            }
            (bytes[4], 0)
        };
        if version >= 0x0100 && codecs == 0 && bytes.len() >= 9 && bytes[4] & 0x03 != 0 {
            codecs = bytes[4];
            interaction = 0x03;
        }
        let frame_size = u16::from_be_bytes([bytes[5], bytes[6]]) as usize;
        let selected_codec = if codecs & 0x02 != 0 { 0x02 } else { 0x01 };

        Some(Self {
            version,
            codecs,
            interaction,
            frame_size: if frame_size == 0 { 120 } else { frame_size },
            selected_codec,
            sample_rate: if selected_codec == 0x02 {
                16_000
            } else {
                8_000
            },
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ImaAdpcmDecoder {
    predictor: i32,
    step_index: i32,
    low_nibble_first: bool,
}

impl ImaAdpcmDecoder {
    const STEP_TABLE: [i32; 89] = [
        7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60,
        66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371,
        408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878,
        2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845,
        8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086,
        29794, 32767,
    ];
    const INDEX_TABLE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

    pub fn set_low_nibble_first(&mut self, enabled: bool) {
        self.low_nibble_first = enabled;
    }

    pub fn reset(&mut self, predictor: i32, step_index: i32) {
        self.predictor = predictor.clamp(i16::MIN as i32, i16::MAX as i32);
        self.step_index = step_index.clamp(0, 88);
    }

    pub fn decode(&mut self, bytes: &[u8]) -> Vec<i16> {
        let mut samples = Vec::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let nibbles = if self.low_nibble_first {
                [byte & 0x0f, byte >> 4]
            } else {
                [byte >> 4, byte & 0x0f]
            };
            samples.extend(nibbles.map(|nibble| self.decode_nibble(nibble)));
        }
        samples
    }

    fn decode_nibble(&mut self, nibble: u8) -> i16 {
        let step = Self::STEP_TABLE[self.step_index as usize];
        let mut difference = step >> 3;
        if nibble & 1 != 0 {
            difference += step >> 2;
        }
        if nibble & 2 != 0 {
            difference += step >> 1;
        }
        if nibble & 4 != 0 {
            difference += step;
        }
        if nibble & 8 != 0 {
            self.predictor -= difference;
        } else {
            self.predictor += difference;
        }
        self.predictor = self.predictor.clamp(i16::MIN as i32, i16::MAX as i32);
        self.step_index = (self.step_index + Self::INDEX_TABLE[(nibble & 7) as usize]).clamp(0, 88);
        self.predictor as i16
    }
}

#[derive(Debug, Default)]
pub struct FrameAccumulator {
    pending: VecDeque<u8>,
}

impl FrameAccumulator {
    pub fn append(&mut self, bytes: &[u8], frame_size: usize) -> Vec<Vec<u8>> {
        if frame_size == 0 {
            return Vec::new();
        }
        self.pending.extend(bytes);
        let mut frames = Vec::new();
        while self.pending.len() >= frame_size {
            frames.push(self.pending.drain(..frame_size).collect());
        }
        frames
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

pub fn process_pcm(input: &[i16], gain_db: f32) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut filtered: Vec<i32> = input.iter().map(|sample| *sample as i32).collect();
    for index in 1..input.len().saturating_sub(1) {
        filtered[index] =
            (input[index - 1] as i32 + 2 * input[index] as i32 + input[index + 1] as i32) >> 2;
    }
    let safe_gain = if gain_db.is_finite() { gain_db } else { 0.0 }.clamp(-24.0, 24.0);
    let gain = 10_f32.powf(safe_gain / 20.0);
    filtered
        .into_iter()
        .map(|sample| {
            ((sample as f32 * gain).round() as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_parse_v1_and_compatibility_layout() {
        let caps = Capabilities::parse(&[0x0b, 0x01, 0x00, 0x02, 0x03, 0x00, 0x78]).unwrap();
        assert_eq!(caps, Capabilities::DEFAULT);

        let compat =
            Capabilities::parse(&[0x0b, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0, 0]).unwrap();
        assert_eq!(compat.selected_codec, 0x02);
        assert_eq!(compat.interaction, 0x03);
        assert_eq!(compat.frame_size, 120);
    }

    #[test]
    fn capabilities_reject_malformed_and_identify_8khz() {
        assert!(Capabilities::parse(&[0x0b, 1]).is_none());
        assert!(Capabilities::parse(&[0x01, 0, 0, 0, 0, 0, 0]).is_none());
        assert_eq!(
            Capabilities::parse(&[0x0b, 0x01, 0x00, 0x01, 0x03, 0, 120])
                .unwrap()
                .sample_rate,
            8_000
        );
    }

    #[test]
    fn commands_match_atvv_wire_format() {
        assert_eq!(microphone_open(0x0100, 2), [0x0c, 0]);
        assert_eq!(microphone_open(1, 2), [0x0c, 0, 2]);
        assert_eq!(microphone_close(0x0100, 7), [0x0d, 7]);
        assert_eq!(microphone_close(1, 7), [0x0d]);
        assert_eq!(microphone_extend(0x0100, 7).unwrap(), [0x0e, 7]);
        assert_eq!(microphone_extend(1, 7), None);
    }

    #[test]
    fn decoder_supports_both_nibble_orders_and_sync_clamping() {
        let mut high = ImaAdpcmDecoder::default();
        let high_samples = high.decode(&[0x12, 0x34]);
        let mut low = ImaAdpcmDecoder::default();
        low.set_low_nibble_first(true);
        let low_samples = low.decode(&[0x12, 0x34]);
        assert_ne!(high_samples, low_samples);
        assert_eq!(high_samples.len(), 4);

        high.reset(99_999, 255);
        assert_eq!(high.decode(&[0])[0], i16::MAX);
    }

    #[test]
    fn frame_accumulator_preserves_fragments_and_coalesced_frames() {
        let mut accumulator = FrameAccumulator::default();
        assert!(accumulator.append(&[1, 2], 3).is_empty());
        assert_eq!(accumulator.pending_len(), 2);
        assert_eq!(
            accumulator.append(&[3, 4, 5, 6, 7], 3),
            vec![vec![1, 2, 3], vec![4, 5, 6]]
        );
        assert_eq!(accumulator.pending_len(), 1);
        accumulator.reset();
        assert_eq!(accumulator.pending_len(), 0);
    }

    #[test]
    fn postprocessor_smooths_and_clamps_gain() {
        assert_eq!(process_pcm(&[0, 100, 0], 0.0), [0, 50, 0]);
        assert_eq!(process_pcm(&[i16::MAX], 100.0), [i16::MAX]);
        assert_eq!(process_pcm(&[-100], f32::NAN), [-100]);
    }
}
