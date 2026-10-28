use std::collections::BTreeSet;
#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
use std::time::Duration;

use crate::{
    AudioFormatId, AudioStreamId, EncodedAudioCodec, EncryptedAudioFrame, Error, MicrophoneCodec,
    MicrophoneConfig, Result, VideoCodecConfiguration, VideoFormat, WireVideoCodec,
};

pub const SCREEN_WIDTH: u16 = 800;
pub const SCREEN_HEIGHT: u16 = 480;
pub const SCREEN_FRAMES_PER_SECOND: u16 = 30;
pub const TARGET_VIDEO_DEVICE: &str = "/dev/video1";
pub const TARGET_VIDEO_DRIVER_NAME: &str = "meson-video-decoder";
pub const MAXIMUM_CODEC_DATA_BYTES: usize = 64 * 1024;
pub const MAXIMUM_BACKEND_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAXIMUM_AUDIO_STREAMS: usize = 3;
pub const MAXIMUM_RTP_PACKET_BYTES: usize = 64 * 1024;
pub const UNITY_GAIN_Q15: u16 = i16::MAX as u16;
pub const MIXER_TEST_TONE_SAMPLES: u32 = 1_024;
pub const MIXER_OUTPUT_BUFFER_MICROS: u32 = 10_000;
pub const MICROPHONE_CAPTURE_PERIOD_MICROS: u32 = 10_000;
pub const DEFAULT_MICROPHONE_FRAME_MICROS: u32 = 20_000;
const MIXER_SAMPLE_RATE_HZ: u32 = 48_000;
const OPUS_RTP_CLOCK_RATE_HZ: u32 = 48_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsValue {
    String(String),
    Signed(i32),
    Unsigned(u32),
    Fraction(u32, u32),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsPlan {
    pub media_type: &'static str,
    pub fields: Vec<(&'static str, CapsValue)>,
}

impl CapsPlan {
    pub fn field(&self, name: &str) -> Option<&CapsValue> {
        self.fields
            .iter()
            .find_map(|(field, value)| (*field == name).then_some(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineElementPlan {
    pub factory: &'static str,
    pub name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPipelinePlan {
    pub format: VideoFormat,
    pub decoder_device: &'static str,
    pub decoder_driver_name: &'static str,
    pub input_caps: CapsPlan,
    pub output_caps: CapsPlan,
    pub elements: Vec<PipelineElementPlan>,
}

impl VideoPipelinePlan {
    pub fn h264(configuration: &VideoCodecConfiguration) -> Result<Self> {
        if configuration.codec != WireVideoCodec::H264 {
            return Err(Error::Unsupported(
                "the target media backend currently supports H.264 only",
            ));
        }
        if configuration.codec_data.is_empty()
            || configuration.codec_data.len() > MAXIMUM_CODEC_DATA_BYTES
            || !matches!(configuration.nal_length_bytes, 1 | 2 | 4)
        {
            return Err(Error::InvalidVideo(
                "H.264 codec data or NAL length width is invalid".to_owned(),
            ));
        }
        Ok(Self {
            format: VideoFormat {
                width: SCREEN_WIDTH,
                height: SCREEN_HEIGHT,
                maximum_fps: SCREEN_FRAMES_PER_SECOND,
            },
            decoder_device: TARGET_VIDEO_DEVICE,
            decoder_driver_name: TARGET_VIDEO_DRIVER_NAME,
            input_caps: CapsPlan {
                media_type: "video/x-h264",
                fields: vec![
                    ("stream-format", CapsValue::String("avc".to_owned())),
                    ("alignment", CapsValue::String("au".to_owned())),
                    (
                        "codec_data",
                        CapsValue::Bytes(configuration.codec_data.clone()),
                    ),
                    ("width", CapsValue::Signed(i32::from(SCREEN_WIDTH))),
                    ("height", CapsValue::Signed(i32::from(SCREEN_HEIGHT))),
                    (
                        "framerate",
                        CapsValue::Fraction(u32::from(SCREEN_FRAMES_PER_SECOND), 1),
                    ),
                ],
            },
            output_caps: CapsPlan {
                media_type: "video/x-raw",
                fields: vec![
                    ("format", CapsValue::String("NV12".to_owned())),
                    ("width", CapsValue::Signed(i32::from(SCREEN_WIDTH))),
                    ("height", CapsValue::Signed(i32::from(SCREEN_HEIGHT))),
                    (
                        "framerate",
                        CapsValue::Fraction(u32::from(SCREEN_FRAMES_PER_SECOND), 1),
                    ),
                ],
            },
            elements: vec![
                PipelineElementPlan {
                    factory: "appsrc",
                    name: "screen-source",
                },
                PipelineElementPlan {
                    factory: "h264parse",
                    name: "h264-parser",
                },
                PipelineElementPlan {
                    factory: "queue",
                    name: "decoder-input-queue",
                },
                PipelineElementPlan {
                    factory: "v4l2h264dec",
                    name: "hardware-decoder",
                },
                PipelineElementPlan {
                    factory: "queue",
                    name: "renderer-queue",
                },
                PipelineElementPlan {
                    factory: "appsink",
                    name: "screen-sink",
                },
            ],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamPlan {
    pub stream_id: AudioStreamId,
    pub format_id: AudioFormatId,
    pub codec: EncodedAudioCodec,
    pub input_caps: CapsPlan,
    pub elements: Vec<PipelineElementPlan>,
    pub aac_audio_specific_config: Option<[u8; 2]>,
    pub jitter_latency_ms: Option<u32>,
    pub playout_latency_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStreamConfig {
    pub stream_id: AudioStreamId,
    pub format_id: AudioFormatId,
    pub negotiated_latency_ms: Option<u32>,
}

impl AudioStreamConfig {
    pub const fn new(stream_id: AudioStreamId, format_id: AudioFormatId) -> Self {
        Self {
            stream_id,
            format_id,
            negotiated_latency_ms: None,
        }
    }

    pub const fn with_negotiated_latency(
        stream_id: AudioStreamId,
        format_id: AudioFormatId,
        negotiated_latency_ms: Option<u32>,
    ) -> Self {
        Self {
            stream_id,
            format_id,
            negotiated_latency_ms,
        }
    }
}

impl AudioStreamPlan {
    pub fn new(stream_id: AudioStreamId, format_id: AudioFormatId) -> Result<Self> {
        Self::with_latency(
            AudioStreamConfig::new(stream_id, format_id),
            AudioLatencyConfig::default(),
        )
    }

    pub(crate) fn with_latency(
        stream: AudioStreamConfig,
        latency: AudioLatencyConfig,
    ) -> Result<Self> {
        let AudioStreamConfig {
            stream_id,
            format_id,
            negotiated_latency_ms,
        } = stream;
        let codec = format_id.codec()?;
        let playout_latency_ms =
            latency.effective_playout_latency_ms(format_id, negotiated_latency_ms)?;
        let (input_caps, codec_elements, aac_audio_specific_config, jitter_latency_ms) = match codec
        {
            EncodedAudioCodec::PcmS16Be {
                sample_rate_hz,
                channels,
            } => (
                CapsPlan {
                    media_type: "audio/x-raw",
                    fields: vec![
                        ("format", CapsValue::String("S16BE".to_owned())),
                        ("layout", CapsValue::String("interleaved".to_owned())),
                        ("rate", CapsValue::Signed(rate_as_i32(sample_rate_hz)?)),
                        ("channels", CapsValue::Signed(i32::from(channels))),
                    ],
                },
                Vec::new(),
                None,
                None,
            ),
            EncodedAudioCodec::Opus { channels, .. } => (
                rtp_caps(
                    "OPUS",
                    OPUS_RTP_CLOCK_RATE_HZ,
                    channels,
                    stream_id,
                    Vec::new(),
                )?,
                vec![
                    PipelineElementPlan {
                        factory: "rtpjitterbuffer",
                        name: "opus-jitter-buffer",
                    },
                    PipelineElementPlan {
                        factory: "rtpopusdepay",
                        name: "opus-depayloader",
                    },
                    PipelineElementPlan {
                        factory: "opusdec",
                        name: "opus-decoder",
                    },
                ],
                None,
                Some(playout_latency_ms),
            ),
            EncodedAudioCodec::AacLc {
                sample_rate_hz,
                channels,
            } => {
                let config = aac_lc_audio_specific_config(sample_rate_hz, channels)?;
                let config_hex = format!("{:02x}{:02x}", config[0], config[1]);
                let extra = vec![
                    ("mode", CapsValue::String("AAC-hbr".to_owned())),
                    ("config", CapsValue::String(config_hex)),
                    ("sizelength", CapsValue::String("13".to_owned())),
                    ("indexlength", CapsValue::String("3".to_owned())),
                    ("indexdeltalength", CapsValue::String("3".to_owned())),
                ];
                (
                    rtp_caps("MPEG4-GENERIC", sample_rate_hz, channels, stream_id, extra)?,
                    vec![
                        PipelineElementPlan {
                            factory: "rtpjitterbuffer",
                            name: "aac-jitter-buffer",
                        },
                        PipelineElementPlan {
                            factory: "rtpmp4gdepay",
                            name: "aac-depayloader",
                        },
                        PipelineElementPlan {
                            factory: "aacparse",
                            name: "aac-parser",
                        },
                        PipelineElementPlan {
                            factory: "faad",
                            name: "aac-decoder",
                        },
                    ],
                    Some(config),
                    Some(playout_latency_ms),
                )
            }
        };
        let mut elements = vec![
            PipelineElementPlan {
                factory: "appsrc",
                name: "audio-source",
            },
            PipelineElementPlan {
                factory: "queue",
                name: "audio-input-queue",
            },
        ];
        elements.extend(codec_elements);
        elements.extend([
            PipelineElementPlan {
                factory: "audioconvert",
                name: "audio-converter",
            },
            PipelineElementPlan {
                factory: "audioresample",
                name: "audio-resampler",
            },
            PipelineElementPlan {
                factory: "queue",
                name: "mixer-input-queue",
            },
        ]);
        Ok(Self {
            stream_id,
            format_id,
            codec,
            input_caps,
            elements,
            aac_audio_specific_config,
            jitter_latency_ms,
            playout_latency_ms,
        })
    }

    pub fn adapt_frame(&self, frame: &EncryptedAudioFrame) -> Result<Vec<u8>> {
        if frame.stream_id != self.stream_id
            || frame.format_id != self.format_id
            || frame.codec != self.codec
        {
            return Err(Error::InvalidAudio(
                "audio frame does not match its configured mixer branch".to_owned(),
            ));
        }
        if frame.rtp_packet.len() > MAXIMUM_RTP_PACKET_BYTES {
            return Err(Error::InvalidAudio(
                "audio RTP packet exceeds backend limit".to_owned(),
            ));
        }
        match self.codec {
            EncodedAudioCodec::PcmS16Be { .. } => {
                if frame.payload.is_empty() || frame.payload.len() % 2 != 0 {
                    return Err(Error::InvalidAudio(
                        "PCM S16BE frame is empty or sample-misaligned".to_owned(),
                    ));
                }
                Ok(frame.payload.clone())
            }
            EncodedAudioCodec::Opus { sample_rate_hz, .. } => opus_rtp(frame, sample_rate_hz),
            EncodedAudioCodec::AacLc { .. } => reframe_aac_rtp(frame),
        }
    }
}

fn rtp_caps(
    encoding_name: &'static str,
    sample_rate_hz: u32,
    channels: u8,
    stream_id: AudioStreamId,
    mut extra: Vec<(&'static str, CapsValue)>,
) -> Result<CapsPlan> {
    let mut fields = vec![
        ("media", CapsValue::String("audio".to_owned())),
        (
            "clock-rate",
            CapsValue::Signed(rate_as_i32(sample_rate_hz)?),
        ),
        ("encoding-name", CapsValue::String(encoding_name.to_owned())),
        ("encoding-params", CapsValue::String(channels.to_string())),
        ("payload", CapsValue::Signed(i32::from(stream_id as u8))),
    ];
    fields.append(&mut extra);
    Ok(CapsPlan {
        media_type: "application/x-rtp",
        fields,
    })
}

fn rate_as_i32(rate: u32) -> Result<i32> {
    i32::try_from(rate).map_err(|_| Error::InvalidAudio("sample rate overflows caps".to_owned()))
}

fn validated_rtp(frame: &EncryptedAudioFrame) -> Result<Vec<u8>> {
    if frame.rtp_packet.len() < 13
        || frame.rtp_packet.get(..12).is_none()
        || frame.rtp_packet.get(12..) != Some(frame.payload.as_slice())
    {
        return Err(Error::InvalidAudio(
            "reconstructed RTP packet does not contain the decrypted payload".to_owned(),
        ));
    }
    Ok(frame.rtp_packet.clone())
}

fn opus_rtp(frame: &EncryptedAudioFrame, sample_rate_hz: u32) -> Result<Vec<u8>> {
    if sample_rate_hz == 0 || OPUS_RTP_CLOCK_RATE_HZ % sample_rate_hz != 0 {
        return Err(Error::InvalidAudio(
            "Opus sample rate does not divide the 48 kHz RTP clock".to_owned(),
        ));
    }
    let mut rtp = validated_rtp(frame)?;
    let timestamp = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]])
        .wrapping_mul(OPUS_RTP_CLOCK_RATE_HZ / sample_rate_hz);
    rtp[4..8].copy_from_slice(&timestamp.to_be_bytes());
    Ok(rtp)
}

fn reframe_aac_rtp(frame: &EncryptedAudioFrame) -> Result<Vec<u8>> {
    const MAXIMUM_AAC_AU_BYTES: usize = 0x1fff;
    let rtp = validated_rtp(frame)?;
    if frame.payload.is_empty() || frame.payload.len() > MAXIMUM_AAC_AU_BYTES {
        return Err(Error::InvalidAudio(
            "AAC access unit is empty or exceeds the 13-bit RFC 3640 size".to_owned(),
        ));
    }
    let au_bits = u16::try_from(frame.payload.len())
        .map_err(|_| Error::InvalidAudio("AAC access unit length overflows".to_owned()))?
        << 3;
    let mut adapted = Vec::with_capacity(rtp.len() + 4);
    adapted.extend_from_slice(&rtp[..12]);
    adapted[1] = 0x80 | (self_payload_type(frame.stream_id) & 0x7f);
    adapted.extend_from_slice(&16_u16.to_be_bytes());
    adapted.extend_from_slice(&au_bits.to_be_bytes());
    adapted.extend_from_slice(&frame.payload);
    Ok(adapted)
}

const fn self_payload_type(stream_id: AudioStreamId) -> u8 {
    stream_id as u8
}

fn aac_lc_audio_specific_config(sample_rate_hz: u32, channels: u8) -> Result<[u8; 2]> {
    let frequency_index = match sample_rate_hz {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        _ => {
            return Err(Error::Unsupported(
                "AAC-LC sample rate has no supported AudioSpecificConfig index",
            ));
        }
    };
    if !(1..=7).contains(&channels) {
        return Err(Error::InvalidAudio(
            "AAC-LC channel configuration is invalid".to_owned(),
        ));
    }
    let value = (2_u16 << 11) | (frequency_index << 7) | (u16::from(channels) << 3);
    Ok(value.to_be_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMixPolicy {
    pub normal_main_q15: u16,
    pub normal_alternate_q15: u16,
    pub normal_high_q15: u16,
    pub ducked_main_q15: u16,
    pub ducked_alternate_q15: u16,
    pub ducked_high_q15: u16,
}

impl AudioMixPolicy {
    pub const fn carplay() -> Self {
        Self {
            normal_main_q15: UNITY_GAIN_Q15,
            normal_alternate_q15: UNITY_GAIN_Q15,
            normal_high_q15: UNITY_GAIN_Q15,
            ducked_main_q15: 4_095,
            ducked_alternate_q15: UNITY_GAIN_Q15,
            ducked_high_q15: UNITY_GAIN_Q15,
        }
    }

    pub fn validate(self) -> Result<Self> {
        if [
            self.normal_main_q15,
            self.normal_alternate_q15,
            self.normal_high_q15,
            self.ducked_main_q15,
            self.ducked_alternate_q15,
            self.ducked_high_q15,
        ]
        .into_iter()
        .any(|gain| gain > UNITY_GAIN_Q15)
        {
            return Err(Error::InvalidAudio(
                "mixer gain policy contains a gain above unity".to_owned(),
            ));
        }
        Ok(self)
    }

    pub const fn gain(self, stream: AudioStreamId, ducked: bool) -> u16 {
        match (stream, ducked) {
            (AudioStreamId::Main, false) => self.normal_main_q15,
            (AudioStreamId::Alternate, false) => self.normal_alternate_q15,
            (AudioStreamId::High, false) => self.normal_high_q15,
            (AudioStreamId::Main, true) => self.ducked_main_q15,
            (AudioStreamId::Alternate, true) => self.ducked_alternate_q15,
            (AudioStreamId::High, true) => self.ducked_high_q15,
        }
    }
}

impl Default for AudioMixPolicy {
    fn default() -> Self {
        Self {
            normal_main_q15: 24_575,
            normal_alternate_q15: 4_096,
            normal_high_q15: 4_096,
            ducked_main_q15: 4_095,
            ducked_alternate_q15: 14_336,
            ducked_high_q15: 14_336,
        }
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioGainRamp {
    start_q15: u16,
    target_q15: u16,
    duration: Duration,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
impl AudioGainRamp {
    pub(crate) fn new(start_q15: u16, target_q15: u16, duration: Duration) -> Result<Self> {
        if start_q15 > UNITY_GAIN_Q15 || target_q15 > UNITY_GAIN_Q15 {
            return Err(Error::InvalidAudio(
                "audio gain ramp endpoint exceeds unity".to_owned(),
            ));
        }
        Ok(Self {
            start_q15,
            target_q15,
            duration,
        })
    }

    pub(crate) const fn target(self) -> u16 {
        self.target_q15
    }

    pub(crate) fn is_complete(self, elapsed: Duration) -> bool {
        self.duration.is_zero() || elapsed >= self.duration
    }

    pub(crate) fn gain_at(self, elapsed: Duration) -> u16 {
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.target_q15;
        }
        let duration = self.duration.as_nanos() as i128;
        let elapsed = elapsed.as_nanos() as i128;
        let start = i128::from(self.start_q15);
        let delta = i128::from(self.target_q15) - start;
        let scaled = delta * elapsed;
        let rounded = if scaled >= 0 {
            (scaled + duration / 2) / duration
        } else {
            (scaled - duration / 2) / duration
        };
        (start + rounded).clamp(0, i128::from(UNITY_GAIN_Q15)) as u16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioLatencyConfig {
    pub opus_jitter_ms: u32,
    pub aac_jitter_ms: u32,
    pub sink_buffer_micros: u32,
    pub sink_period_micros: u32,
}

impl AudioLatencyConfig {
    pub fn validate(self) -> Result<Self> {
        if !(20..=2_000).contains(&self.opus_jitter_ms)
            || !(100..=5_000).contains(&self.aac_jitter_ms)
            || !(20_000..=500_000).contains(&self.sink_buffer_micros)
            || !(1_000..self.sink_buffer_micros).contains(&self.sink_period_micros)
        {
            return Err(Error::InvalidAudio(
                "audio latency configuration is outside bounded target ranges".to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn output_latency_micros(self) -> u32 {
        MIXER_TEST_TONE_SAMPLES * 1_000_000 / MIXER_SAMPLE_RATE_HZ
            + MIXER_OUTPUT_BUFFER_MICROS
            + self.sink_buffer_micros
            + 2 * self.sink_period_micros
    }

    pub fn effective_playout_latency_ms(
        self,
        format_id: AudioFormatId,
        negotiated_latency_ms: Option<u32>,
    ) -> Result<u32> {
        let configured = self.validate()?;
        let negotiated = negotiated_latency_ms.filter(|latency| *latency != 0);
        match format_id.codec()? {
            EncodedAudioCodec::PcmS16Be { .. } => {
                let latency = negotiated.unwrap_or(0);
                if latency > 5_000 {
                    return Err(Error::InvalidAudio(
                        "negotiated PCM playout latency exceeds 5 seconds".to_owned(),
                    ));
                }
                Ok(latency)
            }
            EncodedAudioCodec::Opus { .. } => {
                let latency = negotiated.unwrap_or(configured.opus_jitter_ms);
                if !(20..=2_000).contains(&latency) {
                    return Err(Error::InvalidAudio(
                        "negotiated Opus playout latency is outside 20..=2000 ms".to_owned(),
                    ));
                }
                Ok(latency)
            }
            EncodedAudioCodec::AacLc { .. } => {
                let latency = negotiated.unwrap_or(configured.aac_jitter_ms);
                if !(100..=5_000).contains(&latency) {
                    return Err(Error::InvalidAudio(
                        "negotiated AAC playout latency is outside 100..=5000 ms".to_owned(),
                    ));
                }
                Ok(latency)
            }
        }
    }
}

impl Default for AudioLatencyConfig {
    fn default() -> Self {
        Self {
            opus_jitter_ms: 100,
            aac_jitter_ms: 1_000,
            sink_buffer_micros: 100_000,
            sink_period_micros: 20_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioLatencySnapshot {
    pub configured: AudioLatencyConfig,
    pub gstreamer_query_live: Option<bool>,
    pub gstreamer_query_minimum_ns: Option<u64>,
    pub gstreamer_query_maximum_ns: Option<u64>,
}

impl AudioLatencySnapshot {
    pub const fn unmeasured(configured: AudioLatencyConfig) -> Self {
        Self {
            configured,
            gstreamer_query_live: None,
            gstreamer_query_minimum_ns: None,
            gstreamer_query_maximum_ns: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioMixerPlan {
    pub streams: Vec<AudioStreamPlan>,
    pub mixer: PipelineElementPlan,
    pub output_caps: CapsPlan,
    pub sink: PipelineElementPlan,
    pub mix_policy: AudioMixPolicy,
    pub latency: AudioLatencyConfig,
    pub sink_clock_synchronized: bool,
}

impl AudioMixerPlan {
    pub fn new(streams: &[(AudioStreamId, AudioFormatId)]) -> Result<Self> {
        Self::with_options(
            streams,
            AudioMixPolicy::default(),
            AudioLatencyConfig::default(),
        )
    }

    pub fn with_options(
        streams: &[(AudioStreamId, AudioFormatId)],
        mix_policy: AudioMixPolicy,
        latency: AudioLatencyConfig,
    ) -> Result<Self> {
        let streams = streams
            .iter()
            .map(|&(stream_id, format_id)| AudioStreamConfig::new(stream_id, format_id))
            .collect::<Vec<_>>();
        Self::with_stream_options(&streams, mix_policy, latency)
    }

    pub fn with_stream_options(
        streams: &[AudioStreamConfig],
        mix_policy: AudioMixPolicy,
        latency: AudioLatencyConfig,
    ) -> Result<Self> {
        if streams.is_empty() || streams.len() > MAXIMUM_AUDIO_STREAMS {
            return Err(Error::InvalidAudio(
                "audio mixer requires one to three streams".to_owned(),
            ));
        }
        let mut identifiers = BTreeSet::new();
        let mut plans = Vec::with_capacity(streams.len());
        for &stream in streams {
            if !identifiers.insert(stream.stream_id) {
                return Err(Error::InvalidAudio(
                    "audio mixer stream type is duplicated".to_owned(),
                ));
            }
            plans.push(AudioStreamPlan::with_latency(stream, latency)?);
        }
        let mix_policy = mix_policy.validate()?;
        let latency = latency.validate()?;
        Ok(Self {
            streams: plans,
            mixer: PipelineElementPlan {
                factory: "audiomixer",
                name: "carplay-mixer",
            },
            output_caps: CapsPlan {
                media_type: "audio/x-raw",
                fields: vec![
                    ("format", CapsValue::String("S16LE".to_owned())),
                    ("layout", CapsValue::String("interleaved".to_owned())),
                    ("rate", CapsValue::Signed(48_000)),
                    ("channels", CapsValue::Signed(2)),
                ],
            },
            sink: PipelineElementPlan {
                factory: "alsasink",
                name: "usb-audio-sink",
            },
            mix_policy,
            latency,
            sink_clock_synchronized: true,
        })
    }

    pub fn stream(&self, id: AudioStreamId) -> Option<&AudioStreamPlan> {
        self.streams.iter().find(|stream| stream.stream_id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophonePipelinePlan {
    pub source_caps: CapsPlan,
    pub packetizer_input_caps: CapsPlan,
    pub elements: Vec<PipelineElementPlan>,
    pub opus_bitrate: Option<u32>,
}

impl MicrophonePipelinePlan {
    pub fn new(config: MicrophoneConfig) -> Result<Self> {
        let config = config.validate()?;
        if config.channels != 1 {
            return Err(Error::Unsupported(
                "the Superbird microphone backend negotiates mono uplink only",
            ));
        }
        let mut elements = vec![
            PipelineElementPlan {
                factory: "alsasrc",
                name: "built-in-microphone",
            },
            PipelineElementPlan {
                factory: "queue",
                name: "microphone-capture-queue",
            },
            PipelineElementPlan {
                factory: "audioconvert",
                name: "microphone-downmixer",
            },
            PipelineElementPlan {
                factory: "audioresample",
                name: "microphone-resampler",
            },
        ];
        let opus_bitrate = if config.codec == MicrophoneCodec::Opus {
            elements.extend([
                PipelineElementPlan {
                    factory: "opusenc",
                    name: "microphone-opus-encoder",
                },
                PipelineElementPlan {
                    factory: "appsink",
                    name: "microphone-opus-sink",
                },
            ]);
            Some(match config.sample_rate_hz {
                16_000 | 24_000 => 48_000,
                32_000 => 64_000,
                48_000 => 96_000,
                _ => {
                    return Err(Error::Unsupported(
                        "negotiated Opus microphone rate has no target bitrate",
                    ));
                }
            })
        } else {
            elements.push(PipelineElementPlan {
                factory: "appsink",
                name: "microphone-pcm-sink",
            });
            None
        };
        Ok(Self {
            source_caps: CapsPlan {
                media_type: "audio/x-raw",
                fields: vec![
                    ("format", CapsValue::String("S32LE".to_owned())),
                    ("layout", CapsValue::String("interleaved".to_owned())),
                    ("rate", CapsValue::Signed(48_000)),
                    ("channels", CapsValue::Signed(4)),
                ],
            },
            packetizer_input_caps: CapsPlan {
                media_type: "audio/x-raw",
                fields: vec![
                    ("format", CapsValue::String("S16LE".to_owned())),
                    ("layout", CapsValue::String("interleaved".to_owned())),
                    (
                        "rate",
                        CapsValue::Signed(rate_as_i32(config.sample_rate_hz)?),
                    ),
                    ("channels", CapsValue::Signed(1)),
                ],
            },
            elements,
            opus_bitrate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_configuration() -> VideoCodecConfiguration {
        VideoCodecConfiguration {
            codec: WireVideoCodec::H264,
            codec_data: vec![1, 100, 0, 31, 0xff, 0xe1, 0, 1, 0x67],
            nal_length_bytes: 4,
        }
    }

    fn frame(codec: EncodedAudioCodec, format_id: AudioFormatId) -> EncryptedAudioFrame {
        let payload = vec![0x11, 0x22, 0x33, 0x44];
        let mut rtp_packet = vec![0x80, 100, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        rtp_packet.extend_from_slice(&payload);
        EncryptedAudioFrame {
            stream_id: AudioStreamId::Main,
            format_id,
            codec,
            sequence: 1,
            timestamp: 2,
            ssrc: 3,
            nonce_counter: 0,
            payload,
            rtp_packet,
        }
    }

    #[test]
    fn video_plan_is_fixed_to_target_stateful_decoder_and_avc_caps() {
        let plan = VideoPipelinePlan::h264(&h264_configuration()).expect("plan");
        assert_eq!(plan.decoder_device, "/dev/video1");
        assert_eq!(plan.decoder_driver_name, "meson-video-decoder");
        assert_eq!(plan.elements[2].factory, "queue");
        assert_eq!(plan.elements[3].factory, "v4l2h264dec");
        assert_eq!(
            plan.input_caps.field("stream-format"),
            Some(&CapsValue::String("avc".to_owned()))
        );
        assert_eq!(
            plan.output_caps.field("format"),
            Some(&CapsValue::String("NV12".to_owned()))
        );
        assert!(
            plan.elements
                .iter()
                .all(|element| element.factory != "videoconvert")
        );
        assert_eq!(
            plan.elements
                .iter()
                .filter(|element| element.factory == "v4l2h264dec")
                .count(),
            1
        );
    }

    #[test]
    fn video_plan_rejects_hevc_without_fallback() {
        let mut configuration = h264_configuration();
        configuration.codec = WireVideoCodec::H265;
        assert!(matches!(
            VideoPipelinePlan::h264(&configuration),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn all_three_audio_streams_share_a_real_mixer() {
        let plan = AudioMixerPlan::new(&[
            (AudioStreamId::Main, AudioFormatId::new(0x8000)),
            (AudioStreamId::Alternate, AudioFormatId::new(0x4000_0000)),
            (AudioStreamId::High, AudioFormatId::new(0x80_0000)),
        ])
        .expect("plan");
        assert_eq!(plan.streams.len(), 3);
        assert_eq!(plan.mixer.factory, "audiomixer");
        assert_eq!(plan.sink.factory, "alsasink");
        assert!(plan.sink_clock_synchronized);
        assert_eq!(
            u32::from(plan.mix_policy.normal_main_q15)
                + u32::from(plan.mix_policy.normal_alternate_q15)
                + u32::from(plan.mix_policy.normal_high_q15),
            u32::from(UNITY_GAIN_Q15)
        );
        assert!(plan.stream(AudioStreamId::Alternate).is_some());
        assert!(plan.stream(AudioStreamId::High).is_some());
        assert_eq!(
            plan.stream(AudioStreamId::Alternate)
                .expect("alternate")
                .jitter_latency_ms,
            Some(100)
        );
        assert_eq!(
            plan.stream(AudioStreamId::High)
                .expect("high")
                .jitter_latency_ms,
            Some(1_000)
        );
    }

    #[test]
    fn negotiated_aac_latency_drives_the_actual_jitter_plan() {
        let stream = AudioStreamConfig::with_negotiated_latency(
            AudioStreamId::High,
            AudioFormatId::new(0x80_0000),
            Some(730),
        );
        let plan = AudioMixerPlan::with_stream_options(
            &[stream],
            AudioMixPolicy::default(),
            AudioLatencyConfig::default(),
        )
        .expect("plan");
        let high = plan.stream(AudioStreamId::High).expect("high stream");
        assert_eq!(high.jitter_latency_ms, Some(730));
        assert_eq!(high.playout_latency_ms, 730);
    }

    #[test]
    fn absent_or_zero_negotiated_latency_uses_codec_default_and_bounds_are_fail_closed() {
        let latency = AudioLatencyConfig::default();
        assert_eq!(
            latency
                .effective_playout_latency_ms(AudioFormatId::new(0x80_0000), None)
                .expect("AAC default"),
            1_000
        );
        assert_eq!(
            latency
                .effective_playout_latency_ms(AudioFormatId::new(0x4000_0000), Some(0))
                .expect("Opus default"),
            100
        );
        assert!(
            latency
                .effective_playout_latency_ms(AudioFormatId::new(0x80_0000), Some(5_001))
                .is_err()
        );
    }

    #[test]
    fn aac_caps_and_packet_match_livi_rfc3640_adaptation() {
        let format = AudioFormatId::new(0x80_0000);
        let plan = AudioStreamPlan::new(AudioStreamId::Main, format).expect("plan");
        assert_eq!(
            plan.input_caps.field("config"),
            Some(&CapsValue::String("1190".to_owned()))
        );
        let codec = format.codec().expect("codec");
        let adapted = plan.adapt_frame(&frame(codec, format)).expect("adapt");
        assert_eq!(&adapted[..12], &[0x80, 0xe4, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(&adapted[12..16], &[0, 16, 0, 32]);
        assert_eq!(&adapted[16..], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn default_output_latency_matches_the_measured_carplay_mixer() {
        assert_eq!(
            AudioLatencyConfig::default().output_latency_micros(),
            171_333
        );
    }

    #[test]
    fn opus_branches_use_the_48k_rtp_clock_that_rtpopusdepay_accepts() {
        for (format, timestamp) in [
            (0x1000_0000, [0, 0, 0, 6]),
            (0x2000_0000, [0, 0, 0, 4]),
            (0x4000_0000, [0, 0, 0, 2]),
        ] {
            let format = AudioFormatId::new(format);
            let plan = AudioStreamPlan::new(AudioStreamId::Main, format).expect("plan");
            assert_eq!(
                plan.input_caps.field("clock-rate"),
                Some(&CapsValue::Signed(48_000))
            );
            let codec = format.codec().expect("codec");
            let adapted = plan.adapt_frame(&frame(codec, format)).expect("adapt");
            assert_eq!(&adapted[4..8], &timestamp);
            assert_eq!(&adapted[12..], &[0x11, 0x22, 0x33, 0x44]);
        }
    }

    #[test]
    fn rescaled_opus_timestamps_stay_continuous_across_wraparound() {
        let format = AudioFormatId::new(0x1000_0000);
        let codec = format.codec().expect("codec");
        let plan = AudioStreamPlan::new(AudioStreamId::Main, format).expect("plan");
        let scaled = |timestamp: u32| {
            let mut wire = frame(codec, format);
            wire.rtp_packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
            let adapted = plan.adapt_frame(&wire).expect("adapt");
            u32::from_be_bytes([adapted[4], adapted[5], adapted[6], adapted[7]])
        };
        assert_eq!(scaled(u32::MAX - 159).wrapping_add(960), scaled(160));
    }

    #[test]
    fn pcm_branch_accepts_payload_not_rtp() {
        let format = AudioFormatId::new(0x8000);
        let codec = format.codec().expect("codec");
        let plan = AudioStreamPlan::new(AudioStreamId::Main, format).expect("plan");
        assert_eq!(
            plan.adapt_frame(&frame(codec, format)).expect("adapt"),
            [0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn microphone_plan_gates_superbird_capture_and_encoder() {
        let config = MicrophoneConfig {
            stream_id: AudioStreamId::Main,
            codec: MicrophoneCodec::Opus,
            sample_rate_hz: 48_000,
            channels: 1,
            frame_duration_micros: 20_000,
            maximum_encoded_frame_bytes: 4096,
            maximum_pcm_buffer_bytes: 8192,
        };
        let plan = MicrophonePipelinePlan::new(config).expect("plan");
        assert_eq!(plan.opus_bitrate, Some(96_000));
        assert_eq!(
            plan.source_caps.field("channels"),
            Some(&CapsValue::Signed(4))
        );
        assert!(
            plan.elements
                .iter()
                .any(|element| element.factory == "opusenc")
        );
    }

    #[test]
    fn invalid_gain_and_latency_policies_are_rejected() {
        let invalid_gain = AudioMixPolicy {
            normal_main_q15: u16::MAX,
            ..AudioMixPolicy::default()
        };
        assert!(invalid_gain.validate().is_err());
        let invalid_latency = AudioLatencyConfig {
            sink_period_micros: 100_000,
            sink_buffer_micros: 100_000,
            ..AudioLatencyConfig::default()
        };
        assert!(invalid_latency.validate().is_err());
    }

    #[test]
    fn carplay_policy_keeps_prompts_at_unity_while_ducking_only_main() {
        let policy = AudioMixPolicy::carplay()
            .validate()
            .expect("CarPlay policy");
        assert_eq!(policy.normal_main_q15, UNITY_GAIN_Q15);
        assert_eq!(policy.normal_alternate_q15, UNITY_GAIN_Q15);
        assert_eq!(policy.normal_high_q15, UNITY_GAIN_Q15);
        assert_eq!(policy.ducked_alternate_q15, UNITY_GAIN_Q15);
        assert_eq!(policy.ducked_high_q15, UNITY_GAIN_Q15);
        assert!(policy.ducked_main_q15 < UNITY_GAIN_Q15);
    }

    #[test]
    fn default_policy_preserves_android_auto_mix_headroom() {
        let policy = AudioMixPolicy::default();
        assert_eq!(policy.normal_main_q15, 24_575);
        assert_eq!(policy.normal_alternate_q15, 4_096);
        assert_eq!(policy.normal_high_q15, 4_096);
        assert_eq!(policy.ducked_main_q15, 4_095);
        assert_eq!(policy.ducked_alternate_q15, 14_336);
        assert_eq!(policy.ducked_high_q15, 14_336);
    }

    #[test]
    fn gain_ramp_is_monotonic_and_reaches_exact_endpoints() {
        let attack = AudioGainRamp::new(UNITY_GAIN_Q15, 3_277, Duration::from_millis(200))
            .expect("attack ramp");
        assert_eq!(attack.gain_at(Duration::ZERO), UNITY_GAIN_Q15);
        let quarter = attack.gain_at(Duration::from_millis(50));
        let half = attack.gain_at(Duration::from_millis(100));
        let three_quarters = attack.gain_at(Duration::from_millis(150));
        assert!(UNITY_GAIN_Q15 > quarter);
        assert!(quarter > half);
        assert!(half > three_quarters);
        assert!(three_quarters > attack.target());
        assert!(!attack.is_complete(Duration::from_millis(199)));
        assert!(attack.is_complete(Duration::from_millis(200)));
        assert_eq!(attack.gain_at(Duration::from_millis(200)), 3_277);
        assert_eq!(attack.gain_at(Duration::from_secs(1)), 3_277);

        let release = AudioGainRamp::new(half, UNITY_GAIN_Q15, Duration::from_millis(300))
            .expect("reversed release ramp");
        assert_eq!(release.gain_at(Duration::ZERO), half);
        assert!(release.gain_at(Duration::from_millis(150)) > half);
        assert_eq!(release.gain_at(Duration::from_millis(300)), UNITY_GAIN_Q15);
    }

    #[test]
    fn zero_duration_gain_ramp_is_immediate() {
        let ramp = AudioGainRamp::new(UNITY_GAIN_Q15, 0, Duration::ZERO).expect("instant ramp");
        assert_eq!(ramp.gain_at(Duration::ZERO), 0);
    }
}
