#![forbid(unsafe_code)]

mod airplay_data_channel;
mod airplay_runtime;
mod android_auto_latency;
mod android_auto_runtime;
mod audio_status;
mod automatic_runtime;
mod config;
mod daemon;
mod device_discovery;
mod diagnostics;
mod display_state;
mod error;
mod first_video;
#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
mod fonts;
mod h264_sps;
mod hands_free;
mod health;
mod heartbeat;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod host_ui;
mod iap2_runtime;
pub mod internet;
mod known_phones;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod media_runtime;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod pairing_code;
mod presets;
pub mod readiness;
mod saved_networks;
mod settings;
#[cfg(target_os = "linux")]
mod shared_mfi;
#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
mod status_screen;
mod updater;
mod usb_monitor;
mod wired_runtime;
mod wireless_carplay;
pub mod wireless_identity;
mod wireless_runtime;

pub use android_auto_runtime::AndroidAutoRuntime;
pub use audio_status::{
    AUDIO_RUNTIME_STATUS_PATH, AUDIO_RUNTIME_STATUS_VERSION, AudioBackendStatus,
    AudioBaseSinkStatus, AudioFailureKind, AudioLatencyStatus, AudioOutputDelaySourceStatus,
    AudioPipelineState, AudioRuntimeRoute, AudioRuntimeSnapshot, AudioStatusError,
    AudioStatusWriter, AudioStreamLatencyStatus, MicrophoneRuntimeStatus,
    VideoSynchronizationStatus, load_audio_runtime_status,
};
pub use automatic_runtime::AutomaticProjectionRuntime;
pub use config::{
    AirPlayConfig, AndroidAutoConfig, AudioBackend, CarPlayDock, DaemonConfig, DiagnosticLevel,
    Iap2Config, MediaConfig, PlatformConfig, ProjectionConfig, ProjectionMode, UsbConfig,
    UsbDacConfig, VideoBackend, WirelessConfig,
};
pub use daemon::{Daemon, ProjectionRuntime, RuntimeProbe, UnsupportedProjectionRuntime};
pub use device_discovery::{CombinedDeviceDiscovery, ProjectionDevice, ProjectionDeviceDiscovery};
pub use diagnostics::{DiagnosticEvent, DiagnosticSnapshot, Diagnostics};
pub use display_state::{
    ConnectionStep, DisplayPlatform, DisplayStatus, load_last_display_platform,
    persist_last_display_platform,
};
pub use error::{Error, Result};
pub use health::{ComponentHealth, HealthRegistry, HealthState, OverallHealth};
pub use heartbeat::HeartbeatWriter;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use host_ui::{HostUi, HostUiEvent, SettingsCommitted};
pub use known_phones::{KnownPhone, MAXIMUM_KNOWN_PHONES};
pub use presets::{PRESET_COUNT, PresetAction, PresetBindings};
pub use settings::{RuntimeSettings, SettingsError};

pub use wired_runtime::{
    DEFAULT_MAX_WORKER_STOP_WAIT, MAX_IAP2_BLOCKING_STAGE, SelectedRuntime, WiredCarkitRuntime,
    configured_worker_stop_bound,
};
pub use wireless_carplay::WirelessCarPlayRuntime;
pub use wireless_runtime::{
    AccessPointInfo, WirelessAndroidAutoPhone, WirelessCarPlayPhone, WirelessLease, WirelessPhone,
    WirelessPhoneDirectory, WirelessPhones, WirelessService,
};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub fn preflight_configured_audio_output(config: &DaemonConfig) -> Result<()> {
    config.validate()?;
    media_runtime::preflight_configured_audio_output(
        &config.media,
        &config.platform,
        !config.airplay.disable_audio_output,
    )
    .map_err(Error::Runtime)
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub fn prepare_status_screens() {
    let _ = fonts::shared();
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub fn render_status_screen(
    platform: DisplayPlatform,
    status: DisplayStatus,
) -> std::result::Result<Vec<u8>, String> {
    status_screen::render(platform, status)
}
