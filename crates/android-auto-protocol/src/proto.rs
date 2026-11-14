use prost::Message;

use crate::{
    Error, Result,
    constants::{
        AV_SETUP_OK, BLUETOOTH_PAIRING_NUMERIC_COMPARISON, MAX_PROTO_PAYLOAD,
        MEDIA_CODEC_H264_BASELINE, MEDIA_CODEC_PCM, STATUS_OK, VIDEO_FPS_30, VIDEO_FPS_60,
        VIDEO_RESOLUTION_480P, channel,
    },
};

#[derive(Clone, PartialEq, Message)]
pub(crate) struct AuthComplete {
    #[prost(int32, required, tag = "1")]
    pub(crate) status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PhoneInfo {
    #[prost(string, optional, tag = "1")]
    pub(crate) instance_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub(crate) connectivity_lifetime_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ServiceDiscoveryRequest {
    #[prost(string, optional, tag = "4")]
    pub(crate) device_name: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub(crate) device_brand: Option<String>,
    #[prost(message, optional, tag = "6")]
    pub(crate) phone_info: Option<PhoneInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Sensor {
    #[prost(int32, required, tag = "1")]
    pub(crate) sensor_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SensorSourceService {
    #[prost(message, repeated, tag = "1")]
    pub(crate) sensors: Vec<Sensor>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) location_characterization: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Insets {
    #[prost(uint32, optional, tag = "1")]
    pub(crate) top: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) bottom: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub(crate) left: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub(crate) right: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct UiConfig {
    #[prost(message, optional, tag = "1")]
    pub(crate) margins: Option<Insets>,
    #[prost(message, optional, tag = "2")]
    pub(crate) content_insets: Option<Insets>,
    #[prost(message, optional, tag = "3")]
    pub(crate) stable_content_insets: Option<Insets>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct VideoConfiguration {
    #[prost(int32, optional, tag = "1")]
    pub(crate) codec_resolution: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub(crate) frame_rate: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub(crate) width_margin: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub(crate) height_margin: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub(crate) density: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub(crate) pixel_aspect_ratio_e4: Option<u32>,
    #[prost(int32, optional, tag = "10")]
    pub(crate) video_codec_type: Option<i32>,
    #[prost(message, optional, tag = "11")]
    pub(crate) ui_config: Option<UiConfig>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct AudioConfiguration {
    #[prost(uint32, required, tag = "1")]
    pub(crate) sampling_rate: u32,
    #[prost(uint32, required, tag = "2")]
    pub(crate) number_of_bits: u32,
    #[prost(uint32, required, tag = "3")]
    pub(crate) number_of_channels: u32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct MediaSinkService {
    #[prost(int32, optional, tag = "1")]
    pub(crate) available_type: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub(crate) audio_type: Option<i32>,
    #[prost(message, repeated, tag = "3")]
    pub(crate) audio_configs: Vec<AudioConfiguration>,
    #[prost(message, repeated, tag = "4")]
    pub(crate) video_configs: Vec<VideoConfiguration>,
    #[prost(bool, optional, tag = "5")]
    pub(crate) available_while_in_call: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct MediaSourceService {
    #[prost(int32, optional, tag = "1")]
    pub(crate) available_type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub(crate) audio_config: Option<AudioConfiguration>,
    #[prost(bool, optional, tag = "3")]
    pub(crate) available_while_in_call: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Touchscreen {
    #[prost(int32, required, tag = "1")]
    pub(crate) width: i32,
    #[prost(int32, required, tag = "2")]
    pub(crate) height: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct InputSourceService {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub(crate) keycodes_supported: Vec<i32>,
    #[prost(message, repeated, tag = "2")]
    pub(crate) touchscreen: Vec<Touchscreen>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BluetoothService {
    #[prost(string, required, tag = "1")]
    pub(crate) car_address: String,
    #[prost(int32, repeated, packed = "true", tag = "2")]
    pub(crate) supported_pairing_methods: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BluetoothPairingRequest {
    #[prost(string, optional, tag = "1")]
    pub(crate) phone_address: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub(crate) pairing_method: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BluetoothPairingResponse {
    #[prost(int32, required, tag = "1")]
    pub(crate) status: i32,
    #[prost(bool, required, tag = "2")]
    pub(crate) already_paired: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BluetoothAuthenticationData {
    #[prost(string, required, tag = "1")]
    pub(crate) auth_data: String,
    #[prost(int32, optional, tag = "2")]
    pub(crate) pairing_method: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BluetoothAuthenticationResult {
    #[prost(int32, optional, tag = "1")]
    pub(crate) status: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Service {
    #[prost(int32, required, tag = "1")]
    pub(crate) id: i32,
    #[prost(message, optional, tag = "2")]
    pub(crate) sensor_source_service: Option<SensorSourceService>,
    #[prost(message, optional, tag = "3")]
    pub(crate) media_sink_service: Option<MediaSinkService>,
    #[prost(message, optional, tag = "4")]
    pub(crate) input_source_service: Option<InputSourceService>,
    #[prost(message, optional, tag = "5")]
    pub(crate) media_source_service: Option<MediaSourceService>,
    #[prost(message, optional, tag = "6")]
    pub(crate) bluetooth_service: Option<BluetoothService>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PingConfiguration {
    #[prost(uint32, optional, tag = "1")]
    pub(crate) timeout_ms: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) interval_ms: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub(crate) high_latency_threshold_ms: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub(crate) tracked_ping_count: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ConnectionConfiguration {
    #[prost(message, optional, tag = "1")]
    pub(crate) ping_configuration: Option<PingConfiguration>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct HeadUnitInfo {
    #[prost(string, optional, tag = "1")]
    pub(crate) make: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub(crate) model: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub(crate) year: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub(crate) head_unit_make: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub(crate) head_unit_model: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub(crate) head_unit_software_build: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub(crate) head_unit_software_version: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ServiceDiscoveryResponse {
    #[prost(message, repeated, tag = "1")]
    pub(crate) channels: Vec<Service>,
    #[prost(int32, optional, tag = "6")]
    pub(crate) driver_position: Option<i32>,
    #[prost(string, optional, tag = "14")]
    pub(crate) display_name: Option<String>,
    #[prost(bool, optional, tag = "15")]
    pub(crate) probe_for_support: Option<bool>,
    #[prost(message, optional, tag = "16")]
    pub(crate) connection_configuration: Option<ConnectionConfiguration>,
    #[prost(message, optional, tag = "17")]
    pub(crate) headunit_info: Option<HeadUnitInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ChannelOpenRequest {
    #[prost(sint32, optional, tag = "1")]
    pub(crate) priority: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub(crate) service_id: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct StatusResponse {
    #[prost(int32, required, tag = "1")]
    pub(crate) status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Ping {
    #[prost(int64, optional, tag = "1")]
    pub(crate) timestamp: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Setup {
    #[prost(int32, optional, tag = "1")]
    pub(crate) media_codec_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SensorRequest {
    #[prost(int32, optional, tag = "1")]
    pub(crate) sensor_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct AudioFocusRequest {
    #[prost(int32, optional, tag = "1")]
    pub(crate) audio_focus_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct VoiceSessionNotification {
    #[prost(int32, optional, tag = "1")]
    pub(crate) status: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BatteryStatusNotification {
    #[prost(uint32, optional, tag = "1")]
    pub(crate) battery_level: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) time_remaining_seconds: Option<u32>,
    #[prost(bool, optional, tag = "3")]
    pub(crate) critical: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct VideoFocusNotification {
    #[prost(int32, optional, tag = "1")]
    pub(crate) focus: Option<i32>,
    #[prost(bool, optional, tag = "2")]
    pub(crate) unsolicited: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SetupResponse {
    #[prost(int32, required, tag = "1")]
    pub(crate) media_status: i32,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) max_unacked: Option<u32>,
    #[prost(uint32, repeated, packed = "true", tag = "3")]
    pub(crate) configs: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Start {
    #[prost(int32, optional, tag = "1")]
    pub(crate) session_id: Option<i32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) configuration_index: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Ack {
    #[prost(int32, optional, tag = "1")]
    pub(crate) session_id: Option<i32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) ack: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct MicrophoneRequest {
    #[prost(bool, optional, tag = "1")]
    pub(crate) open: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub(crate) anc_enabled: Option<bool>,
    #[prost(bool, optional, tag = "3")]
    pub(crate) echo_cancellation_enabled: Option<bool>,
    #[prost(int32, optional, tag = "4")]
    pub(crate) max_unacked: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct MicrophoneResponse {
    #[prost(int32, required, tag = "1")]
    pub(crate) status: i32,
    #[prost(int32, optional, tag = "2")]
    pub(crate) session_id: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ShutdownRequest {
    #[prost(int32, optional, tag = "1")]
    pub(crate) reason: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TouchPointMessage {
    #[prost(uint32, required, tag = "1")]
    pub(crate) x: u32,
    #[prost(uint32, required, tag = "2")]
    pub(crate) y: u32,
    #[prost(uint32, required, tag = "3")]
    pub(crate) pointer_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    pub(crate) pointer_data: Vec<TouchPointMessage>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) action_index: Option<u32>,
    #[prost(int32, optional, tag = "3")]
    pub(crate) action: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Key {
    #[prost(uint32, required, tag = "1")]
    pub(crate) keycode: u32,
    #[prost(bool, required, tag = "2")]
    pub(crate) down: bool,
    #[prost(uint32, required, tag = "3")]
    pub(crate) metastate: u32,
    #[prost(bool, optional, tag = "4")]
    pub(crate) longpress: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KeyEvent {
    #[prost(message, repeated, tag = "1")]
    pub(crate) keys: Vec<Key>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RelativeEntry {
    #[prost(uint32, required, tag = "1")]
    pub(crate) keycode: u32,
    #[prost(int32, required, tag = "2")]
    pub(crate) delta: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RelativeEvent {
    #[prost(message, repeated, tag = "1")]
    pub(crate) data: Vec<RelativeEntry>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct InputReport {
    #[prost(uint64, required, tag = "1")]
    pub(crate) timestamp: u64,
    #[prost(message, optional, tag = "3")]
    pub(crate) touch_event: Option<TouchEvent>,
    #[prost(message, optional, tag = "4")]
    pub(crate) key_event: Option<KeyEvent>,
    #[prost(message, optional, tag = "6")]
    pub(crate) relative_event: Option<RelativeEvent>,
}

fn empty_insets() -> Insets {
    Insets {
        top: Some(0),
        bottom: Some(0),
        left: Some(0),
        right: Some(0),
    }
}

fn audio_configuration(rate: u32, channels: u32) -> AudioConfiguration {
    AudioConfiguration {
        sampling_rate: rate,
        number_of_bits: 16,
        number_of_channels: channels,
    }
}

fn audio_sink(id: u8, audio_type: i32, rate: u32, channels: u32) -> Service {
    Service {
        id: i32::from(id),
        sensor_source_service: None,
        media_sink_service: Some(MediaSinkService {
            available_type: Some(MEDIA_CODEC_PCM),
            audio_type: Some(audio_type),
            audio_configs: vec![audio_configuration(rate, channels)],
            video_configs: Vec::new(),
            available_while_in_call: Some(true),
        }),
        input_source_service: None,
        media_source_service: None,
        bluetooth_service: None,
    }
}

pub const MAX_HEAD_UNIT_NAME_BYTES: usize = 64;

pub(crate) fn validate_head_unit_name(head_unit_name: &str) -> Result<()> {
    if head_unit_name.trim().is_empty()
        || head_unit_name.len() > MAX_HEAD_UNIT_NAME_BYTES
        || head_unit_name.chars().any(char::is_control)
    {
        return Err(Error::InvalidMessage("head-unit name is invalid"));
    }
    Ok(())
}

pub const DEFAULT_VIDEO_DENSITY_DPI: u32 = 170;
pub const MIN_VIDEO_DENSITY_DPI: u32 = 100;
pub const MAX_VIDEO_DENSITY_DPI: u32 = 480;

pub(crate) fn validate_bluetooth_address(address: &str) -> Result<()> {
    let octets = address.split(':').collect::<Vec<_>>();
    if octets.len() != 6
        || octets
            .iter()
            .any(|octet| octet.len() != 2 || !octet.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(Error::InvalidMessage("Bluetooth address is malformed"));
    }
    Ok(())
}

fn validate_video_density(density_dpi: u32) -> Result<()> {
    if !(MIN_VIDEO_DENSITY_DPI..=MAX_VIDEO_DENSITY_DPI).contains(&density_dpi) {
        return Err(Error::InvalidMessage(
            "video density is outside the supported range",
        ));
    }
    Ok(())
}

fn service_discovery_message(
    head_unit_name: &str,
    density_dpi: u32,
    audio_output: bool,
    bluetooth_address: Option<&str>,
) -> ServiceDiscoveryResponse {
    let video = Service {
        id: i32::from(channel::VIDEO),
        sensor_source_service: None,
        media_sink_service: Some(MediaSinkService {
            available_type: Some(MEDIA_CODEC_H264_BASELINE),
            audio_type: None,
            audio_configs: Vec::new(),
            video_configs: vec![
                VideoConfiguration {
                    codec_resolution: Some(VIDEO_RESOLUTION_480P),
                    frame_rate: Some(VIDEO_FPS_60),
                    width_margin: Some(0),
                    height_margin: Some(0),
                    density: Some(density_dpi),
                    pixel_aspect_ratio_e4: Some(10_000),
                    video_codec_type: Some(MEDIA_CODEC_H264_BASELINE),
                    ui_config: Some(UiConfig {
                        margins: Some(empty_insets()),
                        content_insets: Some(empty_insets()),
                        stable_content_insets: Some(empty_insets()),
                    }),
                },
                VideoConfiguration {
                    codec_resolution: Some(VIDEO_RESOLUTION_480P),
                    frame_rate: Some(VIDEO_FPS_30),
                    width_margin: Some(0),
                    height_margin: Some(0),
                    density: Some(density_dpi),
                    pixel_aspect_ratio_e4: Some(10_000),
                    video_codec_type: Some(MEDIA_CODEC_H264_BASELINE),
                    ui_config: Some(UiConfig {
                        margins: Some(empty_insets()),
                        content_insets: Some(empty_insets()),
                        stable_content_insets: Some(empty_insets()),
                    }),
                },
            ],
            available_while_in_call: Some(true),
        }),
        input_source_service: None,
        media_source_service: None,
        bluetooth_service: None,
    };
    let sensor = Service {
        id: i32::from(channel::SENSOR),
        sensor_source_service: Some(SensorSourceService {
            sensors: vec![Sensor { sensor_type: 13 }, Sensor { sensor_type: 10 }],
            location_characterization: None,
        }),
        media_sink_service: None,
        input_source_service: None,
        media_source_service: None,
        bluetooth_service: None,
    };
    let input = Service {
        id: i32::from(channel::INPUT),
        sensor_source_service: None,
        media_sink_service: None,
        input_source_service: Some(InputSourceService {
            keycodes_supported: vec![
                3, 4, 5, 6, 19, 20, 21, 22, 23, 66, 79, 82, 84, 85, 86, 87, 88, 89, 90, 91, 111,
                126, 127, 164, 219, 231, 260, 261, 262, 263, 65_536,
            ],
            touchscreen: vec![Touchscreen {
                width: 800,
                height: 480,
            }],
        }),
        media_source_service: None,
        bluetooth_service: None,
    };
    let microphone = Service {
        id: i32::from(channel::MICROPHONE),
        sensor_source_service: None,
        media_sink_service: None,
        input_source_service: None,
        media_source_service: Some(MediaSourceService {
            available_type: Some(MEDIA_CODEC_PCM),
            audio_config: Some(audio_configuration(16_000, 1)),
            available_while_in_call: Some(true),
        }),
        bluetooth_service: None,
    };

    let mut channels = if audio_output {
        vec![
            sensor,
            video,
            audio_sink(channel::MEDIA_AUDIO, 3, 48_000, 2),
            audio_sink(channel::GUIDANCE_AUDIO, 1, 16_000, 1),
            audio_sink(channel::SYSTEM_AUDIO, 2, 16_000, 1),
            input,
            microphone,
        ]
    } else {
        vec![
            sensor,
            video,
            audio_sink(channel::SYSTEM_AUDIO, 2, 16_000, 1),
            input,
            microphone,
        ]
    };
    if let Some(car_address) = bluetooth_address {
        channels.push(Service {
            id: i32::from(channel::BLUETOOTH),
            sensor_source_service: None,
            media_sink_service: None,
            input_source_service: None,
            media_source_service: None,
            bluetooth_service: Some(BluetoothService {
                car_address: car_address.to_owned(),
                supported_pairing_methods: vec![BLUETOOTH_PAIRING_NUMERIC_COMPARISON],
            }),
        });
    }

    ServiceDiscoveryResponse {
        channels,
        driver_position: Some(0),
        display_name: Some(head_unit_name.to_owned()),
        probe_for_support: Some(false),
        connection_configuration: Some(ConnectionConfiguration {
            ping_configuration: Some(PingConfiguration {
                timeout_ms: Some(5_000),
                interval_ms: Some(1_500),
                high_latency_threshold_ms: Some(500),
                tracked_ping_count: Some(5),
            }),
        }),
        headunit_info: Some(HeadUnitInfo {
            make: Some(head_unit_name.to_owned()),
            model: Some(head_unit_name.to_owned()),
            year: None,
            head_unit_make: Some("Vanta Labs".to_owned()),
            head_unit_model: Some(head_unit_name.to_owned()),
            head_unit_software_build: Some("1".to_owned()),
            head_unit_software_version: Some("0.1".to_owned()),
        }),
    }
}

pub fn service_discovery_payload(head_unit_name: &str) -> Result<Vec<u8>> {
    service_discovery_payload_with_density(head_unit_name, DEFAULT_VIDEO_DENSITY_DPI, true, None)
}

pub fn service_discovery_payload_with_density(
    head_unit_name: &str,
    density_dpi: u32,
    audio_output: bool,
    bluetooth_address: Option<&str>,
) -> Result<Vec<u8>> {
    validate_head_unit_name(head_unit_name)?;
    validate_video_density(density_dpi)?;
    if let Some(address) = bluetooth_address {
        validate_bluetooth_address(address)?;
    }
    Ok(
        service_discovery_message(head_unit_name, density_dpi, audio_output, bluetooth_address)
            .encode_to_vec(),
    )
}

pub(crate) fn decode<M: Message + Default>(context: &'static str, payload: &[u8]) -> Result<M> {
    if payload.len() > MAX_PROTO_PAYLOAD {
        return Err(Error::Oversized {
            kind: "protobuf message",
            actual: payload.len(),
            limit: MAX_PROTO_PAYLOAD,
        });
    }
    M::decode(payload).map_err(|source| Error::Protobuf { context, source })
}

pub(crate) fn auth_complete() -> Vec<u8> {
    AuthComplete { status: STATUS_OK }.encode_to_vec()
}

pub(crate) fn status_ok() -> Vec<u8> {
    StatusResponse { status: STATUS_OK }.encode_to_vec()
}

pub(crate) fn bluetooth_pairing_response(already_paired: bool) -> Vec<u8> {
    BluetoothPairingResponse {
        status: STATUS_OK,
        already_paired,
    }
    .encode_to_vec()
}

pub(crate) fn bluetooth_authentication(passkey: u32) -> Vec<u8> {
    BluetoothAuthenticationData {
        auth_data: format!("{passkey:06}"),
        pairing_method: Some(BLUETOOTH_PAIRING_NUMERIC_COMPARISON),
    }
    .encode_to_vec()
}

pub(crate) fn setup_ok() -> Vec<u8> {
    setup_ok_with_window(1)
}

pub(crate) fn setup_ok_with_window(max_unacked: u32) -> Vec<u8> {
    SetupResponse {
        media_status: AV_SETUP_OK,
        max_unacked: Some(max_unacked),
        configs: vec![0],
    }
    .encode_to_vec()
}

pub(crate) fn ack(session_id: i32) -> Vec<u8> {
    Ack {
        session_id: Some(session_id),
        ack: Some(1),
    }
    .encode_to_vec()
}

pub(crate) fn input_touch(
    timestamp_micros: u64,
    action: i32,
    action_index: u32,
    pointers: impl IntoIterator<Item = TouchPointMessage>,
) -> Vec<u8> {
    InputReport {
        timestamp: timestamp_micros,
        touch_event: Some(TouchEvent {
            pointer_data: pointers.into_iter().collect(),
            action_index: Some(action_index),
            action: Some(action),
        }),
        key_event: None,
        relative_event: None,
    }
    .encode_to_vec()
}

pub(crate) fn input_key(
    timestamp_micros: u64,
    keycode: u32,
    down: bool,
    longpress: bool,
) -> Vec<u8> {
    InputReport {
        timestamp: timestamp_micros,
        touch_event: None,
        key_event: Some(KeyEvent {
            keys: vec![Key {
                keycode,
                down,
                metastate: 0,
                longpress: Some(longpress),
            }],
        }),
        relative_event: None,
    }
    .encode_to_vec()
}

pub(crate) fn input_rotary(timestamp_micros: u64, delta: i32) -> Vec<u8> {
    InputReport {
        timestamp: timestamp_micros,
        touch_event: None,
        key_event: None,
        relative_event: Some(RelativeEvent {
            data: vec![RelativeEntry {
                keycode: 65_536,
                delta,
            }],
        }),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn service_discovery_is_fixed_to_the_supported_minimal_profile() {
        let payload = service_discovery_payload("Test Head Unit").expect("SDR payload");
        let response = ServiceDiscoveryResponse::decode(payload.as_slice()).expect("SDR");
        let ids = response
            .channels
            .iter()
            .map(|service| u8::try_from(service.id).expect("channel ID"))
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, channel::ADVERTISED.into_iter().collect());
        for excluded in [10, 12, 13, 14, 18, 19, 20] {
            assert!(!ids.contains(&excluded));
        }

        let video = response
            .channels
            .iter()
            .find(|service| service.id == i32::from(channel::VIDEO))
            .and_then(|service| service.media_sink_service.as_ref())
            .expect("video");
        assert_eq!(video.video_configs.len(), 2);
        let config_60 = &video.video_configs[0];
        assert_eq!(config_60.codec_resolution, Some(VIDEO_RESOLUTION_480P));
        assert_eq!(config_60.frame_rate, Some(VIDEO_FPS_60));
        assert_eq!(config_60.density, Some(DEFAULT_VIDEO_DENSITY_DPI));
        assert_eq!(config_60.pixel_aspect_ratio_e4, Some(10_000));
        assert_eq!(config_60.video_codec_type, Some(MEDIA_CODEC_H264_BASELINE));
        let config_30 = &video.video_configs[1];
        assert_eq!(config_30.codec_resolution, Some(VIDEO_RESOLUTION_480P));
        assert_eq!(config_30.frame_rate, Some(VIDEO_FPS_30));
        assert_eq!(config_30.density, Some(DEFAULT_VIDEO_DENSITY_DPI));
        assert_eq!(config_30.pixel_aspect_ratio_e4, Some(10_000));
        assert_eq!(config_30.video_codec_type, Some(MEDIA_CODEC_H264_BASELINE));

        let media = response
            .channels
            .iter()
            .find(|service| service.id == i32::from(channel::MEDIA_AUDIO))
            .and_then(|service| service.media_sink_service.as_ref())
            .expect("media audio");
        assert_eq!(media.audio_configs[0].sampling_rate, 48_000);
        assert_eq!(media.audio_configs[0].number_of_channels, 2);
        assert_eq!(response.display_name.as_deref(), Some("Test Head Unit"));
        assert_eq!(
            response
                .headunit_info
                .as_ref()
                .and_then(|info| info.make.as_deref()),
            Some("Test Head Unit")
        );
        assert_eq!(
            response
                .headunit_info
                .as_ref()
                .and_then(|info| info.head_unit_model.as_deref()),
            Some("Test Head Unit")
        );
    }

    #[test]
    fn livi_setup_response_golden_requires_status_two_and_one_ack() {
        assert_eq!(setup_ok(), [0x08, 0x02, 0x10, 0x01, 0x1a, 0x01, 0x00]);
    }

    #[test]
    fn livi_touch_and_key_goldens_include_required_zero_fields() {
        let touch = input_touch(
            1,
            0,
            0,
            [TouchPointMessage {
                x: 1,
                y: 2,
                pointer_id: 0,
            }],
        );
        assert_eq!(
            touch,
            [
                0x08, 0x01, 0x1a, 0x0c, 0x0a, 0x06, 0x08, 0x01, 0x10, 0x02, 0x18, 0x00, 0x10, 0x00,
                0x18, 0x00,
            ]
        );

        let key = input_key(1, 85, true, false);
        assert_eq!(
            key,
            [
                0x08, 0x01, 0x22, 0x0a, 0x0a, 0x08, 0x08, 0x55, 0x10, 0x01, 0x18, 0x00, 0x20, 0x00,
            ]
        );
    }

    #[test]
    fn negative_rotary_delta_uses_signed_int32_wire_encoding() {
        let bytes = input_rotary(7, -1);
        let decoded = InputReport::decode(bytes.as_slice()).expect("input report");
        let entry = &decoded.relative_event.expect("relative").data[0];
        assert_eq!(entry.keycode, 65_536);
        assert_eq!(entry.delta, -1);
    }

    #[test]
    fn required_field_presence_remains_observable_after_decode() {
        let start = decode::<Start>("start", &[]).expect("syntactic protobuf");
        assert_eq!(start.session_id, None);
        assert_eq!(start.configuration_index, None);
        let open = decode::<ChannelOpenRequest>("open", &[0x08, 0x00]).expect("syntactic protobuf");
        assert_eq!(open.priority, Some(0));
        assert_eq!(open.service_id, None);
    }

    #[test]
    fn head_unit_name_is_bounded_and_free_of_control_data() {
        assert!(service_discovery_payload("").is_err());
        assert!(service_discovery_payload("   ").is_err());
        assert!(service_discovery_payload("bad\0name").is_err());
        assert!(service_discovery_payload("bad\nname").is_err());
        assert!(service_discovery_payload(&"x".repeat(MAX_HEAD_UNIT_NAME_BYTES + 1)).is_err());
        assert!(service_discovery_payload(&"x".repeat(MAX_HEAD_UNIT_NAME_BYTES)).is_ok());
    }

    #[test]
    fn service_discovery_advertises_the_requested_video_density() {
        let payload = service_discovery_payload_with_density("Test Head Unit", 200, true, None)
            .expect("SDR payload");
        let response = ServiceDiscoveryResponse::decode(payload.as_slice()).expect("SDR");
        let density = response
            .channels
            .iter()
            .find(|service| service.id == i32::from(channel::VIDEO))
            .and_then(|service| service.media_sink_service.as_ref())
            .and_then(|media| media.video_configs.first())
            .and_then(|config| config.density);
        assert_eq!(density, Some(200));
    }

    #[test]
    fn phone_audio_omits_media_and_guidance_sinks() {
        let payload = service_discovery_payload_with_density(
            "Test Head Unit",
            DEFAULT_VIDEO_DENSITY_DPI,
            false,
            None,
        )
        .expect("SDR payload");
        let response = ServiceDiscoveryResponse::decode(payload.as_slice()).expect("SDR");
        let ids = response
            .channels
            .iter()
            .map(|service| u8::try_from(service.id).expect("channel ID"))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            [
                channel::SENSOR,
                channel::VIDEO,
                channel::SYSTEM_AUDIO,
                channel::INPUT,
                channel::MICROPHONE,
            ]
            .into_iter()
            .collect()
        );
        for id in channel::PHONE_AUDIO {
            assert!(!channel::advertised(id, false, false));
            assert!(channel::advertised(id, true, false));
        }
        assert!(channel::advertised(channel::SYSTEM_AUDIO, false, false));
        assert!(channel::advertised(channel::MICROPHONE, false, false));
    }

    #[test]
    fn hands_free_calls_advertise_the_car_bluetooth_address() {
        let payload = service_discovery_payload_with_density(
            "Test Head Unit",
            DEFAULT_VIDEO_DENSITY_DPI,
            true,
            Some("30:E3:D6:03:96:1E"),
        )
        .expect("SDR payload");
        let response = ServiceDiscoveryResponse::decode(payload.as_slice()).expect("SDR");
        let bluetooth = response
            .channels
            .iter()
            .find(|service| service.id == i32::from(channel::BLUETOOTH))
            .and_then(|service| service.bluetooth_service.as_ref())
            .expect("Bluetooth service");
        assert_eq!(bluetooth.car_address, "30:E3:D6:03:96:1E");
        assert_eq!(
            bluetooth.supported_pairing_methods,
            [BLUETOOTH_PAIRING_NUMERIC_COMPARISON]
        );
        assert!(channel::advertised(channel::BLUETOOTH, true, true));
        assert!(!channel::advertised(channel::BLUETOOTH, true, false));
    }

    #[test]
    fn malformed_bluetooth_addresses_are_rejected() {
        for address in [
            "",
            "30:E3:D6:03:96",
            "30:E3:D6:03:96:1E:00",
            "zz:E3:D6:03:96:1E",
            "30E3D603961E",
        ] {
            assert!(
                service_discovery_payload_with_density(
                    "Test Head Unit",
                    DEFAULT_VIDEO_DENSITY_DPI,
                    true,
                    Some(address),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn authentication_data_carries_the_six_digit_passkey() {
        let data = BluetoothAuthenticationData::decode(bluetooth_authentication(4_217).as_slice())
            .expect("authentication data");
        assert_eq!(data.auth_data, "004217");
        assert_eq!(
            data.pairing_method,
            Some(BLUETOOTH_PAIRING_NUMERIC_COMPARISON)
        );
    }

    #[test]
    fn pairing_answer_reports_whether_a_bond_exists() {
        for already_paired in [false, true] {
            let response = BluetoothPairingResponse::decode(
                bluetooth_pairing_response(already_paired).as_slice(),
            )
            .expect("response");
            assert_eq!(response.status, STATUS_OK);
            assert_eq!(response.already_paired, already_paired);
        }
    }

    #[test]
    fn service_discovery_rejects_out_of_range_video_density() {
        assert!(service_discovery_payload_with_density("Test Head Unit", 99, true, None).is_err());
        assert!(service_discovery_payload_with_density("Test Head Unit", 481, true, None).is_err());
    }
}
