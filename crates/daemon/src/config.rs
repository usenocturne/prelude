use std::{
    fs,
    path::{Path, PathBuf},
};

use platform_superbird::{
    AccessoryIdentity, AccessoryIdentitySource, AmbientLightConfig, IdentityError,
};
use serde::{Deserialize, Serialize};

use crate::{Error, PresetBindings, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionMode {
    #[default]
    Disabled,
    Automatic,
    ExperimentalPrivateProtocol,
    AppleLicensedPlugin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsbConfig {
    pub polling_interval_ms: u64,
    pub configuration: Option<u8>,
    pub configuration_is_private: bool,
    pub allow_configuration_changes: bool,
    pub allow_private_apple_configuration: bool,
    pub allow_private_lockdown_service: bool,
    pub enable_iphone_syslog_tap: bool,
    pub allow_first_time_pairing: bool,
    pub pairing_records_directory: PathBuf,
    pub pairing_dialog_timeout_ms: u64,
    pub pairing_retry_interval_ms: u64,
    pub configuration_reconnect_timeout_ms: u64,
    pub configuration_poll_interval_ms: u64,
    pub control_transfer_timeout_ms: u64,
    pub mux_handshake_timeout_ms: u64,
    pub mux_io_timeout_ms: u64,
    pub mux_connect_timeout_ms: u64,
    pub reconnect_initial_delay_ms: u64,
    pub reconnect_maximum_delay_ms: u64,
}

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            polling_interval_ms: 1_000,
            configuration: None,
            configuration_is_private: false,
            allow_configuration_changes: false,
            allow_private_apple_configuration: false,
            allow_private_lockdown_service: false,
            enable_iphone_syslog_tap: false,
            allow_first_time_pairing: false,
            pairing_records_directory: PathBuf::from("/var/lib/lockdown"),
            pairing_dialog_timeout_ms: 60_000,
            pairing_retry_interval_ms: 1_000,
            configuration_reconnect_timeout_ms: 5_000,
            configuration_poll_interval_ms: 200,
            control_transfer_timeout_ms: 3_000,
            mux_handshake_timeout_ms: 3_000,
            mux_io_timeout_ms: 250,
            mux_connect_timeout_ms: 5_000,
            reconnect_initial_delay_ms: 2_000,
            reconnect_maximum_delay_ms: 15_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectionConfig {
    pub mode: ProjectionMode,
    pub allow_experimental_private_protocol: bool,
    pub licensed_plugin_path: Option<String>,
    pub require_complete_capabilities: bool,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            mode: ProjectionMode::Disabled,
            allow_experimental_private_protocol: false,
            licensed_plugin_path: None,
            require_complete_capabilities: true,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AndroidAutoConfig {
    pub enable_live_session: bool,
    pub head_unit_name: String,
    pub ui_density_dpi: u32,
    pub android_vendor_ids: Vec<u16>,
    pub aoap_control_timeout_ms: u64,
    pub aoap_reenumeration_timeout_ms: u64,
    pub usb_transfer_timeout_ms: u64,
    pub session_io_timeout_ms: u64,
    pub ping_interval_ms: u64,
    pub ping_timeout_ms: u64,
    pub disable_audio_output: bool,
}

impl Default for AndroidAutoConfig {
    fn default() -> Self {
        Self {
            enable_live_session: false,
            head_unit_name: "Prelude".to_owned(),
            ui_density_dpi: 170,
            android_vendor_ids: Vec::new(),
            aoap_control_timeout_ms: 2_000,
            aoap_reenumeration_timeout_ms: 5_000,
            usb_transfer_timeout_ms: 1_000,
            session_io_timeout_ms: 250,
            ping_interval_ms: 1_500,
            ping_timeout_ms: 5_000,
            disable_audio_output: false,
        }
    }
}

impl std::fmt::Debug for AndroidAutoConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AndroidAutoConfig")
            .field("enable_live_session", &self.enable_live_session)
            .field(
                "head_unit_name_configured",
                &!self.head_unit_name.is_empty(),
            )
            .field("ui_density_dpi", &self.ui_density_dpi)
            .field(
                "android_vendor_ids_configured",
                &self.android_vendor_ids.len(),
            )
            .field("aoap_control_timeout_ms", &self.aoap_control_timeout_ms)
            .field(
                "aoap_reenumeration_timeout_ms",
                &self.aoap_reenumeration_timeout_ms,
            )
            .field("usb_transfer_timeout_ms", &self.usb_transfer_timeout_ms)
            .field("session_io_timeout_ms", &self.session_io_timeout_ms)
            .field("ping_interval_ms", &self.ping_interval_ms)
            .field("ping_timeout_ms", &self.ping_timeout_ms)
            .field("disable_audio_output", &self.disable_audio_output)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WirelessConfig {
    pub enabled: bool,
    pub android_auto: bool,
    pub carplay: bool,
    pub bluetooth_adapter: String,
    pub wifi_interface: Option<String>,
    pub ssid: Option<String>,
    pub passphrase: Option<String>,
    pub country: String,
    pub channel: u8,
    pub channel_width_mhz: u16,
    pub access_point_address: std::net::Ipv4Addr,
    pub android_auto_port: u16,
    pub discoverable: bool,
}

impl Default for WirelessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            android_auto: true,
            carplay: true,
            bluetooth_adapter: "hci0".to_owned(),
            wifi_interface: None,
            ssid: None,
            passphrase: None,
            country: "US".to_owned(),
            channel: 36,
            channel_width_mhz: 20,
            access_point_address: std::net::Ipv4Addr::new(10, 47, 0, 1),
            android_auto_port: prelude_wireless::wpp::DEFAULT_PROJECTION_PORT,
            discoverable: true,
        }
    }
}

impl std::fmt::Debug for WirelessConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WirelessConfig")
            .field("enabled", &self.enabled)
            .field("android_auto", &self.android_auto)
            .field("carplay", &self.carplay)
            .field("bluetooth_adapter", &self.bluetooth_adapter)
            .field("wifi_interface", &self.wifi_interface)
            .field("ssid", &self.ssid)
            .field("passphrase_configured", &self.passphrase.is_some())
            .field("country", &self.country)
            .field("channel", &self.channel)
            .field("channel_width_mhz", &self.channel_width_mhz)
            .field("access_point_address", &self.access_point_address)
            .field("android_auto_port", &self.android_auto_port)
            .field("discoverable", &self.discoverable)
            .finish()
    }
}

impl WirelessConfig {
    pub fn channel_width(&self) -> Option<prelude_wireless::access_point::ChannelWidth> {
        prelude_wireless::access_point::ChannelWidth::from_mhz(self.channel_width_mhz)
    }

    pub fn access_point(
        &self,
        interface: String,
        ssid: String,
        passphrase: String,
        run_dir: PathBuf,
    ) -> Result<prelude_wireless::access_point::AccessPointConfig> {
        let config = prelude_wireless::access_point::AccessPointConfig {
            interface,
            ssid,
            passphrase,
            country: self.country.clone(),
            channel: self.channel,
            width: self.channel_width().ok_or_else(|| {
                Error::Config("wireless.channel_width_mhz must be 20, 40, or 80".to_owned())
            })?,
            address: self.access_point_address,
            run_dir,
        };
        config
            .validate()
            .map_err(|error| Error::Config(format!("wireless: {error}")))?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !self.android_auto && !self.carplay {
            return Err(Error::Config(
                "wireless.enabled requires wireless.android_auto or wireless.carplay".to_owned(),
            ));
        }
        if self.bluetooth_adapter.is_empty()
            || !self
                .bluetooth_adapter
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(Error::Config(
                "wireless.bluetooth_adapter must be a BlueZ adapter name such as hci0".to_owned(),
            ));
        }
        if self.android_auto_port < 1024 {
            return Err(Error::Config(
                "wireless.android_auto_port must be an unprivileged port".to_owned(),
            ));
        }
        self.access_point(
            self.wifi_interface
                .clone()
                .unwrap_or_else(|| "wlan0".to_owned()),
            self.ssid.clone().unwrap_or_else(|| "Prelude".to_owned()),
            self.passphrase
                .clone()
                .unwrap_or_else(|| "placeholder-passphrase".to_owned()),
            PathBuf::from("/run/prelude/wireless"),
        )
        .map(drop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VideoBackend {
    #[default]
    None,
    AmlogicV4l2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioBackend {
    #[default]
    None,
    AlsaUsb,
    BuiltInSuperbird,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformConfig {
    pub ui_rotation_degrees: u16,
    pub diagnostic_level: DiagnosticLevel,
    pub ambient_light: AmbientLightConfig,
    #[serde(skip)]
    pub presets: PresetBindings,
}

impl std::fmt::Debug for PlatformConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlatformConfig")
            .field("ui_rotation_degrees", &self.ui_rotation_degrees)
            .field("diagnostic_level", &self.diagnostic_level)
            .field("ambient_light", &self.ambient_light)
            .field("presets", &self.presets.actions())
            .finish()
    }
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            ui_rotation_degrees: 270,
            diagnostic_level: DiagnosticLevel::Info,
            ambient_light: AmbientLightConfig::default(),
            presets: PresetBindings::default(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    #[serde(skip)]
    pub equalizer: prelude_media::EqualizerControl,
    pub video: VideoBackend,
    pub audio: AudioBackend,
    pub microphone: AudioBackend,
    pub usb_dac: Option<UsbDacConfig>,
    pub socket_poll_interval_ms: u64,
    pub timing_interval_ms: u64,
}

impl std::fmt::Debug for MediaConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaConfig")
            .field("video", &self.video)
            .field("audio", &self.audio)
            .field("microphone", &self.microphone)
            .field("usb_dac_configured", &self.usb_dac.is_some())
            .field("socket_poll_interval_ms", &self.socket_poll_interval_ms)
            .field("timing_interval_ms", &self.timing_interval_ms)
            .finish()
    }
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            equalizer: Default::default(),
            video: VideoBackend::None,
            audio: AudioBackend::None,
            microphone: AudioBackend::None,
            usb_dac: None,
            socket_poll_interval_ms: 20,
            timing_interval_ms: 1_000,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbDacConfig {
    pub vendor_id: Option<String>,
    pub product_id: Option<String>,
    pub serial: Option<String>,
    pub product: Option<String>,
}

impl std::fmt::Debug for UsbDacConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsbDacConfig")
            .field("vendor_id", &self.vendor_id)
            .field("product_id", &self.product_id)
            .field("serial", &self.serial.as_ref().map(|_| "<redacted>"))
            .field("product", &self.product.as_ref().map(|_| "<configured>"))
            .finish()
    }
}

impl UsbDacConfig {
    pub(crate) fn matcher(&self) -> prelude_media::Result<prelude_media::UsbDacMatcher> {
        match (&self.vendor_id, &self.product_id) {
            (None, None) if self.serial.is_none() && self.product.is_none() => {
                Ok(prelude_media::UsbDacMatcher::any())
            }
            (Some(vendor_id), Some(product_id)) => prelude_media::UsbDacMatcher::new(
                vendor_id.clone(),
                product_id.clone(),
                self.serial.clone(),
                self.product.clone(),
            ),
            _ => Err(prelude_media::Error::Backend(
                "USB DAC vendor_id and product_id must both be set for an exact match, and serial/product require them"
                    .to_owned(),
            )),
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Iap2Config {
    pub enable_live_session: bool,
    pub accessory_name: Option<String>,
    pub model_identifier: Option<String>,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub firmware_version: Option<String>,
    pub hardware_version: Option<String>,
}

impl std::fmt::Debug for Iap2Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Iap2Config")
            .field("enable_live_session", &self.enable_live_session)
            .field("accessory_name_configured", &self.accessory_name.is_some())
            .field(
                "model_identifier_configured",
                &self.model_identifier.is_some(),
            )
            .field("manufacturer_configured", &self.manufacturer.is_some())
            .field("serial_number_configured", &self.serial_number.is_some())
            .field(
                "firmware_version_configured",
                &self.firmware_version.is_some(),
            )
            .field(
                "hardware_version_configured",
                &self.hardware_version.is_some(),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CarPlayDock {
    #[default]
    Left,
    Right,
    Bottom,
}

impl CarPlayDock {
    pub const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Bottom];

    pub const fn right_hand_drive(self) -> bool {
        matches!(self, Self::Right)
    }

    pub const fn status_bar_edge(self) -> u64 {
        match self {
            Self::Left | Self::Right => 2,
            Self::Bottom => 1,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Bottom => "Bottom",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AirPlayConfig {
    pub enable_live_receiver: bool,
    pub enable_live_input: bool,
    pub disable_audio_output: bool,
    pub dock: CarPlayDock,
    pub setup_code_file: Option<PathBuf>,
    pub display_width_physical_mm: u16,
    pub display_height_physical_mm: u16,
    pub display_max_fps: u16,
    pub accept_poll_interval_ms: u64,
    pub control_io_timeout_ms: u64,
    pub input_poll_interval_ms: u64,
    pub pairing_timeout_ms: u64,
}

impl Default for AirPlayConfig {
    fn default() -> Self {
        Self {
            enable_live_receiver: false,
            enable_live_input: false,
            disable_audio_output: false,
            dock: CarPlayDock::Left,
            setup_code_file: None,
            display_width_physical_mm: 86,
            display_height_physical_mm: 52,
            display_max_fps: 30,
            accept_poll_interval_ms: 100,
            control_io_timeout_ms: 250,
            input_poll_interval_ms: 20,
            pairing_timeout_ms: 15_000,
        }
    }
}

impl std::fmt::Debug for AirPlayConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AirPlayConfig")
            .field("enable_live_receiver", &self.enable_live_receiver)
            .field("enable_live_input", &self.enable_live_input)
            .field("disable_audio_output", &self.disable_audio_output)
            .field("dock", &self.dock)
            .field(
                "setup_code_file_configured",
                &self.setup_code_file.is_some(),
            )
            .field("display_width_physical_mm", &self.display_width_physical_mm)
            .field(
                "display_height_physical_mm",
                &self.display_height_physical_mm,
            )
            .field("display_max_fps", &self.display_max_fps)
            .field("accept_poll_interval_ms", &self.accept_poll_interval_ms)
            .field("control_io_timeout_ms", &self.control_io_timeout_ms)
            .field("input_poll_interval_ms", &self.input_poll_interval_ms)
            .field("pairing_timeout_ms", &self.pairing_timeout_ms)
            .finish()
    }
}

impl Iap2Config {
    pub fn needs_platform_identity(&self) -> bool {
        self.accessory_name.is_none()
            || self.model_identifier.is_none()
            || self.manufacturer.is_none()
            || self.serial_number.is_none()
            || self.firmware_version.is_none()
            || self.hardware_version.is_none()
    }

    pub fn resolve_missing_identity(
        &mut self,
        source: &impl AccessoryIdentitySource,
    ) -> std::result::Result<(), IdentityError> {
        if !self.needs_platform_identity() {
            return Ok(());
        }
        self.merge_missing_identity(source.load()?);
        Ok(())
    }

    fn merge_missing_identity(&mut self, identity: AccessoryIdentity) {
        self.accessory_name.get_or_insert(identity.name);
        self.model_identifier
            .get_or_insert(identity.model_identifier);
        self.manufacturer.get_or_insert(identity.manufacturer);
        self.serial_number.get_or_insert(identity.serial_number);
        self.firmware_version
            .get_or_insert(identity.firmware_version);
        self.hardware_version
            .get_or_insert(identity.hardware_version);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub usb: UsbConfig,
    pub projection: ProjectionConfig,
    pub android_auto: AndroidAutoConfig,
    pub iap2: Iap2Config,
    pub airplay: AirPlayConfig,
    pub media: MediaConfig,
    pub platform: PlatformConfig,
    pub wireless: WirelessConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/prelude"),
            usb: UsbConfig::default(),
            projection: ProjectionConfig::default(),
            android_auto: AndroidAutoConfig::default(),
            iap2: Iap2Config::default(),
            airplay: AirPlayConfig::default(),
            media: MediaConfig::default(),
            platform: PlatformConfig::default(),
            wireless: WirelessConfig::default(),
        }
    }
}

impl DaemonConfig {
    pub fn from_toml(input: &str) -> Result<Self> {
        let config: Self = toml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input = fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&input)
    }

    pub fn validate(&self) -> Result<()> {
        let carplay_enabled = matches!(
            self.projection.mode,
            ProjectionMode::Automatic | ProjectionMode::ExperimentalPrivateProtocol
        );
        let android_auto_enabled = self.projection.mode == ProjectionMode::Automatic;
        if !(100..=60_000).contains(&self.usb.polling_interval_ms) {
            return Err(Error::Config(
                "usb.polling_interval_ms must be within 100..=60000".to_owned(),
            ));
        }
        self.wireless.validate()?;
        if !self.state_dir.is_absolute() {
            return Err(Error::Config(
                "state_dir must be an absolute path".to_owned(),
            ));
        }
        if !matches!(self.platform.ui_rotation_degrees, 0 | 90 | 180 | 270) {
            return Err(Error::Config(
                "platform.ui_rotation_degrees must be 0, 90, 180, or 270".to_owned(),
            ));
        }
        let ambient = &self.platform.ambient_light;
        if !(50..=5_000).contains(&ambient.poll_interval_ms) {
            return Err(Error::Config(
                "platform.ambient_light.poll_interval_ms must be within 50..=5000".to_owned(),
            ));
        }
        if !(3..=63).contains(&ambient.median_window) || ambient.median_window % 2 == 0 {
            return Err(Error::Config(
                "platform.ambient_light.median_window must be an odd value within 3..=63"
                    .to_owned(),
            ));
        }
        if ambient.raw_at_max_brightness <= ambient.dim_knee {
            return Err(Error::Config(
                "platform.ambient_light.raw_at_max_brightness must exceed dim_knee".to_owned(),
            ));
        }
        if ambient.minimum_brightness == 0 {
            return Err(Error::Config(
                "platform.ambient_light.minimum_brightness must be non-zero".to_owned(),
            ));
        }
        if ambient.night_mode_on_darkness_percent > 100
            || ambient.night_mode_off_darkness_percent > 100
            || ambient.night_mode_off_darkness_percent >= ambient.night_mode_on_darkness_percent
        {
            return Err(Error::Config(
                "platform.ambient_light night-mode off threshold must be below the on threshold"
                    .to_owned(),
            ));
        }
        for (name, value) in [
            ("night_mode_on_delay_ms", ambient.night_mode_on_delay_ms),
            ("night_mode_off_delay_ms", ambient.night_mode_off_delay_ms),
        ] {
            if !(1..=60_000).contains(&value) {
                return Err(Error::Config(format!(
                    "platform.ambient_light.{name} must be within 1..=60000"
                )));
            }
        }
        if ambient.integration_time_ms == 0 || ambient.integration_time_ms > 1_000 {
            return Err(Error::Config(
                "platform.ambient_light.integration_time_ms must be within 1..=1000".to_owned(),
            ));
        }
        if !matches!(ambient.gain, 1 | 8 | 16 | 120) {
            return Err(Error::Config(
                "platform.ambient_light.gain must be one of 1, 8, 16, or 120".to_owned(),
            ));
        }
        if (self.airplay.enable_live_receiver || self.android_auto.enable_live_session)
            && self.platform.ui_rotation_degrees != 270
        {
            return Err(Error::Config(
                "live Car Thing projection requires platform.ui_rotation_degrees = 270 to match the BSP display and input transforms"
                    .to_owned(),
            ));
        }
        if !(1..=60).contains(&self.airplay.display_max_fps) {
            return Err(Error::Config(
                "airplay.display_max_fps must be within 1..=60".to_owned(),
            ));
        }
        if self.usb.configuration_is_private && self.usb.configuration.is_none() {
            return Err(Error::Config(
                "usb.configuration_is_private requires usb.configuration".to_owned(),
            ));
        }
        for (name, value) in [
            (
                "configuration_reconnect_timeout_ms",
                self.usb.configuration_reconnect_timeout_ms,
            ),
            (
                "configuration_poll_interval_ms",
                self.usb.configuration_poll_interval_ms,
            ),
            (
                "control_transfer_timeout_ms",
                self.usb.control_transfer_timeout_ms,
            ),
            (
                "pairing_dialog_timeout_ms",
                self.usb.pairing_dialog_timeout_ms,
            ),
            (
                "pairing_retry_interval_ms",
                self.usb.pairing_retry_interval_ms,
            ),
            (
                "mux_handshake_timeout_ms",
                self.usb.mux_handshake_timeout_ms,
            ),
            ("mux_io_timeout_ms", self.usb.mux_io_timeout_ms),
            ("mux_connect_timeout_ms", self.usb.mux_connect_timeout_ms),
            (
                "reconnect_initial_delay_ms",
                self.usb.reconnect_initial_delay_ms,
            ),
            (
                "reconnect_maximum_delay_ms",
                self.usb.reconnect_maximum_delay_ms,
            ),
        ] {
            if value == 0 {
                return Err(Error::Config(format!("usb.{name} must be non-zero")));
            }
        }
        if self.usb.configuration_poll_interval_ms > self.usb.configuration_reconnect_timeout_ms {
            return Err(Error::Config(
                "USB configuration poll interval exceeds reconnect timeout".to_owned(),
            ));
        }
        if self.usb.reconnect_initial_delay_ms > self.usb.reconnect_maximum_delay_ms {
            return Err(Error::Config(
                "USB reconnect initial delay exceeds maximum delay".to_owned(),
            ));
        }
        if self.usb.pairing_retry_interval_ms > self.usb.pairing_dialog_timeout_ms {
            return Err(Error::Config(
                "USB pairing retry interval exceeds dialog timeout".to_owned(),
            ));
        }
        if !self.usb.pairing_records_directory.is_absolute() {
            return Err(Error::Config(
                "usb.pairing_records_directory must be an absolute path".to_owned(),
            ));
        }
        for (name, value) in [
            ("accessory_name", self.iap2.accessory_name.as_deref()),
            ("model_identifier", self.iap2.model_identifier.as_deref()),
            ("manufacturer", self.iap2.manufacturer.as_deref()),
            ("serial_number", self.iap2.serial_number.as_deref()),
            ("firmware_version", self.iap2.firmware_version.as_deref()),
            ("hardware_version", self.iap2.hardware_version.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty() || value.as_bytes().contains(&0)) {
                return Err(Error::Config(format!(
                    "iap2.{name} cannot be empty or contain NUL"
                )));
            }
        }
        if self.iap2.enable_live_session && !carplay_enabled {
            return Err(Error::Config(
                "iap2.enable_live_session requires automatic or experimental_private_protocol mode"
                    .to_owned(),
            ));
        }
        if self.android_auto.enable_live_session && !android_auto_enabled {
            return Err(Error::Config(
                "android_auto.enable_live_session requires automatic projection mode".to_owned(),
            ));
        }
        if android_auto_enabled && !self.android_auto.enable_live_session {
            return Err(Error::Config(
                "automatic projection mode requires android_auto.enable_live_session".to_owned(),
            ));
        }
        if android_auto_enabled
            && (!self.iap2.enable_live_session || !self.airplay.enable_live_receiver)
        {
            return Err(Error::Config(
                "automatic projection mode requires live CarPlay iAP2/AirPlay and Android Auto sessions"
                    .to_owned(),
            ));
        }
        if self.android_auto.head_unit_name.trim().is_empty()
            || self.android_auto.head_unit_name.len()
                > prelude_android_auto_protocol::MAX_HEAD_UNIT_NAME_BYTES
            || self
                .android_auto
                .head_unit_name
                .chars()
                .any(char::is_control)
        {
            return Err(Error::Config(
                "android_auto.head_unit_name is empty, too long, or contains control data"
                    .to_owned(),
            ));
        }
        if !(prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI
            ..=prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI)
            .contains(&self.android_auto.ui_density_dpi)
        {
            return Err(Error::Config(format!(
                "android_auto.ui_density_dpi must be within {}..={}",
                prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI,
                prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI
            )));
        }
        if self.android_auto.android_vendor_ids.len() > 32
            || self.android_auto.android_vendor_ids.contains(&0)
        {
            return Err(Error::Config(
                "android_auto.android_vendor_ids must contain at most 32 nonzero USB vendor IDs"
                    .to_owned(),
            ));
        }
        for (name, value) in [
            (
                "aoap_control_timeout_ms",
                self.android_auto.aoap_control_timeout_ms,
            ),
            (
                "aoap_reenumeration_timeout_ms",
                self.android_auto.aoap_reenumeration_timeout_ms,
            ),
            (
                "usb_transfer_timeout_ms",
                self.android_auto.usb_transfer_timeout_ms,
            ),
            (
                "session_io_timeout_ms",
                self.android_auto.session_io_timeout_ms,
            ),
            ("ping_interval_ms", self.android_auto.ping_interval_ms),
            ("ping_timeout_ms", self.android_auto.ping_timeout_ms),
        ] {
            if value == 0 || value > 60_000 {
                return Err(Error::Config(format!(
                    "android_auto.{name} must be within 1..=60000"
                )));
            }
        }
        if self.android_auto.ping_interval_ms >= self.android_auto.ping_timeout_ms {
            return Err(Error::Config(
                "Android Auto ping interval must be shorter than its timeout".to_owned(),
            ));
        }
        if self.android_auto.session_io_timeout_ms > self.android_auto.ping_interval_ms
            || self.android_auto.usb_transfer_timeout_ms > self.android_auto.ping_interval_ms
        {
            return Err(Error::Config(
                "Android Auto USB/session I/O timeouts must not exceed its ping interval"
                    .to_owned(),
            ));
        }
        for (name, value) in [
            (
                "accept_poll_interval_ms",
                self.airplay.accept_poll_interval_ms,
            ),
            ("control_io_timeout_ms", self.airplay.control_io_timeout_ms),
            (
                "input_poll_interval_ms",
                self.airplay.input_poll_interval_ms,
            ),
            ("pairing_timeout_ms", self.airplay.pairing_timeout_ms),
        ] {
            if value == 0 || value > 60_000 {
                return Err(Error::Config(format!(
                    "airplay.{name} must be within 1..=60000"
                )));
            }
        }
        for (name, path) in [("setup_code_file", self.airplay.setup_code_file.as_deref())] {
            if path.is_some_and(|path| !path.is_absolute()) {
                return Err(Error::Config(format!(
                    "airplay.{name} must be an absolute path"
                )));
            }
        }
        if self.airplay.enable_live_receiver {
            if !carplay_enabled || !self.iap2.enable_live_session {
                return Err(Error::Config(
                    "airplay.enable_live_receiver requires live iAP2 in automatic or experimental projection mode"
                        .to_owned(),
                ));
            }
            if self.airplay.setup_code_file.is_none() {
                return Err(Error::Config(
                    "live AirPlay requires a caller-provisioned setup_code_file".to_owned(),
                ));
            }
        }
        if self.airplay.enable_live_input && !self.airplay.enable_live_receiver {
            return Err(Error::Config(
                "airplay.enable_live_input requires the live AirPlay receiver".to_owned(),
            ));
        }
        if self.airplay.enable_live_receiver
            && self.projection.require_complete_capabilities
            && !self.airplay.enable_live_input
        {
            return Err(Error::Config(
                "projection.require_complete_capabilities requires airplay.enable_live_input for the live receiver"
                    .to_owned(),
            ));
        }
        for (name, value) in [
            (
                "socket_poll_interval_ms",
                self.media.socket_poll_interval_ms,
            ),
            ("timing_interval_ms", self.media.timing_interval_ms),
        ] {
            if value == 0 || value > 60_000 {
                return Err(Error::Config(format!(
                    "media.{name} must be within 1..=60000"
                )));
            }
        }
        if self.airplay.enable_live_receiver || self.android_auto.enable_live_session {
            if !cfg!(feature = "linux-media") {
                return Err(Error::Config(
                    "live media requires the Linux production media feature".to_owned(),
                ));
            }
            if self.media.video != VideoBackend::AmlogicV4l2 {
                return Err(Error::Config(
                    "live media requires media.video = amlogic_v4l2".to_owned(),
                ));
            }
            if self.media.audio != AudioBackend::AlsaUsb {
                return Err(Error::Config(
                    "live media requires media.audio = alsa_usb".to_owned(),
                ));
            }
            if self.media.microphone != AudioBackend::BuiltInSuperbird {
                return Err(Error::Config(
                    "live media requires media.microphone = built_in_superbird".to_owned(),
                ));
            }
            let usb_dac = self.media.usb_dac.as_ref().ok_or_else(|| {
                Error::Config("live USB audio requires media.usb_dac discovery".to_owned())
            })?;
            usb_dac.matcher().map_err(|error| {
                Error::Config(format!("invalid media.usb_dac matcher: {error}"))
            })?;
        }
        for (name, dimension) in [
            (
                "display_width_physical_mm",
                Some(self.airplay.display_width_physical_mm),
            ),
            (
                "display_height_physical_mm",
                Some(self.airplay.display_height_physical_mm),
            ),
        ] {
            if dimension.is_some_and(|dimension| !(1..=2_000).contains(&dimension)) {
                return Err(Error::Config(format!(
                    "airplay.{name} must be within 1..=2000"
                )));
            }
        }
        if self.usb.allow_private_apple_configuration && !self.usb.allow_configuration_changes {
            return Err(Error::Config(
                "private USB configuration requires configuration changes to be enabled".to_owned(),
            ));
        }
        if self.usb.allow_first_time_pairing && !carplay_enabled {
            return Err(Error::Config(
                "first-time iPhone pairing is allowed only in automatic or experimental projection mode"
                    .to_owned(),
            ));
        }
        match self.projection.mode {
            ProjectionMode::Disabled => {
                if self.projection.allow_experimental_private_protocol {
                    return Err(Error::Config(
                        "experimental protocol cannot be enabled while projection is disabled"
                            .to_owned(),
                    ));
                }
            }
            ProjectionMode::Automatic | ProjectionMode::ExperimentalPrivateProtocol => {
                if !self.projection.allow_experimental_private_protocol {
                    return Err(Error::Config(
                        "CarPlay-capable projection mode requires its explicit experimental protocol opt-in"
                            .to_owned(),
                    ));
                }
                if !self.usb.allow_private_apple_configuration
                    || !self.usb.allow_private_lockdown_service
                    || !self.usb.configuration_is_private
                {
                    return Err(Error::Config(
                        "experimental projection requires explicit private USB and Lockdown opt-ins"
                            .to_owned(),
                    ));
                }
                if self.usb.configuration != Some(6) {
                    return Err(Error::Config(
                        "experimental wired carrier requires usb.configuration = 6".to_owned(),
                    ));
                }
            }
            ProjectionMode::AppleLicensedPlugin => {
                if self
                    .projection
                    .licensed_plugin_path
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    return Err(Error::Config(
                        "licensed projection mode requires licensed_plugin_path".to_owned(),
                    ));
                }
                if self.projection.allow_experimental_private_protocol {
                    return Err(Error::Config(
                        "licensed and experimental projection modes cannot be combined".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedIdentitySource;

    impl AccessoryIdentitySource for FixedIdentitySource {
        fn load(&self) -> std::result::Result<AccessoryIdentity, IdentityError> {
            Ok(AccessoryIdentity {
                name: "Prelude".to_owned(),
                model_identifier: "Prelude".to_owned(),
                manufacturer: "Vanta Labs".to_owned(),
                serial_number: "platform-serial".to_owned(),
                firmware_version: "0.1.0".to_owned(),
                hardware_version: "Prelude".to_owned(),
            })
        }
    }

    #[test]
    fn empty_config_is_disabled_and_valid() {
        let config = DaemonConfig::from_toml("").expect("config");
        assert_eq!(config.projection.mode, ProjectionMode::Disabled);
        assert!(!config.usb.allow_configuration_changes);
        assert_eq!(config.state_dir, Path::new("/var/lib/prelude"));
        assert_eq!(config.platform.ui_rotation_degrees, 270);
    }

    #[test]
    fn packaged_image_configs_match_the_daemon_schema() {
        for input in [
            include_str!("../../../image/meta-prelude/recipes-core/prelude/files/config.toml"),
            include_str!("../../../image/meta-prelude/recipes-core/prelude/files/config-dev.toml"),
        ] {
            DaemonConfig::from_toml(input).expect("packaged daemon configuration");
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(DaemonConfig::from_toml("surprise = true").is_err());
    }

    #[test]
    fn partial_experimental_opt_in_is_rejected() {
        let input = r#"
            [projection]
            mode = "experimental_private_protocol"
            allow_experimental_private_protocol = true
        "#;
        assert!(matches!(
            DaemonConfig::from_toml(input),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn complete_experimental_opt_in_is_accepted() {
        let input = r#"
            [usb]
            configuration = 6
            configuration_is_private = true
            allow_configuration_changes = true
            allow_private_apple_configuration = true
            allow_private_lockdown_service = true
            allow_first_time_pairing = true

            [projection]
            mode = "experimental_private_protocol"
            allow_experimental_private_protocol = true
        "#;
        assert!(DaemonConfig::from_toml(input).is_ok());
    }

    #[test]
    fn automatic_mode_requires_both_explicit_carplay_and_android_auto_opt_ins() {
        let input = r#"
            [usb]
            configuration = 6
            configuration_is_private = true
            allow_configuration_changes = true
            allow_private_apple_configuration = true
            allow_private_lockdown_service = true

            [projection]
            mode = "automatic"
            allow_experimental_private_protocol = true

            [android_auto]
            enable_live_session = true

            [iap2]
            enable_live_session = true

            [airplay]
            enable_live_receiver = true
            enable_live_input = true
            setup_code_file = "/var/lib/prelude/airplay/setup-code"

            [media]
            video = "amlogic_v4l2"
            audio = "alsa_usb"
            microphone = "built_in_superbird"

            [media.usb_dac]
        "#;
        assert!(DaemonConfig::from_toml(input).is_ok());
        assert!(
            DaemonConfig::from_toml(
                &input.replace("enable_live_session = true", "enable_live_session = false")
            )
            .is_err()
        );
        assert!(
            DaemonConfig::from_toml(&input.replace(
                "allow_experimental_private_protocol = true",
                "allow_experimental_private_protocol = false"
            ))
            .is_err()
        );
    }

    #[test]
    fn android_auto_config_rejects_invalid_identity_and_timeouts() {
        assert!(DaemonConfig::from_toml("[android_auto]\nhead_unit_name = ' '").is_err());
        assert!(
            DaemonConfig::from_toml(&format!(
                "[android_auto]\nhead_unit_name = '{}'",
                "x".repeat(prelude_android_auto_protocol::MAX_HEAD_UNIT_NAME_BYTES + 1)
            ))
            .is_err()
        );
        assert!(
            DaemonConfig::from_toml(
                "[android_auto]\nping_interval_ms = 5000\nping_timeout_ms = 5000"
            )
            .is_err()
        );
        assert!(DaemonConfig::from_toml("[android_auto]\nui_density_dpi = 99").is_err());
        assert!(DaemonConfig::from_toml("[android_auto]\nui_density_dpi = 481").is_err());
        assert!(DaemonConfig::from_toml("[android_auto]\nusb_transfer_timeout_ms = 0").is_err());
        assert!(matches!(
            DaemonConfig::from_toml("[android_auto]\naoap_control_timeout_ms = 0"),
            Err(Error::Config(message)) if message.contains("android_auto.aoap_control_timeout_ms")
        ));
    }

    #[test]
    fn live_iap2_requires_experimental_mode_but_allows_diagnostic_gaps() {
        assert!(DaemonConfig::from_toml("[iap2]\nenable_live_session = true").is_err());
        let input = r#"
            [usb]
            configuration = 6
            configuration_is_private = true
            allow_configuration_changes = true
            allow_private_apple_configuration = true
            allow_private_lockdown_service = true

            [projection]
            mode = "experimental_private_protocol"
            allow_experimental_private_protocol = true

            [iap2]
            enable_live_session = true
        "#;
        assert!(DaemonConfig::from_toml(input).is_ok());
    }

    #[test]
    fn iap2_rejects_static_receiver_and_network_values() {
        assert!(DaemonConfig::from_toml("[iap2]\ncarplay_service_port = 7000").is_err());
        assert!(DaemonConfig::from_toml("[iap2]\nreceiver_device_identifier = 'static'").is_err());
        assert!(DaemonConfig::from_toml("[iap2]\nncm_addresses = ['fe80::1']").is_err());
        assert!(DaemonConfig::from_toml("[iap2]\naccessory_name = ' '").is_err());
    }

    #[test]
    fn iap2_debug_redacts_identifiers_and_key_material() {
        let config = Iap2Config {
            serial_number: Some("sensitive-serial".into()),
            ..Iap2Config::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sensitive-serial"));
    }

    #[test]
    fn live_airplay_requires_files_and_live_experimental_iap2() {
        assert!(DaemonConfig::from_toml("[airplay]\nenable_live_receiver = true").is_err());
        let input = r#"
            [usb]
            configuration = 6
            configuration_is_private = true
            allow_configuration_changes = true
            allow_private_apple_configuration = true
            allow_private_lockdown_service = true

            [projection]
            mode = "experimental_private_protocol"
            allow_experimental_private_protocol = true

            [iap2]
            enable_live_session = true

            [airplay]
            enable_live_receiver = true
            enable_live_input = true
            setup_code_file = "/var/lib/prelude/airplay/setup-code"
            display_width_physical_mm = 100
            display_height_physical_mm = 60

            [media]
            video = "amlogic_v4l2"
            audio = "alsa_usb"
            microphone = "built_in_superbird"

            [media.usb_dac]
            vendor_id = "1234"
            product_id = "5678"
        "#;
        assert!(DaemonConfig::from_toml(input).is_ok());
        let automatic_dac = input
            .replace("            vendor_id = \"1234\"\n", "")
            .replace("            product_id = \"5678\"\n", "");
        let config = DaemonConfig::from_toml(&automatic_dac).expect("automatic USB DAC config");
        let usb_dac = config.media.usb_dac.expect("USB DAC discovery");
        assert_eq!(usb_dac.vendor_id, None);
        assert_eq!(usb_dac.product_id, None);
        assert!(matches!(
            DaemonConfig::from_toml(
                &input.replace("            product_id = \"5678\"\n", "")
            ),
            Err(Error::Config(message)) if message.contains("vendor_id and product_id")
        ));
        assert!(
            DaemonConfig::from_toml(
                &input.replace("video = \"amlogic_v4l2\"", "video = \"software\"")
            )
            .is_err()
        );
    }

    #[test]
    fn live_airplay_input_requires_receiver() {
        assert!(DaemonConfig::from_toml("[airplay]\nenable_live_input = true").is_err());
        let input = r#"
            [usb]
            configuration = 6
            configuration_is_private = true
            allow_configuration_changes = true
            allow_private_apple_configuration = true
            allow_private_lockdown_service = true

            [projection]
            mode = "experimental_private_protocol"
            allow_experimental_private_protocol = true

            [iap2]
            enable_live_session = true

            [airplay]
            enable_live_receiver = true
            enable_live_input = true
            setup_code_file = "/var/lib/prelude/airplay/setup-code"

            [media]
            video = "amlogic_v4l2"
            audio = "alsa_usb"
            microphone = "built_in_superbird"

            [media.usb_dac]
            vendor_id = "1234"
            product_id = "5678"
        "#;
        let mut config = DaemonConfig::from_toml(input).expect("live AirPlay config");
        config.platform.ui_rotation_degrees = 90;
        assert!(matches!(
            config.validate(),
            Err(Error::Config(message)) if message.contains("requires platform.ui_rotation_degrees = 270")
        ));
        config.platform.ui_rotation_degrees = 270;
        let usb_dac = config.media.usb_dac.take();
        assert!(matches!(
            config.validate(),
            Err(Error::Config(message)) if message.contains("media.usb_dac")
        ));
        config.media.usb_dac = usb_dac;
        config.media.microphone = AudioBackend::None;
        assert!(matches!(
            config.validate(),
            Err(Error::Config(message)) if message.contains("built_in_superbird")
        ));
    }

    #[test]
    fn airplay_rejects_relative_files_and_unbounded_timeouts() {
        assert!(DaemonConfig::from_toml("[airplay]\nsetup_code_file = 'relative'").is_err());
        assert!(DaemonConfig::from_toml("[airplay]\ncontrol_io_timeout_ms = 60001").is_err());
    }

    #[test]
    fn media_debug_redacts_usb_dac_serial() {
        let config = MediaConfig {
            usb_dac: Some(UsbDacConfig {
                vendor_id: Some("1234".to_owned()),
                product_id: Some("5678".to_owned()),
                serial: Some("sensitive-dac-serial".to_owned()),
                product: Some("USB DAC".to_owned()),
            }),
            ..MediaConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sensitive-dac-serial"));
        assert!(!debug.contains("USB DAC"));
    }

    #[test]
    fn platform_identity_fills_only_missing_iap2_fields() {
        let mut config = Iap2Config {
            accessory_name: Some("Explicit Name".to_owned()),
            serial_number: Some("explicit-serial".to_owned()),
            ..Iap2Config::default()
        };
        config
            .resolve_missing_identity(&FixedIdentitySource)
            .expect("identity");
        assert_eq!(config.accessory_name.as_deref(), Some("Explicit Name"));
        assert_eq!(config.serial_number.as_deref(), Some("explicit-serial"));
        assert_eq!(config.model_identifier.as_deref(), Some("Prelude"));
        assert_eq!(config.manufacturer.as_deref(), Some("Vanta Labs"));
        assert_eq!(config.firmware_version.as_deref(), Some("0.1.0"));
        assert_eq!(config.hardware_version.as_deref(), Some("Prelude"));
    }

    #[test]
    fn first_time_pairing_is_rejected_outside_experimental_mode() {
        let input = r#"
            [usb]
            allow_first_time_pairing = true
        "#;
        assert!(matches!(
            DaemonConfig::from_toml(input),
            Err(Error::Config(message)) if message.contains("only in automatic or experimental")
        ));
    }

    #[test]
    fn invalid_platform_settings_are_rejected() {
        assert!(DaemonConfig::from_toml("[platform]\nui_rotation_degrees = 45").is_err());
    }

    #[test]
    fn invalid_ambient_light_settings_are_rejected() {
        assert!(DaemonConfig::from_toml("[platform.ambient_light]\nmedian_window = 10").is_err());
        assert!(DaemonConfig::from_toml("[platform.ambient_light]\ngain = 2").is_err());
        assert!(DaemonConfig::from_toml(
            "[platform.ambient_light]\nnight_mode_on_darkness_percent = 40\nnight_mode_off_darkness_percent = 40"
        )
        .is_err());
    }
}
