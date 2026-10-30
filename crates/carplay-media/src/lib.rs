#![forbid(unsafe_code)]

mod audio;
mod audio_wire;
mod backend_plan;
mod backpressure;
mod device_discovery;
#[cfg(all(target_os = "linux", feature = "linux-backend"))]
mod display_runtime;
mod echo_canceller;
mod equalizer;
mod error;
#[cfg(all(target_os = "linux", feature = "linux-backend"))]
mod linux_backend;
mod microphone_wire;
#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
mod pipeline_state;
mod presentation_timing;
mod screen_wire;
mod session;
mod timing_wire;
mod video;
#[cfg(all(target_os = "linux", feature = "linux-backend"))]
mod wayland_presenter;
mod wire_crypto;
#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
mod worker_lifecycle;

pub use audio::{
    AudioMixer, AudioPacket, AudioSink, MicrophoneSource, PcmFormat, UnsupportedAudioSink,
};
pub use audio_wire::{
    AIRPLAY_AUDIO_TAIL_LENGTH, AudioFormatId, AudioStreamId, AudioWireDecoder, AudioWireEvent,
    AudioWireLimits, AudioWireState, EncodedAudioCodec, EncryptedAudioFrame, MediaClockOrigin,
    RTP_HEADER_LENGTH,
};
pub use backend_plan::{
    AudioLatencyConfig, AudioLatencySnapshot, AudioMixPolicy, AudioMixerPlan, AudioStreamConfig,
    AudioStreamPlan, CapsPlan, CapsValue, DEFAULT_MICROPHONE_FRAME_MICROS, MAXIMUM_AUDIO_STREAMS,
    MAXIMUM_BACKEND_FRAME_BYTES, MAXIMUM_CODEC_DATA_BYTES, MAXIMUM_RTP_PACKET_BYTES,
    MICROPHONE_CAPTURE_PERIOD_MICROS, MIXER_OUTPUT_BUFFER_MICROS, MIXER_TEST_TONE_SAMPLES,
    MicrophonePipelinePlan, PipelineElementPlan, SCREEN_FRAMES_PER_SECOND, SCREEN_HEIGHT,
    SCREEN_WIDTH, TARGET_VIDEO_DEVICE, TARGET_VIDEO_DRIVER_NAME, UNITY_GAIN_Q15, VideoPipelinePlan,
};
pub use backpressure::{
    VideoBackpressureConfig, VideoBackpressureStats, VideoFrameDisposition, VideoIngressBuffer,
};
pub use device_discovery::{
    AlsaEndpoint, BUILT_IN_MICROPHONE_CARD_ID, BUILT_IN_MICROPHONE_DEVICE,
    TARGET_VIDEO_DRIVER_ATTRIBUTE, UsbDacMatcher, resolve_builtin_microphone, resolve_usb_dac,
    revalidate_usb_dac, validate_video_decoder_target,
};
#[cfg(all(target_os = "linux", feature = "linux-backend"))]
pub use display_runtime::{LinuxDisplayHandle, LinuxDisplayRuntime};
pub use echo_canceller::{EchoCanceller, EchoCancellerSummary, EchoReference};
pub use equalizer::{
    EQUALIZER_BANDS, EQUALIZER_FREQUENCIES_HZ, EqualizerAvailability, EqualizerControl,
    EqualizerSettings, MAX_BAND_HALF_DB, MAX_PREAMP_HALF_DB, MIN_BAND_HALF_DB, MIN_PREAMP_HALF_DB,
    db_to_linear, half_db_to_db,
};
pub use error::{Error, Result};
#[cfg(all(target_os = "linux", feature = "linux-backend"))]
pub use linux_backend::{
    AudioBackendFailureKind, AudioBackendTelemetrySnapshot, AudioFrameSender, AudioOutputRoute,
    AudioSinkStats, BackendFailure, LinuxAudioBackend, LinuxAudioBackendConfig,
    LinuxMicrophoneBackend, LinuxMicrophoneBackendConfig, LinuxVideoBackend,
    LinuxVideoBackendConfig, MicrophoneBackendEvent, MicrophoneCaptureLevel, MicrophoneInputRoute,
    VideoBackendEvent, VideoSynchronizationTelemetry, VideoSynchronizationTelemetrySnapshot,
};
pub use microphone_wire::{
    MicrophoneCodec, MicrophoneConfig, MicrophoneCounters, MicrophonePacket, MicrophonePacketizer,
    MicrophonePacketizerState,
};
pub use presentation_timing::{
    AudioOutputDelay, AudioOutputDelaySource, MAXIMUM_AUDIO_OUTPUT_DELAY_MS,
};
pub use screen_wire::{
    AIRPLAY_SCREEN_HEADER_LENGTH, DEFAULT_MAXIMUM_SCREEN_BODY, EncryptedVideoFrame,
    NalUnitDescriptor, ScreenControlMessage, ScreenWireDecoder, ScreenWireEvent, ScreenWireLimits,
    ScreenWireState, VideoCodecConfiguration, WireVideoCodec, detect_video_configuration,
};
pub use session::{MediaEvent, MediaSession, MediaState};
pub use timing_wire::{
    NtpTimestamp, TIMING_PACKET_LENGTH, TIMING_REQUEST_TYPE, TIMING_RESPONSE_TYPE, TimingCodec,
    TimingEvent, TimingPacket, TimingPacketKind, TimingSample, TimingState,
};
pub use video::{
    DecodedVideoFrame, EncodedVideoFrame, H264AccessUnit, UnsupportedVideoDecoder,
    UnsupportedVideoRenderer, VideoDecoder, VideoFormat, VideoRenderer,
};
