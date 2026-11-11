//! rc-voice: the Xiaomi voice remote's ATVV audio bridge.
//!
//! Connects to the remote's proprietary ATVV GATT service (the channel its
//! microphone audio actually flows over — Windows never enumerates it as a
//! recording device), decodes the 16 kHz IMA/DVI ADPCM stream and writes
//! the PCM into a user-chosen virtual audio device's play endpoint, which
//! a voice application reads from the matching recording endpoint. The
//! session is driven entirely by the remote: hold its voice key and the
//! remote opens the mic and streams; release ends it.
//!
//! Ported from the hardware-verified `xiaomi-remote-mic` project
//! (2026-08-25, real `2717:32B8` device). The protocol layer, decoder,
//! audio queue/resampler and session state machine are pure and
//! host-tested; the WinRT BLE backend and the cpal output stream compile
//! only on Windows, mirroring how rc-agent gates its Frida hook.
//!
//! The HID side of the voice key is NOT handled here: remote-mic-rune
//! already owns it at the keyboard report layer (voice-key synthesis).

pub mod atvv;
pub mod audio;
pub mod ble;
mod runtime;

pub use audio::AudioQueue;
pub use ble::{BleStatus, StreamConfig};
pub use runtime::{VoiceCommand, VoiceEvent, VoiceRuntime, VoiceSnapshot, VoiceStart};
