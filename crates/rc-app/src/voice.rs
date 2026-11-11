//! The real voice bridge service: `rc_voice::VoiceRuntime` behind the app's
//! [`VoiceService`] seam. Events from the runtime thread arrive as
//! [`UiNotice::Voice`] through the callback main.rs installs (same pattern
//! as the engine pump's notices).

use std::sync::Arc;

use rc_voice::{VoiceCommand, VoiceRuntime, VoiceStart};

use crate::bridge_api::{VoiceService, VoiceStatus};
use crate::settings::VoiceSettings;

pub struct RealVoice {
    runtime: Arc<VoiceRuntime>,
}

impl RealVoice {
    /// Start the runtime from persisted settings. `on_notice` runs on the
    /// voice runtime thread for every state change.
    pub fn start(
        settings: &VoiceSettings,
        on_notice: Box<dyn Fn(crate::bridge_api::UiNotice) + Send>,
    ) -> Arc<Self> {
        let initial = VoiceStart {
            output_device: settings.output_device.clone(),
            gain_db: settings.gain_db,
            enabled: settings.enabled,
        };
        let runtime = VoiceRuntime::start(
            initial,
            Box::new(move |event| {
                on_notice(crate::bridge_api::UiNotice::Voice(VoiceStatus::from_runtime(
                    event.0,
                )))
            }),
        );
        Arc::new(Self { runtime })
    }
}

impl VoiceService for RealVoice {
    fn set_enabled(&self, enabled: bool) {
        self.runtime.send(VoiceCommand::SetEnabled(enabled));
    }

    fn select_output(&self, name: Option<String>) {
        self.runtime.send(VoiceCommand::SelectDevice(name));
    }

    fn set_gain(&self, gain_db: f32) {
        self.runtime.send(VoiceCommand::SetGain(gain_db));
    }

    fn reconnect(&self) {
        self.runtime.send(VoiceCommand::Reconnect);
    }

    fn refresh_devices(&self) {
        self.runtime.send(VoiceCommand::RefreshDevices);
    }

    fn test_tone(&self) {
        self.runtime.send(VoiceCommand::TestTone);
    }

    fn snapshot(&self) -> VoiceStatus {
        VoiceStatus::from_runtime(self.runtime.snapshot())
    }
}
