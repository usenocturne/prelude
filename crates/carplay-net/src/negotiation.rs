use std::{fmt, num::NonZeroU16};

use crate::Result;

pub const MAIN_SCREEN_STREAM_TYPE: u16 = 110;
pub const ALTERNATE_SCREEN_STREAM_TYPE: u16 = 111;
pub const MAIN_AUDIO_STREAM_TYPE: u16 = 100;
pub const ALTERNATE_AUDIO_STREAM_TYPE: u16 = 101;
pub const HIGH_AUDIO_STREAM_TYPE: u16 = 102;
pub const DATA_CHANNEL_STREAM_TYPE: u16 = 130;
pub const VIDEO_PLAYBACK_CLIENT_TYPE: &str = "A6B27562-B43A-4F2D-B75F-82391E250194";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScreenKind {
    Main,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AudioStreamKind {
    Main,
    Alternate,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub name: Option<String>,
    pub device_id: Option<String>,
    pub wifi_mac_address: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingDescriptor {
    pub peer_timing_port: Option<NonZeroU16>,
    pub low_power_keepalive: bool,
    pub peer: PeerIdentity,
    pub offered_features: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPorts {
    pub timing_port: NonZeroU16,
    pub event_port: NonZeroU16,
    pub keep_alive_port: Option<NonZeroU16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenDescriptor {
    pub kind: ScreenKind,
    pub connection_id: u64,
}

impl ScreenDescriptor {
    pub const fn stream_type(&self) -> u16 {
        match self.kind {
            ScreenKind::Main => MAIN_SCREEN_STREAM_TYPE,
            ScreenKind::Alternate => ALTERNATE_SCREEN_STREAM_TYPE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneDescriptor {
    pub peer_data_port: NonZeroU16,
    pub frames_per_packet: Option<u32>,
    pub audio_format: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDescriptor {
    pub kind: AudioStreamKind,
    pub connection_id: u64,
    pub audio_type: String,
    pub audio_format: u64,
    pub audio_latency_ms: Option<u32>,
    pub frames_per_packet: Option<u32>,
    pub microphone: Option<MicrophoneDescriptor>,
}

impl AudioDescriptor {
    pub const fn stream_type(&self) -> u16 {
        match self.kind {
            AudioStreamKind::Main => MAIN_AUDIO_STREAM_TYPE,
            AudioStreamKind::Alternate => ALTERNATE_AUDIO_STREAM_TYPE,
            AudioStreamKind::High => HIGH_AUDIO_STREAM_TYPE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataChannelDescriptor {
    pub seed: u64,
    pub channel_id: Option<String>,
    pub client_type_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageChannelDescriptor {
    pub stream_id: u64,
    pub client_type_uuid: String,
    pub client_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDescriptor {
    Screen(ScreenDescriptor),
    Audio(AudioDescriptor),
    DataChannel(DataChannelDescriptor),
    MessageChannel(MessageChannelDescriptor),
}

impl StreamDescriptor {
    pub const fn stream_type(&self) -> u16 {
        match self {
            Self::Screen(screen) => screen.stream_type(),
            Self::Audio(audio) => audio.stream_type(),
            Self::DataChannel(_) | Self::MessageChannel(_) => DATA_CHANNEL_STREAM_TYPE,
        }
    }

    pub const fn connection_id(&self) -> u64 {
        match self {
            Self::Screen(screen) => screen.connection_id,
            Self::Audio(audio) => audio.connection_id,
            Self::DataChannel(channel) => channel.seed,
            Self::MessageChannel(channel) => channel.stream_id,
        }
    }

    pub const fn key(&self) -> StreamKey {
        StreamKey {
            stream_type: self.stream_type(),
            connection_id: self.connection_id(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamKey {
    pub stream_type: u16,
    pub connection_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPorts {
    Screen {
        data_port: NonZeroU16,
    },
    Audio {
        data_port: NonZeroU16,
        control_port: NonZeroU16,
    },
    DataChannel {
        data_port: NonZeroU16,
    },
    MessageChannel,
}

impl StreamPorts {
    pub const fn matches(self, descriptor: &StreamDescriptor) -> bool {
        matches!(
            (self, descriptor),
            (Self::Screen { .. }, StreamDescriptor::Screen(_))
                | (Self::Audio { .. }, StreamDescriptor::Audio(_))
                | (Self::DataChannel { .. }, StreamDescriptor::DataChannel(_))
                | (Self::MessageChannel, StreamDescriptor::MessageChannel(_))
        )
    }
}

pub trait PortProvider {
    fn session_ports(&mut self, timing: &TimingDescriptor) -> Result<SessionPorts>;
    fn stream_ports(&mut self, stream: &StreamDescriptor) -> Result<StreamPorts>;
}

impl fmt::Display for StreamKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.stream_type, self.connection_id)
    }
}
