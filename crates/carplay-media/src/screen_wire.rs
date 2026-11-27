use zeroize::{Zeroize, Zeroizing};

use crate::{Error, Result, wire_crypto};

pub const AIRPLAY_SCREEN_HEADER_LENGTH: usize = 128;
pub const DEFAULT_MAXIMUM_SCREEN_BODY: usize = 8 * 1024 * 1024;

const VIDEO_FRAME_OPCODE: u8 = 0;
const VIDEO_CONFIG_OPCODE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireVideoCodec {
    H264,
    H265,
}

impl WireVideoCodec {
    pub fn is_random_access(self, unit_type: u8) -> bool {
        match self {
            Self::H264 => unit_type == 5,
            Self::H265 => (16..=21).contains(&unit_type),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenWireState {
    Active,
    Stopped,
    Failed,
}

impl ScreenWireState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoCodecConfiguration {
    pub codec: WireVideoCodec,
    pub codec_data: Vec<u8>,
    pub nal_length_bytes: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnitDescriptor {
    pub payload_offset: usize,
    pub payload_length: usize,
    pub unit_type: u8,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedVideoFrame {
    pub data: Vec<u8>,
    pub codec: WireVideoCodec,
    pub nal_units: Vec<NalUnitDescriptor>,
    pub presentation_time_ns: Option<u64>,
    pub nonce_counter: u64,
}

impl EncryptedVideoFrame {
    pub fn is_key_frame(&self) -> bool {
        self.nal_units
            .iter()
            .any(|descriptor| self.codec.is_random_access(descriptor.unit_type))
    }
}

impl std::fmt::Debug for EncryptedVideoFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedVideoFrame")
            .field(
                "data",
                &format_args!("<redacted:{} bytes>", self.data.len()),
            )
            .field("codec", &self.codec)
            .field("nal_units", &self.nal_units)
            .field("nonce_counter", &self.nonce_counter)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenControlMessage {
    pub opcode: u8,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenWireEvent {
    Configuration(VideoCodecConfiguration),
    Frame(EncryptedVideoFrame),
    Control(ScreenControlMessage),
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenWireLimits {
    pub maximum_body_bytes: usize,
    pub maximum_buffer_bytes: usize,
    pub maximum_nal_units: usize,
}

impl Default for ScreenWireLimits {
    fn default() -> Self {
        Self {
            maximum_body_bytes: DEFAULT_MAXIMUM_SCREEN_BODY,
            maximum_buffer_bytes: DEFAULT_MAXIMUM_SCREEN_BODY + AIRPLAY_SCREEN_HEADER_LENGTH,
            maximum_nal_units: 4096,
        }
    }
}

pub struct ScreenWireDecoder {
    key: Zeroizing<[u8; 32]>,
    limits: ScreenWireLimits,
    buffer: Vec<u8>,
    counter: u64,
    configuration: Option<VideoCodecConfiguration>,
    state: ScreenWireState,
}

impl std::fmt::Debug for ScreenWireDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScreenWireDecoder")
            .field("key", &"<redacted>")
            .field("limits", &self.limits)
            .field("buffered_bytes", &self.buffer.len())
            .field("counter", &self.counter)
            .field("configuration", &self.configuration)
            .field("state", &self.state)
            .finish()
    }
}

impl ScreenWireDecoder {
    pub fn new(key: [u8; 32], limits: ScreenWireLimits) -> Result<Self> {
        if limits.maximum_body_bytes < wire_crypto::TAG_LENGTH
            || limits.maximum_buffer_bytes
                < limits.maximum_body_bytes + AIRPLAY_SCREEN_HEADER_LENGTH
            || limits.maximum_nal_units == 0
        {
            return Err(Error::InvalidVideo("invalid screen wire limits".to_owned()));
        }
        Ok(Self {
            key: Zeroizing::new(key),
            limits,
            buffer: Vec::new(),
            counter: 0,
            configuration: None,
            state: ScreenWireState::Active,
        })
    }

    pub fn state(&self) -> ScreenWireState {
        self.state
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn configuration(&self) -> Option<&VideoCodecConfiguration> {
        self.configuration.as_ref()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ScreenWireEvent>> {
        if self.state != ScreenWireState::Active {
            return Err(Error::NotReady {
                component: "screen wire decoder",
                state: self.state.as_str(),
            });
        }
        let total = self
            .buffer
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| Error::InvalidVideo("screen buffer length overflow".to_owned()))?;
        if total > self.limits.maximum_buffer_bytes {
            return self.fail(Error::InvalidVideo(
                "screen buffer exceeds configured maximum".to_owned(),
            ));
        }
        self.buffer.extend_from_slice(bytes);
        let result = self.decode_buffered();
        if result.is_err() {
            self.state = ScreenWireState::Failed;
            self.key.zeroize();
            self.buffer.zeroize();
            self.buffer.clear();
            self.configuration = None;
        }
        result
    }

    pub fn teardown(&mut self) -> Option<ScreenWireEvent> {
        if self.state == ScreenWireState::Stopped {
            return None;
        }
        self.buffer.zeroize();
        self.buffer.clear();
        self.configuration = None;
        self.key.zeroize();
        self.state = ScreenWireState::Stopped;
        Some(ScreenWireEvent::Teardown)
    }

    fn decode_buffered(&mut self) -> Result<Vec<ScreenWireEvent>> {
        let mut events = Vec::new();
        loop {
            if self.buffer.len() < AIRPLAY_SCREEN_HEADER_LENGTH {
                break;
            }
            let body_length =
                u32::from_le_bytes(self.buffer[..4].try_into().map_err(|_| {
                    Error::InvalidVideo("screen body length is truncated".to_owned())
                })?) as usize;
            if body_length > self.limits.maximum_body_bytes {
                return Err(Error::InvalidVideo(format!(
                    "screen body is {body_length} bytes; maximum is {}",
                    self.limits.maximum_body_bytes
                )));
            }
            let message_length = AIRPLAY_SCREEN_HEADER_LENGTH
                .checked_add(body_length)
                .ok_or_else(|| Error::InvalidVideo("screen message length overflow".to_owned()))?;
            if self.buffer.len() < message_length {
                break;
            }
            let mut header = [0; AIRPLAY_SCREEN_HEADER_LENGTH];
            header.copy_from_slice(&self.buffer[..AIRPLAY_SCREEN_HEADER_LENGTH]);
            let body = self.buffer[AIRPLAY_SCREEN_HEADER_LENGTH..message_length].to_vec();
            let event = self.decode_message(&header, &body)?;
            events.push(event);
            self.buffer.drain(..message_length);
        }
        Ok(events)
    }

    fn decode_message(
        &mut self,
        header: &[u8; AIRPLAY_SCREEN_HEADER_LENGTH],
        body: &[u8],
    ) -> Result<ScreenWireEvent> {
        match header[4] {
            VIDEO_CONFIG_OPCODE => {
                let configuration = detect_video_configuration(body)?;
                self.configuration = Some(configuration.clone());
                Ok(ScreenWireEvent::Configuration(configuration))
            }
            VIDEO_FRAME_OPCODE => {
                let Some(configuration) = self.configuration.as_ref() else {
                    return Err(Error::InvalidVideo(
                        "video frame arrived before codec configuration".to_owned(),
                    ));
                };
                if body.len() <= wire_crypto::TAG_LENGTH {
                    return Err(Error::InvalidVideo(
                        "encrypted video frame has no payload".to_owned(),
                    ));
                }
                if self.counter == u64::MAX {
                    return Err(Error::InvalidVideo(
                        "video nonce counter is exhausted".to_owned(),
                    ));
                }
                let counter = self.counter;
                let data =
                    wire_crypto::open(&self.key, &wire_crypto::nonce(counter), body, header)?;
                let nal_units = validate_length_prefixed_nals(
                    &data,
                    configuration.codec,
                    configuration.nal_length_bytes,
                    self.limits.maximum_nal_units,
                )?;
                self.counter += 1;
                Ok(ScreenWireEvent::Frame(EncryptedVideoFrame {
                    data,
                    codec: configuration.codec,
                    nal_units,
                    presentation_time_ns: None,
                    nonce_counter: counter,
                }))
            }
            opcode => Ok(ScreenWireEvent::Control(ScreenControlMessage {
                opcode,
                body: body.to_vec(),
            })),
        }
    }

    fn fail<T>(&mut self, error: Error) -> Result<T> {
        self.state = ScreenWireState::Failed;
        self.key.zeroize();
        self.buffer.zeroize();
        self.buffer.clear();
        self.configuration = None;
        Err(error)
    }
}

pub fn detect_video_configuration(payload: &[u8]) -> Result<VideoCodecConfiguration> {
    if let Some(record) = record_after_fourcc(payload, b"avcC") {
        return avcc_configuration(record);
    }
    if let Some(record) = record_after_fourcc(payload, b"hvcC") {
        return hvcc_configuration(record);
    }
    if looks_like_avcc(payload) {
        return avcc_configuration(payload);
    }
    if looks_like_hvcc(payload) {
        return hvcc_configuration(payload);
    }
    Err(Error::InvalidVideo(
        "VideoConfig is neither a clear avcC nor hvcC record".to_owned(),
    ))
}

fn record_after_fourcc<'a>(payload: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    for index in 4..=payload.len().saturating_sub(4) {
        if payload.get(index..index + 4) != Some(fourcc) {
            continue;
        }
        let record_start = index + 4;
        let box_start = index - 4;
        let box_size = u32::from_be_bytes(payload[box_start..index].try_into().ok()?) as usize;
        let record_end = box_start.checked_add(box_size)?;
        if box_size >= 8 && record_end <= payload.len() && record_end > record_start {
            return Some(&payload[record_start..record_end]);
        }
        return payload.get(record_start..);
    }
    None
}

fn looks_like_avcc(record: &[u8]) -> bool {
    if record.len() < 9 || record[0] != 1 || record[5] & 0x1f == 0 {
        return false;
    }
    let sps_length = usize::from(u16::from_be_bytes([record[6], record[7]]));
    8 + sps_length <= record.len() && record[8] & 0x1f == 7
}

fn looks_like_hvcc(record: &[u8]) -> bool {
    record.len() >= 23 && record[0] == 1 && record[21] & 0x03 != 2
}

fn avcc_configuration(record: &[u8]) -> Result<VideoCodecConfiguration> {
    if !looks_like_avcc(record) {
        return Err(Error::InvalidVideo("malformed avcC record".to_owned()));
    }
    let length_minus_one = record[4] & 0x03;
    if length_minus_one == 2 {
        return Err(Error::InvalidVideo(
            "avcC uses reserved NAL length size".to_owned(),
        ));
    }
    Ok(VideoCodecConfiguration {
        codec: WireVideoCodec::H264,
        codec_data: record.to_vec(),
        nal_length_bytes: length_minus_one + 1,
    })
}

fn hvcc_configuration(record: &[u8]) -> Result<VideoCodecConfiguration> {
    if !looks_like_hvcc(record) {
        return Err(Error::InvalidVideo("malformed hvcC record".to_owned()));
    }
    Ok(VideoCodecConfiguration {
        codec: WireVideoCodec::H265,
        codec_data: record.to_vec(),
        nal_length_bytes: (record[21] & 0x03) + 1,
    })
}

fn validate_length_prefixed_nals(
    data: &[u8],
    codec: WireVideoCodec,
    length_bytes: u8,
    maximum_nal_units: usize,
) -> Result<Vec<NalUnitDescriptor>> {
    if data.is_empty() || !(1..=4).contains(&length_bytes) {
        return Err(Error::InvalidVideo(
            "empty frame or invalid NAL length size".to_owned(),
        ));
    }
    let length_bytes = usize::from(length_bytes);
    let mut offset = 0;
    let mut units = Vec::new();
    while offset < data.len() {
        if units.len() >= maximum_nal_units || data.len() - offset < length_bytes {
            return Err(Error::InvalidVideo(
                "too many NAL units or truncated NAL length".to_owned(),
            ));
        }
        let mut length = 0usize;
        for byte in &data[offset..offset + length_bytes] {
            length = length
                .checked_mul(256)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .ok_or_else(|| Error::InvalidVideo("NAL length overflow".to_owned()))?;
        }
        offset += length_bytes;
        if length == 0 || length > data.len() - offset {
            return Err(Error::InvalidVideo(
                "empty or truncated length-prefixed NAL unit".to_owned(),
            ));
        }
        let payload = &data[offset..offset + length];
        let unit_type = match codec {
            WireVideoCodec::H264 => {
                if payload[0] & 0x80 != 0 {
                    return Err(Error::InvalidVideo(
                        "H.264 forbidden_zero_bit is set".to_owned(),
                    ));
                }
                let unit_type = payload[0] & 0x1f;
                if unit_type == 0 || unit_type >= 24 {
                    return Err(Error::InvalidVideo(format!(
                        "unsupported H.264 NAL unit type {unit_type}"
                    )));
                }
                unit_type
            }
            WireVideoCodec::H265 => {
                if payload.len() < 2 || payload[0] & 0x80 != 0 || payload[1] & 0x07 == 0 {
                    return Err(Error::InvalidVideo("invalid H.265 NAL header".to_owned()));
                }
                (payload[0] >> 1) & 0x3f
            }
        };
        units.push(NalUnitDescriptor {
            payload_offset: offset,
            payload_length: length,
            unit_type,
        });
        offset += length;
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0x11; 32];

    fn avcc() -> Vec<u8> {
        vec![1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x28]
    }

    fn message(opcode: u8, body: &[u8]) -> Vec<u8> {
        let mut header = [0; AIRPLAY_SCREEN_HEADER_LENGTH];
        header[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
        header[4] = opcode;
        [header.as_slice(), body].concat()
    }

    fn frame(plaintext: &[u8], counter: u64) -> Vec<u8> {
        let mut header = [0; AIRPLAY_SCREEN_HEADER_LENGTH];
        header[..4]
            .copy_from_slice(&((plaintext.len() + wire_crypto::TAG_LENGTH) as u32).to_le_bytes());
        header[4] = VIDEO_FRAME_OPCODE;
        let sealed = wire_crypto::seal(&KEY, &wire_crypto::nonce(counter), plaintext, &header)
            .expect("seal");
        [header.as_slice(), sealed.as_slice()].concat()
    }

    #[test]
    fn random_access_pictures_depend_on_the_codec() {
        assert!(WireVideoCodec::H264.is_random_access(5));
        assert!(!WireVideoCodec::H264.is_random_access(1));
        assert!(!WireVideoCodec::H264.is_random_access(19));
        for unit_type in 16..=21 {
            assert!(WireVideoCodec::H265.is_random_access(unit_type));
        }
        assert!(!WireVideoCodec::H265.is_random_access(1));
        assert!(!WireVideoCodec::H265.is_random_access(5));
    }

    #[test]
    fn livi_avcc_box_and_bare_hvcc_are_detected_without_fallback() {
        let record = avcc();
        let mut boxed = Vec::new();
        boxed.extend_from_slice(&((record.len() + 8) as u32).to_be_bytes());
        boxed.extend_from_slice(b"avcC");
        boxed.extend_from_slice(&record);
        let config = detect_video_configuration(&boxed).expect("avcC");
        assert_eq!(config.codec, WireVideoCodec::H264);
        assert_eq!(config.codec_data, record);
        assert_eq!(config.nal_length_bytes, 4);

        let mut hvcc = vec![0; 23];
        hvcc[0] = 1;
        hvcc[21] = 0xff;
        assert_eq!(
            detect_video_configuration(&hvcc).expect("hvcC").codec,
            WireVideoCodec::H265
        );
        assert!(detect_video_configuration(&[1, 2, 3]).is_err());
    }

    #[test]
    fn incremental_config_and_encrypted_frames_use_header_aad_and_counter() {
        let mut decoder =
            ScreenWireDecoder::new(KEY, ScreenWireLimits::default()).expect("decoder");
        let config = message(VIDEO_CONFIG_OPCODE, &avcc());
        assert!(decoder.push(&config[..63]).expect("partial").is_empty());
        let events = decoder.push(&config[63..]).expect("config");
        assert!(matches!(events[0], ScreenWireEvent::Configuration(_)));

        let mut access_unit = Vec::new();
        access_unit.extend_from_slice(&3_u32.to_be_bytes());
        access_unit.extend_from_slice(&[0x65, 1, 2]);
        let first = frame(&access_unit, 0);
        let second = frame(&access_unit, 1);
        let events = decoder.push(&[first, second].concat()).expect("frames");
        assert_eq!(events.len(), 2);
        let ScreenWireEvent::Frame(first) = &events[0] else {
            panic!("frame")
        };
        assert_eq!(first.data, access_unit);
        assert_eq!(first.nal_units[0].unit_type, 5);
        assert_eq!(first.nonce_counter, 0);
    }

    #[test]
    fn oversize_tamper_and_malformed_nals_fail_closed() {
        let limits = ScreenWireLimits {
            maximum_body_bytes: 64,
            maximum_buffer_bytes: 192,
            maximum_nal_units: 4,
        };
        let mut decoder = ScreenWireDecoder::new(KEY, limits).expect("decoder");
        let mut header = [0; AIRPLAY_SCREEN_HEADER_LENGTH];
        header[..4].copy_from_slice(&65_u32.to_le_bytes());
        assert!(matches!(decoder.push(&header), Err(Error::InvalidVideo(_))));
        assert_eq!(decoder.state(), ScreenWireState::Failed);

        let mut decoder =
            ScreenWireDecoder::new(KEY, ScreenWireLimits::default()).expect("decoder");
        decoder.push(&message(1, &avcc())).expect("config");
        let mut bad = frame(&[0, 0, 0, 2, 0x65, 1], 0);
        *bad.last_mut().expect("tag byte") ^= 1;
        assert!(matches!(decoder.push(&bad), Err(Error::Crypto(_))));
        assert_eq!(decoder.state(), ScreenWireState::Failed);

        let mut decoder =
            ScreenWireDecoder::new(KEY, ScreenWireLimits::default()).expect("decoder");
        decoder.push(&message(1, &avcc())).expect("config");
        assert!(matches!(
            decoder.push(&frame(&[0, 0, 0, 9, 0x65], 0)),
            Err(Error::InvalidVideo(_))
        ));
    }

    #[test]
    fn teardown_is_terminal_and_zeroizes_state() {
        let mut decoder =
            ScreenWireDecoder::new(KEY, ScreenWireLimits::default()).expect("decoder");
        assert_eq!(decoder.teardown(), Some(ScreenWireEvent::Teardown));
        assert_eq!(decoder.teardown(), None);
        assert!(matches!(decoder.push(&[]), Err(Error::NotReady { .. })));
    }
}
