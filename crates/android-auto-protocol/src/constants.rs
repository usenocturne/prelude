pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 7;

pub const FLAG_PLAINTEXT: u8 = 0x03;
pub const FRAME_TYPE_MASK: u8 = 0x03;
pub const FRAME_TYPE_MIDDLE: u8 = 0x00;
pub const FRAME_TYPE_FIRST: u8 = 0x01;
pub const FRAME_TYPE_LAST: u8 = 0x02;
pub const FRAME_TYPE_BULK: u8 = 0x03;
pub const FLAG_FIRST: u8 = FRAME_TYPE_FIRST;
pub const FLAG_LAST: u8 = FRAME_TYPE_LAST;
pub const FLAG_ENCRYPTED: u8 = 0x08;
pub const FLAG_MESSAGE_CONTROL: u8 = 0x04;
pub const FLAG_ENCRYPTED_SPECIFIC_BASE: u8 = FLAG_ENCRYPTED;
pub const FLAG_ENCRYPTED_CONTROL_BASE: u8 = FLAG_ENCRYPTED | FLAG_MESSAGE_CONTROL;
pub const FLAG_ENCRYPTED_SIGNAL: u8 = FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_BULK;
pub const FLAG_ENCRYPTED_CONTROL: u8 = FLAG_ENCRYPTED_CONTROL_BASE | FRAME_TYPE_BULK;

pub const MAX_PHYSICAL_PAYLOAD: usize = u16::MAX as usize;
pub const MAX_REASSEMBLED_PAYLOAD: usize = 4 * 1024 * 1024;
#[allow(dead_code)]
pub const MAX_PHYSICAL_FRAME: usize = MAX_PHYSICAL_PAYLOAD + 8;
pub const MAX_BUFFERED_WIRE: usize = 512 * 1024;
pub const MAX_PROTO_PAYLOAD: usize = 128 * 1024;
pub const MAX_MIC_PCM: usize = 16 * 1024;
pub const MAX_MIC_BACKLOG: usize = 64;
pub const MAX_TOUCH_POINTERS: usize = 10;

pub mod channel {
    pub const CONTROL: u8 = 0;
    pub const SENSOR: u8 = 1;
    pub const VIDEO: u8 = 3;
    pub const MEDIA_AUDIO: u8 = 4;
    pub const GUIDANCE_AUDIO: u8 = 5;
    pub const SYSTEM_AUDIO: u8 = 6;
    pub const INPUT: u8 = 8;
    pub const MICROPHONE: u8 = 9;
    pub const BLUETOOTH: u8 = 10;

    pub const ADVERTISED: [u8; 7] = [
        SENSOR,
        VIDEO,
        MEDIA_AUDIO,
        GUIDANCE_AUDIO,
        SYSTEM_AUDIO,
        INPUT,
        MICROPHONE,
    ];

    pub const PHONE_AUDIO: [u8; 2] = [MEDIA_AUDIO, GUIDANCE_AUDIO];

    pub fn advertised(channel_id: u8, audio_output: bool, bluetooth: bool) -> bool {
        if channel_id == BLUETOOTH {
            return bluetooth;
        }
        ADVERTISED.contains(&channel_id) && (audio_output || !PHONE_AUDIO.contains(&channel_id))
    }
}

pub mod message {
    pub const VERSION_REQUEST: u16 = 0x0001;
    pub const VERSION_RESPONSE: u16 = 0x0002;
    pub const TLS_HANDSHAKE: u16 = 0x0003;
    pub const AUTH_COMPLETE: u16 = 0x0004;
    pub const SERVICE_DISCOVERY_REQUEST: u16 = 0x0005;
    pub const SERVICE_DISCOVERY_RESPONSE: u16 = 0x0006;
    pub const CHANNEL_OPEN_REQUEST: u16 = 0x0007;
    pub const CHANNEL_OPEN_RESPONSE: u16 = 0x0008;
    pub const CHANNEL_CLOSE: u16 = 0x0009;
    pub const PING_REQUEST: u16 = 0x000b;
    pub const PING_RESPONSE: u16 = 0x000c;
    pub const NAVIGATION_FOCUS_REQUEST: u16 = 0x000d;
    pub const NAVIGATION_FOCUS_RESPONSE: u16 = 0x000e;
    pub const SHUTDOWN_REQUEST: u16 = 0x000f;
    pub const SHUTDOWN_RESPONSE: u16 = 0x0010;
    pub const VOICE_SESSION_NOTIFICATION: u16 = 0x0011;
    pub const AUDIO_FOCUS_REQUEST: u16 = 0x0012;
    pub const AUDIO_FOCUS_RESPONSE: u16 = 0x0013;
    pub const BATTERY_STATUS_NOTIFICATION: u16 = 0x0017;
    pub const BINDING_REQUEST: u16 = 0x0019;
    pub const BINDING_RESPONSE: u16 = 0x001a;

    pub const MEDIA_WITH_TIMESTAMP: u16 = 0x0000;
    pub const MEDIA: u16 = 0x0001;
    pub const AV_SETUP_REQUEST: u16 = 0x8000;
    pub const AV_START: u16 = 0x8001;
    pub const AV_STOP: u16 = 0x8002;
    pub const AV_SETUP_RESPONSE: u16 = 0x8003;
    pub const AV_ACK: u16 = 0x8004;
    pub const MICROPHONE_OPEN_REQUEST: u16 = 0x8005;
    pub const MICROPHONE_OPEN_RESPONSE: u16 = 0x8006;
    pub const VIDEO_FOCUS_REQUEST: u16 = 0x8007;
    pub const VIDEO_FOCUS_INDICATION: u16 = 0x8008;

    pub const SENSOR_START_REQUEST: u16 = 0x8001;
    pub const SENSOR_START_RESPONSE: u16 = 0x8002;
    pub const SENSOR_BATCH: u16 = 0x8003;

    pub const INPUT_REPORT: u16 = 0x8001;
    pub const KEY_BINDING_REQUEST: u16 = 0x8002;
    pub const KEY_BINDING_RESPONSE: u16 = 0x8003;

    pub const BLUETOOTH_PAIRING_REQUEST: u16 = 0x8001;
    pub const BLUETOOTH_PAIRING_RESPONSE: u16 = 0x8002;
    pub const BLUETOOTH_AUTHENTICATION_DATA: u16 = 0x8003;
    pub const BLUETOOTH_AUTHENTICATION_RESULT: u16 = 0x8004;
}

pub const MEDIA_CODEC_PCM: i32 = 1;
pub const MEDIA_CODEC_H264_BASELINE: i32 = 3;
pub const VIDEO_RESOLUTION_480P: i32 = 1;
pub const VIDEO_FPS_60: i32 = 1;
pub const VIDEO_FPS_30: i32 = 2;
pub const AV_SETUP_OK: i32 = 2;
pub const STATUS_OK: i32 = 0;
pub const BLUETOOTH_PAIRING_NUMERIC_COMPARISON: i32 = 2;
