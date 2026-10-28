#![cfg(all(target_os = "linux", feature = "linux-media"))]

use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read},
    net::{SocketAddr, SocketAddrV6, TcpListener, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use prelude_media::{
    AudioBackendFailureKind, AudioBackendTelemetrySnapshot, AudioFormatId, AudioLatencyConfig,
    AudioMixPolicy, AudioOutputDelay, AudioOutputDelaySource, AudioOutputRoute, AudioStreamConfig,
    AudioStreamId, AudioWireDecoder, AudioWireEvent, AudioWireLimits, EncryptedAudioFrame,
    Error as MediaError, LinuxAudioBackend, LinuxAudioBackendConfig, LinuxDisplayHandle,
    LinuxMicrophoneBackend, LinuxMicrophoneBackendConfig, LinuxVideoBackend,
    LinuxVideoBackendConfig, MicrophoneBackendEvent, MicrophoneCodec, MicrophoneConfig,
    MicrophoneInputRoute, MicrophonePacketizer, NtpTimestamp, ScreenWireDecoder, ScreenWireEvent,
    ScreenWireLimits, TimingCodec, TimingEvent, UsbDacMatcher, VideoBackendEvent,
    VideoSynchronizationTelemetry, VideoSynchronizationTelemetrySnapshot,
};
use prelude_net::{
    AudioDescriptor, AudioFeedbackAnchor, AudioFeedbackStream, AudioStreamKind, FlushPoint,
    StreamDescriptor, StreamKey, TimingDescriptor,
};
use prelude_usb_carkit::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    AUDIO_RUNTIME_STATUS_VERSION, AudioBackend, AudioBackendStatus, AudioBaseSinkStatus,
    AudioFailureKind, AudioLatencyStatus, AudioOutputDelaySourceStatus, AudioPipelineState,
    AudioRuntimeRoute, AudioRuntimeSnapshot, AudioStreamLatencyStatus, MediaConfig,
    MicrophoneRuntimeStatus, PlatformConfig, UsbDacConfig, VideoSynchronizationStatus,
    airplay_runtime::{EventWorkerCommand, OwnedMediaSockets},
};

#[cfg(test)]
type RecordedDuckingEvents = Arc<Mutex<Vec<(bool, Option<f64>, u32)>>>;

const COMMAND_CAPACITY: usize = 32;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAXIMUM_UDP_PACKET_BYTES: usize = 64 * 1024;
const MAXIMUM_AUDIO_CONTROL_BYTES: usize = 2 * 1024;
const MAXIMUM_AUDIO_PREROLL_BYTES: usize = 512 * 1024;
const MAXIMUM_AUDIO_FLUSH_DRAIN_PACKETS: usize = 4096;
const NTP_UNIX_EPOCH_OFFSET: u64 = 2_208_988_800;
const LOW_LATENCY_MUSIC_BUFFER_MS: u32 = 400;
const TIMING_STEP_THRESHOLD_NS: i128 = 128_000_000;
const TIMING_DELAY_WINDOW: usize = 8;
const TIMING_PICK_COUNT: u8 = 2;
const PCM_48K_STEREO_FORMAT: u64 = 0x8000;
const PERSISTENT_CARPLAY_AUDIO_STREAMS: [AudioStreamId; 2] =
    [AudioStreamId::Main, AudioStreamId::Alternate];
static NEXT_AUDIO_PIPELINE_GENERATION: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
const PCM_VOICE_FORMATS: u64 = 0x03fc;
#[cfg(test)]
const PCM_MEDIA_48K_FORMATS: u64 = 0xc000;
#[cfg(test)]
const PCM_MONO_48K_FORMATS: u64 = 0x4154;
const OPUS_48K_MONO_FORMAT: u64 = 0x4000_0000;
const AAC_LC_48K_STEREO: u64 = 0x0080_0000;
const OFFERED_FORMAT_IDS: &[(AudioStreamId, u64)] = &[
    (AudioStreamId::Main, 0x4),
    (AudioStreamId::Main, 0x8),
    (AudioStreamId::Main, 0x10),
    (AudioStreamId::Main, 0x20),
    (AudioStreamId::Main, 0x40),
    (AudioStreamId::Main, 0x80),
    (AudioStreamId::Main, 0x100),
    (AudioStreamId::Main, 0x200),
    (AudioStreamId::Main, 0x4000),
    (AudioStreamId::Main, 0x8000),
    (AudioStreamId::Main, 0x1000_0000),
    (AudioStreamId::Main, 0x2000_0000),
    (AudioStreamId::Main, OPUS_48K_MONO_FORMAT),
    (AudioStreamId::Alternate, 0x4),
    (AudioStreamId::Alternate, 0x8),
    (AudioStreamId::Alternate, 0x10),
    (AudioStreamId::Alternate, 0x20),
    (AudioStreamId::Alternate, 0x40),
    (AudioStreamId::Alternate, 0x80),
    (AudioStreamId::Alternate, 0x100),
    (AudioStreamId::Alternate, 0x200),
    (AudioStreamId::Alternate, 0x4000),
    (AudioStreamId::Alternate, 0x8000),
    (AudioStreamId::Alternate, 0x1000_0000),
    (AudioStreamId::Alternate, 0x2000_0000),
    (AudioStreamId::Alternate, OPUS_48K_MONO_FORMAT),
    (AudioStreamId::High, AAC_LC_48K_STEREO),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaCapabilities {
    pub screen: bool,
    pub audio: bool,
    pub microphone: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MediaProgress {
    pub timing_ready: bool,
    pub screen_streams: usize,
    pub audio_streams: usize,
    pub microphone_negotiated: bool,
    pub microphone_capture_running: bool,
    pub microphone_ready: bool,
    pub microphone_packets_sent: u64,
    pub microphone_bytes_sent: u64,
    pub microphone_send_failure_events: u64,
    pub microphone_rms_q15: Option<u16>,
    pub microphone_peak_q15: Option<u16>,
    pub recording: bool,
    pub muted: bool,
    pub ducked: bool,
    pub audio_pipeline_generation: u64,
    pub audio_frames_accepted: u64,
    pub audio_ingress_full_events: u64,
    pub audio_pipeline_failure_events: u64,
    pub main_pcm_level_observations: u64,
    pub main_pcm_mean_abs_q15: Option<u16>,
    pub main_pcm_peak_q15: Option<u16>,
    pub main_pcm_be_mean_channel_delta: Option<u16>,
    pub main_pcm_le_mean_abs_q15: Option<u16>,
    pub main_pcm_le_peak_q15: Option<u16>,
    pub main_pcm_le_mean_channel_delta: Option<u16>,
    pub audio_last_failure: Option<AudioFailureKind>,
}

fn retained_media_progress_after_session(progress: MediaProgress) -> MediaProgress {
    MediaProgress {
        audio_pipeline_generation: progress.audio_pipeline_generation,
        audio_frames_accepted: progress.audio_frames_accepted,
        audio_ingress_full_events: progress.audio_ingress_full_events,
        audio_pipeline_failure_events: progress.audio_pipeline_failure_events,
        main_pcm_level_observations: progress.main_pcm_level_observations,
        main_pcm_mean_abs_q15: progress.main_pcm_mean_abs_q15,
        main_pcm_peak_q15: progress.main_pcm_peak_q15,
        main_pcm_be_mean_channel_delta: progress.main_pcm_be_mean_channel_delta,
        main_pcm_le_mean_abs_q15: progress.main_pcm_le_mean_abs_q15,
        main_pcm_le_peak_q15: progress.main_pcm_le_peak_q15,
        main_pcm_le_mean_channel_delta: progress.main_pcm_le_mean_channel_delta,
        audio_last_failure: progress.audio_last_failure,
        ..MediaProgress::default()
    }
}

#[derive(Clone)]
pub(crate) struct MediaController {
    commands: SyncSender<MediaCommand>,
    feedback_clock: Arc<FeedbackClock>,
    screen_frames: Arc<AtomicU64>,
    #[cfg(test)]
    mute_toggle_attempts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    ducking_events: RecordedDuckingEvents,
}

impl std::fmt::Debug for MediaController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MediaController([bounded-channel])")
    }
}

pub(crate) struct LiveMediaRuntime {
    controller: MediaController,
    capabilities: MediaCapabilities,
    progress: Arc<Mutex<MediaProgress>>,
    failure: Arc<Mutex<Option<String>>>,
    audio: Arc<Mutex<Option<LinuxAudioBackend>>>,
    requested_audio_route: AudioRuntimeRoute,
    audio_stream_latencies: Arc<Mutex<Vec<AudioStreamLatencyStatus>>>,
    video_synchronization: VideoSynchronizationTelemetry,
    cancellation: CancellationToken,
    worker: Option<JoinHandle<Result<(), String>>>,
    audio_unavailable: Option<AudioFailureKind>,
}

impl std::fmt::Debug for LiveMediaRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveMediaRuntime")
            .field("capabilities", &self.capabilities)
            .field("progress", &self.snapshot())
            .field("has_failure", &self.failure_detail().is_some())
            .field("worker_running", &self.worker.is_some())
            .finish()
    }
}

enum MediaCommand {
    Peer(SocketAddrV6, SyncSender<Result<(), String>>),
    Timing {
        descriptor: TimingDescriptor,
        socket: UdpSocket,
        reply: SyncSender<Result<(), String>>,
    },
    Stream {
        descriptor: StreamDescriptor,
        sockets: OwnedMediaSockets,
        output_key: Zeroizing<[u8; 32]>,
        input_key: Option<Zeroizing<[u8; 32]>>,
        reply: SyncSender<Result<(), String>>,
    },
    Record(SyncSender<Result<bool, String>>),
    Duck {
        active: bool,
        main_linear_gain: Option<f64>,
        duration_ms: u32,
        reply: SyncSender<Result<(), String>>,
    },
    MuteToggle(SyncSender<Result<(), String>>),
    TestTone(SyncSender<Result<(), String>>),
    Flush(FlushPoint, SyncSender<Result<(), String>>),
    Teardown(Vec<StreamKey>, SyncSender<Result<(), String>>),
}

struct MediaSupervisor {
    commands: Receiver<MediaCommand>,
    cancellation: CancellationToken,
    poll: Duration,
    timing_interval: Duration,
    video: LinuxVideoBackendConfig,
    video_synchronization: VideoSynchronizationTelemetry,
    display: Option<LinuxDisplayHandle>,
    audio_route: Option<AudioOutputRoute>,
    equalizer: prelude_media::EqualizerControl,
    audio_output_enabled: bool,
    peer: Option<SocketAddrV6>,
    audio_descriptors: BTreeMap<StreamKey, AudioDescriptor>,
    audio_stream_generations: BTreeMap<StreamKey, u64>,
    next_audio_stream_generation: u64,
    children: BTreeMap<StreamKey, (CancellationToken, JoinHandle<Result<(), String>>)>,
    timing: Option<(CancellationToken, JoinHandle<Result<(), String>>)>,
    audio: Arc<Mutex<Option<LinuxAudioBackend>>>,
    progress: Arc<Mutex<MediaProgress>>,
    failure: Arc<Mutex<Option<String>>>,
    event_commands: SyncSender<EventWorkerCommand>,
    muted: bool,
    ducked: bool,
    ducked_main_gain: Option<f64>,
    feedback_clock: Arc<FeedbackClock>,
    screen_frames: Arc<AtomicU64>,
    recording_requested: bool,
    flush_state: Arc<Mutex<AudioFlushState>>,
    audio_stream_latencies: Arc<Mutex<Vec<AudioStreamLatencyStatus>>>,
    music_buffer_limit_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
struct AudioFlushState {
    generation: u64,
    point: Option<FlushPoint>,
}

fn audio_backend_failure_kind(kind: AudioBackendFailureKind) -> AudioFailureKind {
    match kind {
        AudioBackendFailureKind::Pipeline => AudioFailureKind::Pipeline,
        AudioBackendFailureKind::Bus => AudioFailureKind::Bus,
        AudioBackendFailureKind::EndpointRevalidation => AudioFailureKind::Endpoint,
    }
}

fn audio_backend_status(snapshot: AudioBackendTelemetrySnapshot) -> AudioBackendStatus {
    AudioBackendStatus {
        pipeline_start_events: snapshot.pipeline_start_events,
        pipeline_stop_events: snapshot.pipeline_stop_events,
        frames_pushed: snapshot.frames_pushed,
        frame_push_failure_events: snapshot.frame_push_failure_events,
        bus_poll_events: snapshot.bus_poll_events,
        bus_failure_events: snapshot.bus_failure_events,
        endpoint_revalidation_events: snapshot.endpoint_revalidation_events,
        endpoint_revalidation_failure_events: snapshot.endpoint_revalidation_failure_events,
        sink_stats_poll_events: snapshot.sink_stats_poll_events,
        base_sink: snapshot.sink_stats.map(|stats| AudioBaseSinkStatus {
            rendered_buffers: stats.rendered_buffers,
            dropped_buffers: stats.dropped_buffers,
        }),
    }
}

fn reported_audio_failure(
    runtime_failed: bool,
    progress: MediaProgress,
    backend_failure: Option<AudioFailureKind>,
) -> Option<AudioFailureKind> {
    runtime_failed.then(|| {
        progress
            .audio_last_failure
            .or(backend_failure)
            .unwrap_or(AudioFailureKind::MediaWorker)
    })
}

#[derive(Debug, Clone, Copy)]
struct FeedbackStreamState {
    stream_type: u16,
    sample_rate_hz: u32,
    connection_id: u64,
    playout_latency_ms: u32,
    origin: Option<prelude_media::MediaClockOrigin>,
}

#[derive(Debug)]
struct FeedbackClockState {
    synchronized_offset_ns: Option<i128>,
    streams: BTreeMap<StreamKey, FeedbackStreamState>,
    timing_delays_ns: [u128; TIMING_DELAY_WINDOW],
    timing_delay_index: usize,
    timing_pick_remaining: u8,
    timing_pick_rtt_ns: u128,
    timing_pick_offset_ns: i128,
}

impl Default for FeedbackClockState {
    fn default() -> Self {
        Self {
            synchronized_offset_ns: None,
            streams: BTreeMap::new(),
            timing_delays_ns: [u128::MAX; TIMING_DELAY_WINDOW],
            timing_delay_index: 0,
            timing_pick_remaining: TIMING_PICK_COUNT,
            timing_pick_rtt_ns: u128::MAX,
            timing_pick_offset_ns: 0,
        }
    }
}

#[derive(Debug)]
struct FeedbackClock {
    monotonic_origin: Instant,
    state: Mutex<FeedbackClockState>,
}

impl FeedbackClock {
    fn new() -> Self {
        Self {
            monotonic_origin: Instant::now(),
            state: Mutex::new(FeedbackClockState::default()),
        }
    }

    fn add_stream(&self, key: StreamKey, descriptor: &AudioDescriptor) -> Result<(), String> {
        let format_id = AudioFormatId::new(descriptor.audio_format);
        let sample_rate_hz = format_id
            .codec()
            .map_err(|error| format!("resolving feedback audio format failed: {error}"))?
            .sample_rate_hz();
        self.state
            .lock()
            .map_err(|_| "media feedback clock lock is poisoned".to_owned())?
            .streams
            .insert(
                key,
                FeedbackStreamState {
                    stream_type: descriptor.stream_type(),
                    sample_rate_hz,
                    connection_id: descriptor.connection_id,
                    playout_latency_ms: AudioLatencyConfig::default()
                        .effective_playout_latency_ms(format_id, descriptor.audio_latency_ms)
                        .map_err(|error| {
                            format!("resolving feedback playout latency failed: {error}")
                        })?,
                    origin: None,
                },
            );
        Ok(())
    }

    fn set_origin(
        &self,
        key: StreamKey,
        origin: prelude_media::MediaClockOrigin,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "media feedback clock lock is poisoned".to_owned())?;
        let stream = state
            .streams
            .get_mut(&key)
            .ok_or_else(|| "audio clock origin arrived for an unknown stream".to_owned())?;
        stream.origin = Some(origin);
        Ok(())
    }

    fn update_timing(&self, offset_ns: i128, round_trip_ns: u128) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "media feedback clock lock is poisoned".to_owned())?;
        if round_trip_ns < state.timing_pick_rtt_ns {
            state.timing_pick_rtt_ns = round_trip_ns;
            state.timing_pick_offset_ns = offset_ns;
        }
        state.timing_pick_remaining = state.timing_pick_remaining.saturating_sub(1);
        if state.timing_pick_remaining != 0 {
            return Ok(());
        }
        let picked_rtt = state.timing_pick_rtt_ns;
        let picked_offset = state.timing_pick_offset_ns;
        state.timing_pick_remaining = TIMING_PICK_COUNT;
        state.timing_pick_rtt_ns = u128::MAX;
        state.timing_pick_offset_ns = 0;
        let use_sample = state
            .timing_delays_ns
            .iter()
            .all(|previous| picked_rtt <= *previous);
        let index = state.timing_delay_index;
        state.timing_delays_ns[index] = picked_rtt;
        state.timing_delay_index = (index + 1) % TIMING_DELAY_WINDOW;
        if !use_sample {
            return Ok(());
        }
        let stepping = state.synchronized_offset_ns.is_none()
            || picked_offset.abs() > TIMING_STEP_THRESHOLD_NS;
        state.synchronized_offset_ns = Some(match state.synchronized_offset_ns {
            None => picked_offset,
            Some(current) if stepping => current + picked_offset,
            Some(current) => current + picked_offset / 8,
        });
        if stepping {
            state.timing_delays_ns.fill(u128::MAX);
            state.timing_delay_index = 0;
        }
        Ok(())
    }

    fn synchronized_now(&self) -> Result<NtpTimestamp, String> {
        self.synchronize_timestamp(now_ntp()?)
    }

    fn synchronize_timestamp(&self, local: NtpTimestamp) -> Result<NtpTimestamp, String> {
        let offset = self
            .state
            .lock()
            .map_err(|_| "media feedback clock lock is poisoned".to_owned())?
            .synchronized_offset_ns
            .unwrap_or(0);
        Ok(add_ntp_offset(local, offset))
    }

    fn remove_stream(&self, key: StreamKey) {
        if let Ok(mut state) = self.state.lock() {
            state.streams.remove(&key);
        }
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = FeedbackClockState::default();
        }
    }

    fn clear_origins(&self) {
        if let Ok(mut state) = self.state.lock() {
            for stream in state.streams.values_mut() {
                stream.origin = None;
            }
        }
    }

    fn snapshot(&self) -> Result<Vec<AudioFeedbackStream>, String> {
        let monotonic_nanoseconds =
            u64::try_from(self.monotonic_origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let local_ntp = now_ntp()?;
        let state = self
            .state
            .lock()
            .map_err(|_| "media feedback clock lock is poisoned".to_owned())?;
        state
            .streams
            .values()
            .map(|stream| {
                let anchor = match (stream.origin, state.synchronized_offset_ns) {
                    (Some(origin), Some(offset_ns)) => {
                        let elapsed_ns = monotonic_nanoseconds
                            .saturating_sub(origin.origin_monotonic_ns)
                            .saturating_sub(u64::from(stream.playout_latency_ms) * 1_000_000);
                        let elapsed_samples = ((u128::from(elapsed_ns)
                            * u128::from(stream.sample_rate_hz)
                            + 500_000_000)
                            / 1_000_000_000) as u64;
                        Some(AudioFeedbackAnchor {
                            connection_id: stream.connection_id,
                            synchronized_ntp: add_ntp_offset(local_ntp, offset_ns).raw(),
                            monotonic_nanoseconds,
                            sample_time: origin.first_sample.wrapping_add(elapsed_samples as u32),
                        })
                    }
                    _ => None,
                };
                Ok(AudioFeedbackStream {
                    stream_type: stream.stream_type,
                    sample_rate_hz: stream.sample_rate_hz,
                    anchor,
                })
            })
            .collect()
    }
}

impl LiveMediaRuntime {
    pub(crate) fn start(
        config: &MediaConfig,
        _platform: &PlatformConfig,
        audio_output_enabled: bool,
        event_commands: SyncSender<EventWorkerCommand>,
        display: Option<LinuxDisplayHandle>,
    ) -> Result<Self, String> {
        let preflight = preflight_audio_output(config, audio_output_enabled)?;
        let video = video_backend_config(config.video, preflight.output_delay)?;
        LinuxVideoBackend::preflight_with_display(video, display.as_ref())
            .map_err(|error| format!("video preflight failed: {error}"))?;
        let audio_route = preflight.route;
        let audio_output_available = audio_output_enabled && audio_route.is_some();
        let requested_audio_route = match (audio_output_enabled, config.audio) {
            (true, AudioBackend::AlsaUsb) => AudioRuntimeRoute::UsbDac,
            _ => AudioRuntimeRoute::Disabled,
        };

        let (commands_tx, commands_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let cancellation = CancellationToken::default();
        let progress = Arc::new(Mutex::new(MediaProgress::default()));
        let failure = Arc::new(Mutex::new(None));
        let audio = Arc::new(Mutex::new(None));
        let audio_stream_latencies = Arc::new(Mutex::new(Vec::new()));
        let video_synchronization = VideoSynchronizationTelemetry::new(video.output_delay);
        let feedback_clock = Arc::new(FeedbackClock::new());
        let screen_frames = Arc::new(AtomicU64::new(0));
        let flush_state = Arc::new(Mutex::new(AudioFlushState::default()));
        let supervisor = MediaSupervisor {
            commands: commands_rx,
            cancellation: cancellation.clone(),
            poll: Duration::from_millis(config.socket_poll_interval_ms),
            timing_interval: Duration::from_millis(config.timing_interval_ms),
            video,
            video_synchronization: video_synchronization.clone(),
            display,
            audio_route,
            equalizer: config.equalizer.clone(),
            audio_output_enabled: audio_output_available,
            peer: None,
            audio_descriptors: BTreeMap::new(),
            audio_stream_generations: BTreeMap::new(),
            next_audio_stream_generation: 1,
            children: BTreeMap::new(),
            timing: None,
            audio: Arc::clone(&audio),
            progress: Arc::clone(&progress),
            failure: Arc::clone(&failure),
            event_commands,
            muted: false,
            ducked: false,
            ducked_main_gain: None,
            feedback_clock: Arc::clone(&feedback_clock),
            screen_frames: Arc::clone(&screen_frames),
            recording_requested: false,
            flush_state,
            audio_stream_latencies: Arc::clone(&audio_stream_latencies),
            music_buffer_limit_ms: config
                .low_latency_music
                .then_some(LOW_LATENCY_MUSIC_BUFFER_MS),
        };
        let worker = thread::Builder::new()
            .name("prelude-media".to_owned())
            .spawn(move || supervisor.run())
            .map_err(|error| format!("starting media supervisor failed: {error}"))?;
        Ok(Self {
            controller: MediaController {
                commands: commands_tx,
                feedback_clock,
                screen_frames,
                #[cfg(test)]
                mute_toggle_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                ducking_events: Arc::new(Mutex::new(Vec::new())),
            },
            capabilities: MediaCapabilities {
                screen: true,
                audio: audio_output_available,
                microphone: audio_output_available,
            },
            progress,
            failure,
            audio,
            requested_audio_route,
            audio_stream_latencies,
            video_synchronization,
            cancellation,
            worker: Some(worker),
            audio_unavailable: preflight.unavailable,
        })
    }

    pub(crate) fn controller(&self) -> MediaController {
        self.controller.clone()
    }

    pub(crate) const fn capabilities(&self) -> MediaCapabilities {
        self.capabilities
    }

    pub(crate) fn audio_output_missing(&self) -> bool {
        self.audio_unavailable == Some(AudioFailureKind::Endpoint)
    }

    pub(crate) fn snapshot(&self) -> MediaProgress {
        self.progress
            .lock()
            .map_or_else(|_| MediaProgress::default(), |value| *value)
    }

    pub(crate) fn failure_detail(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|value| value.clone())
    }

    pub(crate) fn audio_status(&self) -> AudioRuntimeSnapshot {
        if self.requested_audio_route == AudioRuntimeRoute::Disabled {
            return AudioRuntimeSnapshot::disabled();
        }
        if let Some(failure) = self.audio_unavailable {
            let mut status = AudioRuntimeSnapshot::waiting(self.requested_audio_route);
            status.pipeline_state = AudioPipelineState::Failed;
            status.video_synchronization = self
                .video_synchronization
                .snapshot()
                .map(video_synchronization_status)
                .unwrap_or_else(|_| VideoSynchronizationStatus::uncompensated());
            status.last_failure = Some(failure);
            return status;
        }
        let progress = self.snapshot();
        let failure = self.failure_detail();
        let (latency, backend_telemetry) = self
            .audio
            .lock()
            .ok()
            .and_then(|audio| {
                audio
                    .as_ref()
                    .map(|audio| (audio.latency().ok(), audio.telemetry().ok()))
            })
            .unwrap_or((None, None));
        let latency = latency.map(|snapshot| AudioLatencyStatus {
            default_opus_jitter_ms: snapshot.configured.opus_jitter_ms,
            default_aac_jitter_ms: snapshot.configured.aac_jitter_ms,
            sink_buffer_micros: snapshot.configured.sink_buffer_micros,
            sink_period_micros: snapshot.configured.sink_period_micros,
            gstreamer_query_live: snapshot.gstreamer_query_live,
            gstreamer_query_minimum_ns: snapshot.gstreamer_query_minimum_ns,
            gstreamer_query_maximum_ns: snapshot.gstreamer_query_maximum_ns,
        });
        let pipeline_active = backend_telemetry.is_some();
        let backend_failure = backend_telemetry
            .and_then(|telemetry| telemetry.last_failure)
            .map(audio_backend_failure_kind);
        let backend = backend_telemetry.map(audio_backend_status);
        let pipeline_state = if failure.is_some() {
            AudioPipelineState::Failed
        } else if pipeline_active {
            AudioPipelineState::Playing
        } else if progress.audio_streams == 0 {
            AudioPipelineState::PreflightReady
        } else {
            AudioPipelineState::WaitingForStream
        };
        AudioRuntimeSnapshot {
            version: AUDIO_RUNTIME_STATUS_VERSION,
            requested_route: self.requested_audio_route,
            active_route: matches!(pipeline_state, AudioPipelineState::Playing)
                .then_some(self.requested_audio_route),
            pipeline_state,
            pipeline_generation: progress.audio_pipeline_generation,
            recording: progress.recording,
            negotiated_streams: progress.audio_streams,
            muted: progress.muted,
            ducked: progress.ducked,
            frames_accepted: progress.audio_frames_accepted,
            ingress_full_events: progress.audio_ingress_full_events,
            pipeline_failure_events: progress.audio_pipeline_failure_events,
            main_pcm_level_observations: progress.main_pcm_level_observations,
            main_pcm_mean_abs_q15: progress.main_pcm_mean_abs_q15,
            main_pcm_peak_q15: progress.main_pcm_peak_q15,
            main_pcm_be_mean_channel_delta: progress.main_pcm_be_mean_channel_delta,
            main_pcm_le_mean_abs_q15: progress.main_pcm_le_mean_abs_q15,
            main_pcm_le_peak_q15: progress.main_pcm_le_peak_q15,
            main_pcm_le_mean_channel_delta: progress.main_pcm_le_mean_channel_delta,
            backend,
            microphone: MicrophoneRuntimeStatus {
                negotiated: progress.microphone_negotiated,
                capture_running: progress.microphone_capture_running,
                ready: progress.microphone_ready,
                packets_sent: progress.microphone_packets_sent,
                bytes_sent: progress.microphone_bytes_sent,
                send_failure_events: progress.microphone_send_failure_events,
                rms_q15: progress.microphone_rms_q15,
                peak_q15: progress.microphone_peak_q15,
            },
            latency,
            stream_latencies: self
                .audio_stream_latencies
                .lock()
                .map_or_else(|_| Vec::new(), |latencies| latencies.clone()),
            video_synchronization: self
                .video_synchronization
                .snapshot()
                .map(video_synchronization_status)
                .unwrap_or_else(|_| VideoSynchronizationStatus::uncompensated()),
            last_failure: reported_audio_failure(failure.is_some(), progress, backend_failure),
        }
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && self.failure_detail().is_none()
    }

    pub(crate) fn stop(mut self) -> Result<(), String> {
        self.cancellation.cancel();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| "media supervisor panicked during shutdown".to_owned())?
    }
}

pub(crate) fn preflight_configured_audio_output(
    config: &MediaConfig,
    _platform: &PlatformConfig,
    audio_output_enabled: bool,
) -> Result<(), String> {
    let preflight = preflight_audio_output(config, audio_output_enabled)?;
    require_configured_audio_output(audio_output_enabled, preflight.route.is_some())
}

pub(crate) fn configured_audio_output_available(
    config: &MediaConfig,
    audio_output_enabled: bool,
) -> Result<bool, String> {
    Ok(preflight_audio_output(config, audio_output_enabled)?
        .route
        .is_some())
}

fn require_configured_audio_output(
    audio_output_enabled: bool,
    route_available: bool,
) -> Result<(), String> {
    if audio_output_enabled && !route_available {
        return Err(
            "audio route preflight failed: configured audio output is unavailable".to_owned(),
        );
    }
    Ok(())
}

struct AudioOutputPreflight {
    route: Option<AudioOutputRoute>,
    output_delay: AudioOutputDelay,
    unavailable: Option<AudioFailureKind>,
}

fn preflight_audio_output(
    config: &MediaConfig,
    audio_output_enabled: bool,
) -> Result<AudioOutputPreflight, String> {
    let output_delay = AudioOutputDelay::uncompensated();
    let mut unavailable = None;
    let mut route = if audio_output_enabled {
        match config.audio {
            AudioBackend::AlsaUsb => Some(AudioOutputRoute::UsbAlsa(matcher(
                config
                    .usb_dac
                    .as_ref()
                    .ok_or_else(|| "live media has no USB DAC matcher".to_owned())?,
            )?)),
            _ => return Err("live media has no supported audio route".to_owned()),
        }
    } else {
        None
    };
    if let Some(candidate) = route.as_ref() {
        let formats: Vec<_> = OFFERED_FORMAT_IDS
            .iter()
            .map(|(stream, format)| (*stream, AudioFormatId::new(*format)))
            .collect();
        let available = classify_audio_route_preflight(
            config.audio,
            LinuxAudioBackend::preflight_route_formats(candidate, &formats),
        )?;
        if available {
            LinuxMicrophoneBackend::preflight_route(
                preflight_microphone_config(MicrophoneCodec::PcmS16),
                MicrophoneInputRoute::BuiltInSuperbird,
            )
            .map_err(|error| format!("built-in microphone PCM preflight failed: {error}"))?;
            LinuxMicrophoneBackend::preflight_route(
                preflight_microphone_config(MicrophoneCodec::Opus),
                MicrophoneInputRoute::BuiltInSuperbird,
            )
            .map_err(|error| format!("built-in microphone Opus preflight failed: {error}"))?;
        } else {
            route = None;
            unavailable = Some(AudioFailureKind::Endpoint);
        }
    }
    Ok(AudioOutputPreflight {
        route,
        output_delay,
        unavailable,
    })
}

fn classify_audio_route_preflight(
    backend: AudioBackend,
    result: Result<(), MediaError>,
) -> Result<bool, String> {
    match result {
        Ok(()) => Ok(true),
        Err(MediaError::NotReady {
            component: "USB DAC",
            state: "not connected",
        }) if backend == AudioBackend::AlsaUsb => Ok(false),
        Err(error) => Err(format!("audio route preflight failed: {error}")),
    }
}

impl Drop for LiveMediaRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl MediaController {
    #[cfg(test)]
    pub(crate) fn responding_for_test() -> Self {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let ducking_events = Arc::new(Mutex::new(Vec::new()));
        let recorded_ducking_events = Arc::clone(&ducking_events);
        let _ = thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    MediaCommand::Peer(_, reply)
                    | MediaCommand::MuteToggle(reply)
                    | MediaCommand::TestTone(reply)
                    | MediaCommand::Flush(_, reply)
                    | MediaCommand::Teardown(_, reply) => {
                        let _ = reply.send(Ok(()));
                    }
                    MediaCommand::Duck {
                        active,
                        main_linear_gain,
                        duration_ms,
                        reply,
                    } => {
                        if let Ok(mut events) = recorded_ducking_events.lock() {
                            events.push((active, main_linear_gain, duration_ms));
                        }
                        let _ = reply.send(Ok(()));
                    }
                    MediaCommand::Timing { reply, .. } | MediaCommand::Stream { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    MediaCommand::Record(reply) => {
                        let _ = reply.send(Ok(true));
                    }
                }
            }
        });
        Self {
            commands,
            feedback_clock: Arc::new(FeedbackClock::new()),
            screen_frames: Arc::new(AtomicU64::new(0)),
            mute_toggle_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            ducking_events,
        }
    }

    pub(crate) fn audio_feedback(&self) -> Result<Vec<AudioFeedbackStream>, String> {
        self.feedback_clock.snapshot()
    }

    pub(crate) fn screen_frames_received(&self) -> u64 {
        self.screen_frames.load(Ordering::Acquire)
    }

    pub(crate) fn set_peer(&self, peer: SocketAddrV6) -> Result<(), String> {
        self.call(|reply| MediaCommand::Peer(peer, reply))
    }

    pub(crate) fn start_timing(
        &self,
        descriptor: TimingDescriptor,
        socket: UdpSocket,
    ) -> Result<(), String> {
        self.call(|reply| MediaCommand::Timing {
            descriptor,
            socket,
            reply,
        })
    }

    pub(crate) fn start_stream(
        &self,
        descriptor: StreamDescriptor,
        sockets: OwnedMediaSockets,
        output_key: Zeroizing<[u8; 32]>,
        input_key: Option<Zeroizing<[u8; 32]>>,
    ) -> Result<(), String> {
        self.call(|reply| MediaCommand::Stream {
            descriptor,
            sockets,
            output_key,
            input_key,
            reply,
        })
    }

    pub(crate) fn record(&self) -> Result<bool, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        match self.commands.try_send(MediaCommand::Record(reply_tx)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err("media command queue is full; request rejected".to_owned());
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("media supervisor is stopped".to_owned());
            }
        }
        reply_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "media command did not complete within its bound".to_owned())?
    }

    pub(crate) fn set_ducking(
        &self,
        active: bool,
        main_linear_gain: Option<f64>,
        duration_ms: u32,
    ) -> Result<(), String> {
        self.call(|reply| MediaCommand::Duck {
            active,
            main_linear_gain,
            duration_ms,
            reply,
        })
    }

    pub(crate) fn toggle_mute(&self) -> Result<(), String> {
        #[cfg(test)]
        self.mute_toggle_attempts.fetch_add(1, Ordering::Relaxed);
        self.call(MediaCommand::MuteToggle)
    }

    #[cfg(test)]
    pub(crate) fn mute_toggle_attempt_count(&self) -> usize {
        self.mute_toggle_attempts.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn ducking_events(&self) -> Vec<(bool, Option<f64>, u32)> {
        self.ducking_events
            .lock()
            .map_or_else(|_| Vec::new(), |events| events.clone())
    }

    pub(crate) fn play_test_tone(&self) -> Result<(), String> {
        self.call(MediaCommand::TestTone)
    }

    pub(crate) fn flush(&self, point: FlushPoint) -> Result<(), String> {
        self.call(|reply| MediaCommand::Flush(point, reply))
    }

    pub(crate) fn teardown(&self, streams: Vec<StreamKey>) -> Result<(), String> {
        self.call(|reply| MediaCommand::Teardown(streams, reply))
    }

    fn call(
        &self,
        command: impl FnOnce(SyncSender<Result<(), String>>) -> MediaCommand,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        match self.commands.try_send(command(reply_tx)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err("media command queue is full; request rejected".to_owned());
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("media supervisor is stopped".to_owned());
            }
        }
        reply_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "media command did not complete within its bound".to_owned())?
    }
}

impl MediaSupervisor {
    fn run(mut self) -> Result<(), String> {
        let result = (|| {
            while !self.cancellation.is_cancelled() {
                self.audit_children()?;
                match self.commands.recv_timeout(self.poll) {
                    Ok(command) => self.handle(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })();
        self.cancellation.cancel();
        let cleanup = self.stop_all();
        match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), cleanup) => cleanup,
        }
    }

    fn handle(&mut self, command: MediaCommand) {
        match command {
            MediaCommand::Peer(peer, reply) => {
                let result = self.set_peer(peer);
                let _ = reply.send(result);
            }
            MediaCommand::Timing {
                descriptor,
                socket,
                reply,
            } => {
                let result = self.start_timing(descriptor, socket);
                let _ = reply.send(result);
            }
            MediaCommand::Stream {
                descriptor,
                sockets,
                output_key,
                input_key,
                reply,
            } => {
                let result = self.start_stream(descriptor, sockets, output_key, input_key);
                let _ = reply.send(result);
            }
            MediaCommand::Record(reply) => {
                let result = self.start_recording();
                let _ = reply.send(result);
            }
            MediaCommand::Duck {
                active,
                main_linear_gain,
                duration_ms,
                reply,
            } => {
                let previous_ducked = self.ducked;
                let previous_main_gain = self.ducked_main_gain;
                self.ducked = active;
                self.ducked_main_gain = main_linear_gain;
                let result = self.with_audio_if_started(|audio| {
                    audio.set_ducking_gain_ramped(
                        active,
                        main_linear_gain,
                        Duration::from_millis(u64::from(duration_ms)),
                    )
                });
                if result.is_ok() {
                    self.update_progress(|progress| progress.ducked = active);
                } else {
                    self.ducked = previous_ducked;
                    self.ducked_main_gain = previous_main_gain;
                }
                let _ = reply.send(result);
            }
            MediaCommand::MuteToggle(reply) => {
                let requested = !self.muted;
                let result = self.with_audio_if_started(|audio| audio.set_muted(requested));
                if result.is_ok() {
                    self.muted = requested;
                    self.update_progress(|progress| progress.muted = requested);
                }
                let _ = reply.send(result);
            }
            MediaCommand::TestTone(reply) => {
                let result = self.with_audio_if_started(LinuxAudioBackend::play_test_tone);
                let _ = reply.send(result);
            }
            MediaCommand::Flush(point, reply) => {
                let result = self.flush_audio(point);
                let _ = reply.send(result);
            }
            MediaCommand::Teardown(streams, reply) => {
                let result = self.teardown_streams(&streams);
                let _ = reply.send(result);
            }
        }
    }

    fn set_peer(&mut self, peer: SocketAddrV6) -> Result<(), String> {
        if !peer.ip().is_unicast_link_local() || peer.scope_id() == 0 || peer.port() == 0 {
            return Err("media peer is not a scoped link-local control peer".to_owned());
        }
        match self.peer {
            Some(existing) if !same_scoped_peer(existing, peer) => {
                Err("media peer changed after control acceptance".to_owned())
            }
            Some(_) => Ok(()),
            None => {
                self.peer = Some(peer);
                Ok(())
            }
        }
    }

    fn start_timing(
        &mut self,
        descriptor: TimingDescriptor,
        socket: UdpSocket,
    ) -> Result<(), String> {
        if self.timing.is_some() {
            return Err("timing worker was already started".to_owned());
        }
        let peer = self
            .peer
            .ok_or_else(|| "control peer is unavailable".to_owned())?;
        let peer_port = descriptor
            .peer_timing_port
            .ok_or_else(|| "phone SETUP omitted timingPort".to_owned())?;
        validate_local_scope(&socket, peer.scope_id())?;
        socket
            .set_nonblocking(false)
            .map_err(|error| format!("configuring blocking timing I/O failed: {error}"))?;
        socket
            .set_read_timeout(Some(self.poll))
            .map_err(|error| format!("setting timing timeout failed: {error}"))?;
        let cancellation = CancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let interval = self.timing_interval;
        let feedback_clock = Arc::clone(&self.feedback_clock);
        let worker = thread::Builder::new()
            .name("prelude-timing".to_owned())
            .spawn(move || {
                timing_worker(
                    socket,
                    SocketAddrV6::new(*peer.ip(), peer_port.get(), 0, peer.scope_id()),
                    interval,
                    thread_cancellation,
                    feedback_clock,
                )
            })
            .map_err(|error| format!("starting timing worker failed: {error}"))?;
        self.timing = Some((cancellation, worker));
        self.update_progress(|progress| progress.timing_ready = true);
        Ok(())
    }

    fn allocate_audio_stream_generation(&mut self) -> Result<u64, String> {
        let generation = self.next_audio_stream_generation;
        self.next_audio_stream_generation = generation
            .checked_add(1)
            .ok_or_else(|| "audio stream generation counter exhausted".to_owned())?;
        Ok(generation)
    }

    fn start_stream(
        &mut self,
        descriptor: StreamDescriptor,
        sockets: OwnedMediaSockets,
        output_key: Zeroizing<[u8; 32]>,
        input_key: Option<Zeroizing<[u8; 32]>>,
    ) -> Result<(), String> {
        let descriptor = limit_music_buffer(descriptor, self.music_buffer_limit_ms);
        let key = descriptor.key();
        if self.children.contains_key(&key) {
            return Err("media stream worker was already started".to_owned());
        }
        if !matches!(
            (&descriptor, &sockets),
            (StreamDescriptor::Screen(_), OwnedMediaSockets::Screen(_))
                | (StreamDescriptor::Audio(_), OwnedMediaSockets::Audio { .. })
        ) {
            return Err("negotiated stream and retained socket types differ".to_owned());
        }
        let peer = self
            .peer
            .ok_or_else(|| "control peer is unavailable".to_owned())?;
        let cancellation = CancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let poll = self.poll;
        let video = self.video;
        let video_synchronization = self.video_synchronization.clone();
        let display = self.display.clone();
        let screen_frames = Arc::clone(&self.screen_frames);
        let audio = Arc::clone(&self.audio);
        let progress = Arc::clone(&self.progress);
        let event_commands = self.event_commands.clone();
        let is_screen = matches!(descriptor, StreamDescriptor::Screen(_));
        let microphone_expected = matches!(
            &descriptor,
            StreamDescriptor::Audio(audio) if audio.microphone.is_some()
        );
        let mut recording_reactivated = false;
        let mut audio_generation = None;
        if let StreamDescriptor::Audio(audio_descriptor) = &descriptor {
            validate_audio_descriptor(audio_descriptor)?;
            let stream_id = audio_stream_id(audio_descriptor.kind);
            if self
                .audio_descriptors
                .values()
                .any(|active| audio_stream_id(active.kind) == stream_id)
            {
                return Err(format!(
                    "CarPlay {stream_id:?} already has an active stream"
                ));
            }
            let generation = self.allocate_audio_stream_generation()?;
            self.feedback_clock.add_stream(key, audio_descriptor)?;
            self.audio_descriptors.insert(key, audio_descriptor.clone());
            self.audio_stream_generations.insert(key, generation);
            audio_generation = Some(generation);
            if let Err(error) = self.refresh_audio_stream_latencies() {
                self.feedback_clock.remove_stream(key);
                self.audio_descriptors.remove(&key);
                self.audio_stream_generations.remove(&key);
                return Err(error);
            }
            if self.recording_requested {
                let backend_started = self
                    .audio
                    .lock()
                    .map_err(|_| "audio backend lock is poisoned".to_owned())?
                    .is_some();
                let configured = if backend_started {
                    let stream = audio_stream_config(audio_descriptor);
                    self.with_audio_if_started(|audio| audio.configure_stream(stream, generation))
                        .or_else(|error| {
                            tracing::info!(
                                error,
                                stream = ?stream.stream_id,
                                "rebuilding the audio mixer for a new stream layout"
                            );
                            self.stop_audio_backend()?;
                            self.start_audio_backend()
                        })
                } else {
                    self.start_audio_backend()
                };
                if let Err(error) = configured {
                    self.feedback_clock.remove_stream(key);
                    self.audio_descriptors.remove(&key);
                    self.audio_stream_generations.remove(&key);
                    self.refresh_audio_stream_latencies()?;
                    return Err(error);
                }
                recording_reactivated = true;
            }
        }
        let worker = match (descriptor, sockets) {
            (StreamDescriptor::Screen(_), OwnedMediaSockets::Screen(listener)) => {
                thread::Builder::new()
                    .name("prelude-screen".to_owned())
                    .spawn(move || {
                        let result = screen_worker(ScreenWorkerContext {
                            listener,
                            peer,
                            key: output_key,
                            video,
                            poll,
                            cancellation: thread_cancellation,
                            event_commands,
                            video_synchronization,
                            display,
                            screen_frames,
                        });
                        if let Err(error) = &result {
                            tracing::warn!(error, "screen media worker stopped");
                        }
                        result
                    })
            }
            (StreamDescriptor::Audio(descriptor), OwnedMediaSockets::Audio { data, control }) => {
                let generation = audio_generation
                    .ok_or_else(|| "audio stream generation is absent".to_owned())?;
                let feedback_clock = Arc::clone(&self.feedback_clock);
                let flush_state = Arc::clone(&self.flush_state);
                let microphone_route = MicrophoneInputRoute::BuiltInSuperbird;
                thread::Builder::new()
                    .name(format!("prelude-audio-{}", descriptor.stream_type()))
                    .spawn(move || {
                        let result = audio_worker(
                            descriptor,
                            data,
                            control,
                            peer,
                            output_key,
                            input_key,
                            audio,
                            progress,
                            poll,
                            thread_cancellation,
                            feedback_clock,
                            flush_state,
                            microphone_route,
                            generation,
                        );
                        if let Err(error) = &result {
                            tracing::warn!(error, "audio media worker stopped");
                        }
                        result
                    })
            }
            _ => unreachable!("stream and socket ownership was checked before mutation"),
        }
        .map_err(|error| format!("starting media stream worker failed: {error}"));
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                if !is_screen {
                    self.feedback_clock.remove_stream(key);
                    let descriptor = self.audio_descriptors.get(&key).cloned();
                    let generation = self.audio_stream_generations.get(&key).copied();
                    if let (Some(descriptor), Some(generation)) = (descriptor, generation) {
                        let stream_id = audio_stream_id(descriptor.kind);
                        if let Err(rollback) = self.with_audio_if_started(|audio| {
                            audio.deactivate_stream(stream_id, generation)
                        }) {
                            let stopped = self.stop_audio_backend();
                            self.audio_descriptors.remove(&key);
                            self.audio_stream_generations.remove(&key);
                            self.refresh_audio_stream_latencies()?;
                            return match stopped {
                                Ok(()) => Err(format!(
                                    "{error}; retiring the failed audio stream also failed: {rollback}"
                                )),
                                Err(stopped) => Err(format!(
                                    "{error}; retiring the failed audio stream failed: {rollback}; stopping the mixer also failed: {stopped}"
                                )),
                            };
                        }
                    }
                    self.audio_descriptors.remove(&key);
                    self.audio_stream_generations.remove(&key);
                    self.refresh_audio_stream_latencies()?;
                    if self.audio_descriptors.is_empty() {
                        self.stop_audio_backend()?;
                    }
                }
                return Err(error);
            }
        };
        self.children.insert(key, (cancellation, worker));
        self.update_progress(|progress| {
            if is_screen {
                progress.screen_streams = progress.screen_streams.saturating_add(1);
            } else {
                progress.audio_streams = progress.audio_streams.saturating_add(1);
                progress.microphone_negotiated |= microphone_expected;
                progress.recording |= recording_reactivated;
            }
        });
        Ok(())
    }

    fn start_recording(&mut self) -> Result<bool, String> {
        self.recording_requested = true;
        let Some(needs_audio) =
            recording_audio_requirement(self.audio_output_enabled, self.audio_descriptors.len())?
        else {
            return Ok(false);
        };
        if !needs_audio {
            self.update_progress(|progress| progress.recording = true);
            return Ok(true);
        }
        if self
            .audio
            .lock()
            .map_err(|_| "audio backend lock is poisoned".to_owned())?
            .is_none()
        {
            self.start_audio_backend()?;
        }
        self.update_progress(|progress| progress.recording = true);
        Ok(true)
    }

    fn audio_streams(&self) -> Vec<AudioStreamConfig> {
        persistent_audio_stream_configs(&self.audio_descriptors)
    }

    fn refresh_audio_stream_latencies(&self) -> Result<(), String> {
        let mut latencies = self
            .audio_descriptors
            .values()
            .map(|descriptor| {
                let format = AudioFormatId::new(descriptor.audio_format);
                let effective_playout_latency_ms = AudioLatencyConfig::default()
                    .effective_playout_latency_ms(format, descriptor.audio_latency_ms)
                    .map_err(|error| format!("resolving stream latency failed: {error}"))?;
                Ok(AudioStreamLatencyStatus {
                    stream_type: descriptor.stream_type(),
                    effective_playout_latency_ms,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        latencies.sort_by_key(|latency| latency.stream_type);
        *self
            .audio_stream_latencies
            .lock()
            .map_err(|_| "audio latency status lock is poisoned".to_owned())? = latencies;
        Ok(())
    }

    fn start_audio_backend(&mut self) -> Result<(), String> {
        if self.audio_descriptors.is_empty() {
            return Ok(());
        }
        let streams = self.audio_streams();
        let base_route = self
            .audio_route
            .clone()
            .ok_or_else(|| "audio output is enabled without a route".to_owned())?;
        let mut config = LinuxAudioBackendConfig::with_negotiated_route(base_route, streams);
        config.equalizer = self.equalizer.clone();
        config.command_capacity = 64;
        config.mix_policy = AudioMixPolicy::carplay();
        let backend = match LinuxAudioBackend::start(config) {
            Ok(backend) => backend,
            Err(error) => {
                self.update_progress(|progress| {
                    progress.audio_pipeline_failure_events =
                        progress.audio_pipeline_failure_events.saturating_add(1);
                    progress.audio_last_failure = Some(AudioFailureKind::Pipeline);
                });
                return Err(format!("starting audio mixer failed: {error}"));
            }
        };
        for (key, descriptor) in &self.audio_descriptors {
            let generation = self
                .audio_stream_generations
                .get(key)
                .copied()
                .ok_or_else(|| "negotiated audio stream has no generation".to_owned())?;
            backend
                .configure_stream(audio_stream_config(descriptor), generation)
                .map_err(|error| format!("activating negotiated audio stream failed: {error}"))?;
        }
        if let Err(error) = backend.set_ducking_gain(self.ducked, self.ducked_main_gain) {
            self.update_progress(|progress| {
                progress.audio_pipeline_failure_events =
                    progress.audio_pipeline_failure_events.saturating_add(1);
                progress.audio_last_failure = Some(AudioFailureKind::Pipeline);
            });
            return Err(format!("applying initial duck state failed: {error}"));
        }
        if let Err(error) = backend.set_muted(self.muted) {
            self.update_progress(|progress| {
                progress.audio_pipeline_failure_events =
                    progress.audio_pipeline_failure_events.saturating_add(1);
                progress.audio_last_failure = Some(AudioFailureKind::Pipeline);
            });
            return Err(format!("applying initial mute state failed: {error}"));
        }
        let mut active = self
            .audio
            .lock()
            .map_err(|_| "audio backend lock is poisoned".to_owned())?;
        self.update_progress(mark_audio_pipeline_started);
        *active = Some(backend);
        Ok(())
    }

    fn stop_audio_backend(&mut self) -> Result<(), String> {
        let active = self
            .audio
            .lock()
            .map_err(|_| "audio backend lock is poisoned".to_owned())?
            .take();
        if let Some(active) = active {
            active
                .shutdown()
                .map_err(|error| format!("stopping audio mixer failed: {error}"))?;
        }
        Ok(())
    }

    fn flush_audio(&mut self, point: FlushPoint) -> Result<(), String> {
        {
            let mut state = self
                .flush_state
                .lock()
                .map_err(|_| "audio flush state lock is poisoned".to_owned())?;
            state.generation = state.generation.wrapping_add(1);
            if state.generation == 0 {
                state.generation = 1;
            }
            state.point = Some(point);
        }
        self.feedback_clock.clear_origins();
        if self.recording_requested && !self.audio_descriptors.is_empty() {
            self.with_audio_if_started(LinuxAudioBackend::flush)?;
        }
        Ok(())
    }

    fn with_audio_if_started(
        &self,
        operation: impl FnOnce(&LinuxAudioBackend) -> prelude_media::Result<()>,
    ) -> Result<(), String> {
        let active = self
            .audio
            .lock()
            .map_err(|_| "audio backend lock is poisoned".to_owned())?;
        match active.as_ref() {
            Some(backend) => {
                operation(backend).map_err(|error| format!("audio mixer control failed: {error}"))
            }
            None => Ok(()),
        }
    }

    fn teardown_streams(&mut self, streams: &[StreamKey]) -> Result<(), String> {
        if streams.is_empty() {
            self.audio_descriptors.clear();
            self.recording_requested = false;
            return self.stop_all();
        }
        for key in streams {
            if let Some((cancellation, worker)) = self.children.remove(key) {
                cancellation.cancel();
                worker
                    .join()
                    .map_err(|_| "media stream worker panicked during teardown".to_owned())??;
            }
            let descriptor = self.audio_descriptors.get(key).cloned();
            let generation = self.audio_stream_generations.get(key).copied();
            match (descriptor, generation) {
                (Some(descriptor), Some(generation)) => {
                    self.with_audio_if_started(|audio| {
                        audio.deactivate_stream(audio_stream_id(descriptor.kind), generation)
                    })?;
                    self.audio_descriptors.remove(key);
                    self.audio_stream_generations.remove(key);
                    self.feedback_clock.remove_stream(*key);
                }
                (None, None) => {
                    self.feedback_clock.remove_stream(*key);
                }
                _ => {
                    return Err(
                        "audio descriptor and generation ownership diverged during teardown"
                            .to_owned(),
                    );
                }
            }
        }
        self.refresh_audio_stream_latencies()?;
        let recording = self.recording_requested;
        self.update_progress(|progress| {
            progress.recording = recording;
            progress.screen_streams = self
                .children
                .keys()
                .filter(|key| key.stream_type == 110 || key.stream_type == 111)
                .count();
            progress.audio_streams = self.audio_descriptors.len();
            progress.microphone_negotiated = self
                .audio_descriptors
                .values()
                .any(|descriptor| descriptor.microphone.is_some());
        });
        Ok(())
    }

    fn audit_children(&mut self) -> Result<(), String> {
        if self
            .timing
            .as_ref()
            .is_some_and(|(_, worker)| worker.is_finished())
            && !self.cancellation.is_cancelled()
        {
            let (_, worker) = self
                .timing
                .take()
                .expect("finished timing worker was checked before removal");
            let failure = join_media_worker(worker, "timing worker")
                .err()
                .unwrap_or_else(|| "timing worker stopped unexpectedly".to_owned());
            self.update_progress(|progress| {
                progress.audio_last_failure = Some(AudioFailureKind::MediaWorker);
            });
            return self.fail(&failure);
        }

        let finished = self
            .children
            .iter()
            .filter_map(|(key, (_, worker))| worker.is_finished().then_some(*key))
            .collect::<Vec<_>>();
        for key in finished {
            let (_, worker) = self
                .children
                .remove(&key)
                .expect("finished media worker was checked before removal");
            if let Err(error) = join_media_worker(worker, "media stream worker") {
                self.update_progress(|progress| {
                    progress.audio_last_failure = Some(AudioFailureKind::MediaWorker);
                });
                return self.fail(&error);
            }
        }
        if let Ok(active) = self.audio.lock() {
            if let Some(audio) = active.as_ref() {
                if let Ok(Some(failure)) = audio.try_error() {
                    let failure_kind = audio
                        .telemetry()
                        .ok()
                        .and_then(|telemetry| telemetry.last_failure)
                        .map_or(AudioFailureKind::Pipeline, audio_backend_failure_kind);
                    self.update_progress(|progress| {
                        progress.audio_pipeline_failure_events =
                            progress.audio_pipeline_failure_events.saturating_add(1);
                        progress.audio_last_failure = Some(failure_kind);
                    });
                    return self.fail(&format!(
                        "{} backend failed: {}",
                        failure.component, failure.message
                    ));
                }
            }
        }
        Ok(())
    }

    fn stop_all(&mut self) -> Result<(), String> {
        let mut failure = None;
        if let Some((cancellation, worker)) = self.timing.take() {
            cancellation.cancel();
            record_cleanup_result(
                &mut failure,
                worker
                    .join()
                    .map_err(|_| "timing worker panicked during shutdown".to_owned())
                    .and_then(|result| result),
            );
        }
        for (_, (cancellation, worker)) in std::mem::take(&mut self.children) {
            cancellation.cancel();
            record_cleanup_result(
                &mut failure,
                worker
                    .join()
                    .map_err(|_| "media stream worker panicked during shutdown".to_owned())
                    .and_then(|result| result),
            );
        }
        match self.audio.lock() {
            Ok(mut active) => {
                if let Some(audio) = active.take() {
                    record_cleanup_result(
                        &mut failure,
                        audio
                            .shutdown()
                            .map_err(|error| format!("stopping USB audio mixer failed: {error}")),
                    );
                }
            }
            Err(_) => record_cleanup_result(
                &mut failure,
                Err("audio backend lock is poisoned".to_owned()),
            ),
        }
        self.audio_descriptors.clear();
        self.audio_stream_generations.clear();
        if let Ok(mut latencies) = self.audio_stream_latencies.lock() {
            latencies.clear();
        }
        self.recording_requested = false;
        self.feedback_clock.clear();
        self.update_progress(|progress| {
            *progress = retained_media_progress_after_session(*progress);
        });
        failure.map_or(Ok(()), Err)
    }

    fn fail<T>(&self, detail: &str) -> Result<T, String> {
        if let Ok(mut failure) = self.failure.lock() {
            if failure.is_none() {
                *failure = Some(detail.to_owned());
            }
        }
        Err(detail.to_owned())
    }

    fn update_progress(&self, update: impl FnOnce(&mut MediaProgress)) {
        if let Ok(mut progress) = self.progress.lock() {
            update(&mut progress);
        }
    }
}

fn join_media_worker(worker: JoinHandle<Result<(), String>>, name: &str) -> Result<(), String> {
    worker.join().map_err(|_| format!("{name} panicked"))?
}

fn record_cleanup_result(failure: &mut Option<String>, result: Result<(), String>) {
    if let Err(error) = result {
        if failure.is_none() {
            *failure = Some(error);
        }
    }
}

fn recording_audio_requirement(
    audio_output_enabled: bool,
    negotiated_audio_streams: usize,
) -> Result<Option<bool>, String> {
    match (audio_output_enabled, negotiated_audio_streams) {
        (false, 0) => Ok(Some(false)),
        (false, _) => Err("audio stream arrived while audio output was disabled".to_owned()),
        (true, 0) => Ok(None),
        (true, _) => Ok(Some(true)),
    }
}

fn timing_worker(
    socket: UdpSocket,
    peer: SocketAddrV6,
    interval: Duration,
    cancellation: CancellationToken,
    feedback_clock: Arc<FeedbackClock>,
) -> Result<(), String> {
    let mut codec = TimingCodec::default();
    let mut next_request = Instant::now();
    let mut packet = [0_u8; 64];
    while !cancellation.is_cancelled() {
        if Instant::now() >= next_request {
            if matches!(codec.state(), prelude_media::TimingState::Pending) {
                codec
                    .expire_pending()
                    .map_err(|error| format!("expiring timing request failed: {error}"))?;
            }
            let request = codec
                .request(feedback_clock.synchronized_now()?)
                .map_err(|error| format!("building timing request failed: {error}"))?;
            socket
                .send_to(&request, peer)
                .map_err(|error| format!("sending timing type-210 failed: {error}"))?;
            next_request = Instant::now() + interval;
        }
        match socket.recv_from(&mut packet) {
            Ok((length, source)) => {
                if !same_scoped_peer_address(source, peer) {
                    return Err("timing datagram came from a different scoped NCM peer".to_owned());
                }
                let receive = feedback_clock.synchronized_now()?;
                let transmit = feedback_clock.synchronized_now()?;
                match codec
                    .receive(&packet[..length], receive, transmit)
                    .map_err(|error| format!("decoding timing packet failed: {error}"))?
                {
                    TimingEvent::Response(response) => socket
                        .send_to(&response, source)
                        .map(|_| ())
                        .map_err(|error| format!("sending timing type-211 failed: {error}"))?,
                    TimingEvent::Sample(sample) => {
                        feedback_clock.update_timing(sample.offset_ns, sample.round_trip_ns)?;
                    }
                    TimingEvent::Stale => {
                        tracing::debug!("ignored a late or duplicate CarPlay timing response");
                    }
                    TimingEvent::Teardown => {}
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("receiving timing packet failed: {error}")),
        }
    }
    let _ = codec.teardown();
    Ok(())
}

struct ScreenWorkerContext {
    listener: TcpListener,
    peer: SocketAddrV6,
    key: Zeroizing<[u8; 32]>,
    video: LinuxVideoBackendConfig,
    poll: Duration,
    cancellation: CancellationToken,
    event_commands: SyncSender<EventWorkerCommand>,
    video_synchronization: VideoSynchronizationTelemetry,
    display: Option<LinuxDisplayHandle>,
    screen_frames: Arc<AtomicU64>,
}

fn screen_worker(context: ScreenWorkerContext) -> Result<(), String> {
    let ScreenWorkerContext {
        listener,
        peer,
        key,
        video,
        poll,
        cancellation,
        event_commands,
        video_synchronization,
        display,
        screen_frames,
    } = context;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configuring screen listener failed: {error}"))?;
    let mut accepted = None;
    while accepted.is_none() && !cancellation.is_cancelled() {
        match listener.accept() {
            Ok((stream, source)) => {
                if !same_scoped_peer_address(source, peer) {
                    return Err("screen TCP peer differs from the selected NCM peer".to_owned());
                }
                accepted = Some(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(poll),
            Err(error) => return Err(format!("accepting screen stream failed: {error}")),
        }
    }
    let Some(mut stream) = accepted else {
        return Ok(());
    };
    stream
        .set_read_timeout(Some(poll))
        .map_err(|error| format!("setting screen timeout failed: {error}"))?;
    let mut decoder = ScreenWireDecoder::new(*key, ScreenWireLimits::default())
        .map_err(|error| format!("initializing screen decoder failed: {error}"))?;
    let mut backend: Option<LinuxVideoBackend> = None;
    let mut frame_ordinal = 0_u64;
    let mut input = [0_u8; 64 * 1024];
    'screen: while !cancellation.is_cancelled() {
        let length = match stream.read(&mut input) {
            Ok(0) => break 'screen,
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                poll_video_backend(backend.as_ref(), &event_commands)?;
                continue;
            }
            Err(error) => return Err(format!("reading screen stream failed: {error}")),
        };
        for event in decoder
            .push(&input[..length])
            .map_err(|error| format!("decoding screen stream failed: {error}"))?
        {
            match event {
                ScreenWireEvent::Configuration(configuration) => {
                    tracing::debug!(
                        codec = ?configuration.codec,
                        codec_data_bytes = configuration.codec_data.len(),
                        nal_length_bytes = configuration.nal_length_bytes,
                        "received CarPlay screen codec configuration"
                    );
                    frame_ordinal = 0;
                    match backend.as_ref() {
                        Some(active) => {
                            active
                                .submit_configuration(configuration)
                                .map_err(|error| {
                                    format!("updating video configuration failed: {error}")
                                })?
                        }
                        None => {
                            backend = Some(
                                LinuxVideoBackend::start_with_display(
                                    video,
                                    configuration,
                                    video_synchronization.clone(),
                                    display.clone(),
                                )
                                .map_err(|error| {
                                    format!("starting H.264 backend failed: {error}")
                                })?,
                            );
                        }
                    }
                }
                ScreenWireEvent::Frame(frame) => {
                    frame_ordinal = frame_ordinal.saturating_add(1);
                    screen_frames.fetch_add(1, Ordering::AcqRel);
                    let idr = frame
                        .nal_units
                        .iter()
                        .any(|descriptor| descriptor.unit_type == 5);
                    if frame_ordinal == 1 {
                        tracing::info!(
                            idr,
                            "the first CarPlay video frame arrived from the iPhone"
                        );
                    }
                    if frame_ordinal <= 20 || idr {
                        let nal_types = frame
                            .nal_units
                            .iter()
                            .map(|descriptor| descriptor.unit_type)
                            .collect::<Vec<_>>();
                        let nal_lengths = frame
                            .nal_units
                            .iter()
                            .map(|descriptor| descriptor.payload_length)
                            .collect::<Vec<_>>();
                        tracing::debug!(
                            frame_ordinal,
                            nonce_counter = frame.nonce_counter,
                            frame_bytes = frame.data.len(),
                            ?nal_types,
                            ?nal_lengths,
                            idr,
                            "received authenticated CarPlay screen access unit"
                        );
                    }
                    backend
                        .as_ref()
                        .ok_or_else(|| {
                            "screen frame arrived before backend configuration".to_owned()
                        })?
                        .submit_frame(frame)
                        .map_err(|error| format!("submitting H.264 frame failed: {error}"))
                        .map(|_| ())?;
                }
                ScreenWireEvent::Control(_) => {}
                ScreenWireEvent::Teardown => break 'screen,
            }
        }
        poll_video_backend(backend.as_ref(), &event_commands)?;
    }
    let _ = decoder.teardown();
    if let Some(active) = backend {
        active
            .shutdown()
            .map_err(|error| format!("stopping video backend failed: {error}"))?;
    }
    Ok(())
}

fn poll_video_backend(
    backend: Option<&LinuxVideoBackend>,
    event_commands: &SyncSender<EventWorkerCommand>,
) -> Result<(), String> {
    let Some(active) = backend else {
        return Ok(());
    };
    if let Some(failure) = active
        .try_error()
        .map_err(|error| format!("polling video backend failed: {error}"))?
    {
        return Err(format!("{} failed: {}", failure.component, failure.message));
    }
    while let Some(event) = active
        .try_event()
        .map_err(|error| format!("polling video backend events failed: {error}"))?
    {
        if event == VideoBackendEvent::FirstFrameRendered {
            tracing::info!("the first CarPlay video frame is on screen");
            continue;
        }
        match event_commands.try_send(EventWorkerCommand::ForceKeyFrame) {
            Ok(()) => {
                tracing::info!("requesting a clean CarPlay keyframe for the hardware decoder");
            }
            Err(TrySendError::Full(_)) => {
                tracing::debug!("CarPlay event channel is busy; deferring keyframe request");
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(
                    "queueing CarPlay keyframe request failed: event worker stopped".to_owned(),
                );
            }
        }
    }
    Ok(())
}

fn video_backend_config(
    backend: crate::VideoBackend,
    output_delay: AudioOutputDelay,
) -> Result<LinuxVideoBackendConfig, String> {
    if backend != crate::VideoBackend::AmlogicV4l2 {
        return Err("live media requires the Amlogic V4L2 hardware decoder".to_owned());
    }
    Ok(LinuxVideoBackendConfig {
        output_delay,
        ..LinuxVideoBackendConfig::default()
    })
}

fn video_synchronization_status(
    snapshot: VideoSynchronizationTelemetrySnapshot,
) -> VideoSynchronizationStatus {
    VideoSynchronizationStatus {
        output_delay_source: match snapshot.output_delay.source() {
            AudioOutputDelaySource::Uncompensated => AudioOutputDelaySourceStatus::Uncompensated,
            AudioOutputDelaySource::ConfiguredCalibration => {
                AudioOutputDelaySourceStatus::ConfiguredCalibration
            }
            AudioOutputDelaySource::TransportReported => {
                AudioOutputDelaySourceStatus::TransportReported
            }
            AudioOutputDelaySource::Measured => AudioOutputDelaySourceStatus::Measured,
        },
        output_delay_ms: snapshot.output_delay.milliseconds(),
        presented_frames: snapshot.presented_frames,
        presentation_deadline_miss_events: snapshot.presentation_deadline_miss_events,
        total_scheduled_wait_ns: snapshot.total_scheduled_wait_ns,
        maximum_presentation_lateness_ns: snapshot.maximum_presentation_lateness_ns,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcmEndian {
    Big,
    Little,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PcmStatistics {
    mean_abs_q15: u16,
    peak_q15: u16,
    mean_channel_delta: u16,
}

fn pcm_s16_statistics(payload: &[u8], channels: u8, endian: PcmEndian) -> Option<PcmStatistics> {
    let channels = usize::from(channels);
    if payload.is_empty()
        || payload.len() % 2 != 0
        || channels == 0
        || channels > 8
        || payload.len() / 2 % channels != 0
    {
        return None;
    }

    let mut absolute_sum = 0_u64;
    let mut peak = 0_u16;
    let mut sample_count = 0_u64;
    let mut previous = [0_i16; 8];
    let mut channel_delta_sum = 0_u64;
    let mut channel_delta_count = 0_u64;
    for (index, sample) in payload.chunks_exact(2).enumerate() {
        let value = match endian {
            PcmEndian::Big => i16::from_be_bytes([sample[0], sample[1]]),
            PcmEndian::Little => i16::from_le_bytes([sample[0], sample[1]]),
        };
        let magnitude = value.unsigned_abs();
        absolute_sum = absolute_sum.saturating_add(u64::from(magnitude));
        peak = peak.max(magnitude);
        sample_count += 1;
        let channel = index % channels;
        if index >= channels {
            let delta = (i32::from(value) - i32::from(previous[channel])).unsigned_abs();
            channel_delta_sum = channel_delta_sum.saturating_add(u64::from(delta));
            channel_delta_count += 1;
        }
        previous[channel] = value;
    }

    Some(PcmStatistics {
        mean_abs_q15: (absolute_sum / sample_count) as u16,
        peak_q15: peak,
        mean_channel_delta: channel_delta_sum
            .checked_div(channel_delta_count)
            .unwrap_or(0) as u16,
    })
}

fn next_audio_pipeline_generation() -> u64 {
    NEXT_AUDIO_PIPELINE_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .unwrap_or(u64::MAX)
}

fn mark_audio_pipeline_started(progress: &mut MediaProgress) {
    progress.audio_pipeline_generation = next_audio_pipeline_generation();
    progress.main_pcm_level_observations = 0;
    progress.main_pcm_mean_abs_q15 = None;
    progress.main_pcm_peak_q15 = None;
    progress.main_pcm_be_mean_channel_delta = None;
    progress.main_pcm_le_mean_abs_q15 = None;
    progress.main_pcm_le_peak_q15 = None;
    progress.main_pcm_le_mean_channel_delta = None;
}

#[allow(clippy::too_many_arguments)]
fn audio_worker(
    descriptor: AudioDescriptor,
    data: UdpSocket,
    control: UdpSocket,
    peer: SocketAddrV6,
    output_key: Zeroizing<[u8; 32]>,
    input_key: Option<Zeroizing<[u8; 32]>>,
    backend: Arc<Mutex<Option<LinuxAudioBackend>>>,
    progress: Arc<Mutex<MediaProgress>>,
    poll: Duration,
    cancellation: CancellationToken,
    feedback_clock: Arc<FeedbackClock>,
    flush_state: Arc<Mutex<AudioFlushState>>,
    microphone_route: MicrophoneInputRoute,
    stream_generation: u64,
) -> Result<(), String> {
    validate_local_scope(&data, peer.scope_id())?;
    validate_local_scope(&control, peer.scope_id())?;
    configure_audio_sockets(&data, &control, poll)?;
    let stream_id = audio_stream_id(descriptor.kind);
    let mut decoder = AudioWireDecoder::new(
        *output_key,
        stream_id,
        AudioFormatId::new(descriptor.audio_format),
        AudioWireLimits::default(),
    )
    .map_err(|error| format!("initializing audio decoder failed: {error}"))?;
    let microphone_destination = descriptor.microphone.as_ref().map(|microphone| {
        SocketAddrV6::new(
            *peer.ip(),
            microphone.peer_data_port.get(),
            0,
            peer.scope_id(),
        )
    });
    let mut microphone = None;
    let mut microphone_socket = None;
    let mut microphone_failed = false;
    let mut input = vec![0_u8; MAXIMUM_UDP_PACKET_BYTES];
    let mut control_input = [0_u8; MAXIMUM_AUDIO_CONTROL_BYTES];
    let mut preroll = AudioPreroll::default();
    let mut microphone_activity = MicrophoneActivityGuard::new(Arc::clone(&progress));
    let mut flush_generation = flush_state
        .lock()
        .map_err(|_| "audio flush state lock is poisoned".to_owned())?
        .generation;
    let mut next_playback_level_diagnostic = Instant::now();
    let stream_key = StreamKey {
        stream_type: descriptor.stream_type(),
        connection_id: descriptor.connection_id,
    };
    'audio: while !cancellation.is_cancelled() {
        if apply_pending_audio_flush(
            &mut decoder,
            &mut preroll,
            &mut flush_generation,
            &flush_state,
        )?
        .is_some_and(|point| point.sequence.is_none() && point.rtp_time.is_none())
        {
            drain_audio_data_socket(&data, peer, &mut input)?;
        }
        match control.recv_from(&mut control_input) {
            Ok((_, source)) if !same_scoped_peer_address(source, peer) => {
                return Err("audio control datagram came from a different NCM peer".to_owned());
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(format!("polling audio control socket failed: {error}")),
        }
        let (length, source) = match data.recv_from(&mut input) {
            Ok(packet) => packet,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                poll_microphone(
                    &mut microphone,
                    microphone_socket.as_ref(),
                    microphone_destination,
                    &mut microphone_activity,
                );
                continue;
            }
            Err(error) => return Err(format!("receiving audio data failed: {error}")),
        };
        if !same_scoped_peer_address(source, peer) {
            return Err("audio datagram came from a different scoped NCM peer".to_owned());
        }
        if apply_pending_audio_flush(
            &mut decoder,
            &mut preroll,
            &mut flush_generation,
            &flush_state,
        )?
        .is_some_and(|point| point.sequence.is_none() && point.rtp_time.is_none())
        {
            drain_audio_data_socket(&data, peer, &mut input)?;
            continue;
        }
        let monotonic_ns =
            u64::try_from(feedback_clock.monotonic_origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let events = decoder
            .decode_at(&input[..length], monotonic_ns)
            .map_err(|error| format!("decoding audio packet failed: {error}"))?;
        for event in events {
            match event {
                AudioWireEvent::Started(origin) => {
                    feedback_clock.set_origin(stream_key, origin)?;
                    if microphone.is_none() && !microphone_failed {
                        if let Some(microphone_descriptor) = descriptor.microphone.as_ref() {
                            match start_microphone(
                                &descriptor,
                                microphone_descriptor,
                                input_key.as_deref(),
                                &microphone_route,
                                &data,
                            ) {
                                Ok((started, socket, config)) => {
                                    microphone = Some(started);
                                    microphone_socket = Some(socket);
                                    microphone_activity.capture_started(config);
                                }
                                Err(error) => {
                                    microphone_failed = true;
                                    tracing::warn!(
                                        error,
                                        "CarPlay microphone could not start; keeping projection alive"
                                    );
                                }
                            }
                        }
                    }
                }
                AudioWireEvent::Frame(frame) => {
                    let now = Instant::now();
                    if now >= next_playback_level_diagnostic {
                        if let prelude_media::EncodedAudioCodec::PcmS16Be { channels, .. } =
                            frame.codec
                        {
                            let big = pcm_s16_statistics(&frame.payload, channels, PcmEndian::Big);
                            let little =
                                pcm_s16_statistics(&frame.payload, channels, PcmEndian::Little);
                            if let (Some(big), Some(little)) = (big, little) {
                                if frame.stream_id == AudioStreamId::Main {
                                    if let Ok(mut progress) = progress.lock() {
                                        progress.main_pcm_level_observations =
                                            progress.main_pcm_level_observations.saturating_add(1);
                                        progress.main_pcm_mean_abs_q15 = Some(big.mean_abs_q15);
                                        progress.main_pcm_peak_q15 = Some(big.peak_q15);
                                        progress.main_pcm_be_mean_channel_delta =
                                            Some(big.mean_channel_delta);
                                        progress.main_pcm_le_mean_abs_q15 =
                                            Some(little.mean_abs_q15);
                                        progress.main_pcm_le_peak_q15 = Some(little.peak_q15);
                                        progress.main_pcm_le_mean_channel_delta =
                                            Some(little.mean_channel_delta);
                                    }
                                }
                                tracing::info!(
                                    stream = ?frame.stream_id,
                                    channels,
                                    be_mean_abs_q15 = big.mean_abs_q15,
                                    be_peak_q15 = big.peak_q15,
                                    be_mean_channel_delta = big.mean_channel_delta,
                                    le_mean_abs_q15 = little.mean_abs_q15,
                                    le_peak_q15 = little.peak_q15,
                                    le_mean_channel_delta = little.mean_channel_delta,
                                    "CarPlay PCM playback coherence"
                                );
                            } else {
                                tracing::warn!(
                                    stream = ?frame.stream_id,
                                    payload_bytes = frame.payload.len(),
                                    channels,
                                    "CarPlay PCM playback frame has invalid sample geometry"
                                );
                            }
                            next_playback_level_diagnostic = now + Duration::from_secs(2);
                        }
                    }
                    let active = backend
                        .lock()
                        .map_err(|_| "audio backend lock is poisoned".to_owned())?;
                    if let Some(active) = active.as_ref() {
                        for pending in preroll.drain() {
                            submit_audio_frame(
                                active,
                                stream_generation,
                                pending,
                                &progress,
                                true,
                            )?;
                        }
                        submit_audio_frame(active, stream_generation, frame, &progress, false)?;
                    } else {
                        preroll.push(frame);
                    }
                }
                AudioWireEvent::Teardown { .. } => break 'audio,
            }
        }
        poll_microphone(
            &mut microphone,
            microphone_socket.as_ref(),
            microphone_destination,
            &mut microphone_activity,
        );
    }
    let _ = decoder.teardown();
    if let Some(microphone) = microphone {
        if let Err(error) = microphone.shutdown() {
            tracing::warn!(error = %error, "stopping CarPlay microphone failed");
        }
    }
    Ok(())
}

struct MicrophoneActivityGuard {
    progress: Arc<Mutex<MediaProgress>>,
    capture_running: bool,
    announced_first_packet: bool,
}

impl MicrophoneActivityGuard {
    fn new(progress: Arc<Mutex<MediaProgress>>) -> Self {
        Self {
            progress,
            capture_running: false,
            announced_first_packet: false,
        }
    }

    fn capture_started(&mut self, config: MicrophoneConfig) {
        self.capture_running = true;
        if let Ok(mut progress) = self.progress.lock() {
            progress.microphone_negotiated = true;
            progress.microphone_capture_running = true;
            progress.microphone_ready = false;
            progress.microphone_rms_q15 = None;
            progress.microphone_peak_q15 = None;
        }
        tracing::info!(
            codec = ?config.codec,
            sample_rate_hz = config.sample_rate_hz,
            channels = config.channels,
            frame_duration_micros = config.frame_duration_micros,
            "CarPlay microphone capture started"
        );
    }

    fn packet_sent(
        &mut self,
        packet_bytes: usize,
        level: Option<prelude_media::MicrophoneCaptureLevel>,
    ) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.microphone_ready = true;
            progress.microphone_packets_sent = progress.microphone_packets_sent.saturating_add(1);
            progress.microphone_bytes_sent = progress
                .microphone_bytes_sent
                .saturating_add(u64::try_from(packet_bytes).unwrap_or(u64::MAX));
            if let Some(level) = level {
                progress.microphone_rms_q15 = Some(level.rms_q15);
                progress.microphone_peak_q15 = Some(
                    progress
                        .microphone_peak_q15
                        .unwrap_or(0)
                        .max(level.peak_q15),
                );
            }
        }
        if !self.announced_first_packet {
            self.announced_first_packet = true;
            tracing::info!("first authenticated CarPlay microphone packet sent");
        }
    }

    fn capture_stopped(&mut self) {
        if !std::mem::replace(&mut self.capture_running, false) {
            return;
        }
        tracing::info!("CarPlay microphone capture stopped");
        if let Ok(mut progress) = self.progress.lock() {
            progress.microphone_capture_running = false;
            progress.microphone_ready = false;
        }
    }

    fn packet_failed(&mut self) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.microphone_send_failure_events =
                progress.microphone_send_failure_events.saturating_add(1);
            progress.microphone_ready = false;
        }
    }
}

impl Drop for MicrophoneActivityGuard {
    fn drop(&mut self) {
        self.capture_stopped();
    }
}

fn submit_audio_frame(
    backend: &LinuxAudioBackend,
    stream_generation: u64,
    frame: EncryptedAudioFrame,
    progress: &Mutex<MediaProgress>,
    buffered: bool,
) -> Result<(), String> {
    match backend.try_submit_generation(stream_generation, frame) {
        Ok(()) => {
            if let Ok(mut progress) = progress.lock() {
                progress.audio_frames_accepted = progress.audio_frames_accepted.saturating_add(1);
            }
            Ok(())
        }
        Err(error) => {
            if let Ok(mut progress) = progress.lock() {
                if error.to_string().contains("audio mixer ingress is full") {
                    progress.audio_ingress_full_events =
                        progress.audio_ingress_full_events.saturating_add(1);
                    progress.audio_last_failure = Some(AudioFailureKind::IngressBackpressure);
                    return Ok(());
                } else {
                    progress.audio_last_failure = Some(AudioFailureKind::Pipeline);
                }
            }
            let context = if buffered {
                "submitting buffered audio frame failed"
            } else {
                "submitting audio frame failed"
            };
            Err(format!("{context}: {error}"))
        }
    }
}

fn apply_pending_audio_flush(
    decoder: &mut AudioWireDecoder,
    preroll: &mut AudioPreroll,
    applied_generation: &mut u64,
    flush_state: &Mutex<AudioFlushState>,
) -> Result<Option<FlushPoint>, String> {
    let flush = *flush_state
        .lock()
        .map_err(|_| "audio flush state lock is poisoned".to_owned())?;
    if flush.generation == *applied_generation {
        return Ok(None);
    }
    let applied = if let Some(point) = flush.point {
        let point = if decoder.origin().is_some() {
            point
        } else {
            FlushPoint {
                sequence: None,
                rtp_time: None,
            }
        };
        decoder
            .flush(point.sequence, point.rtp_time)
            .map_err(|error| format!("flushing audio decoder failed: {error}"))?;
        *preroll = AudioPreroll::default();
        Some(point)
    } else {
        None
    };
    *applied_generation = flush.generation;
    Ok(applied)
}

fn drain_audio_data_socket(
    data: &UdpSocket,
    peer: SocketAddrV6,
    input: &mut [u8],
) -> Result<(), String> {
    data.set_nonblocking(true)
        .map_err(|error| format!("starting audio flush drain failed: {error}"))?;
    let drain = (|| {
        for _ in 0..MAXIMUM_AUDIO_FLUSH_DRAIN_PACKETS {
            match data.recv_from(input) {
                Ok((_, source)) if !same_scoped_peer_address(source, peer) => {
                    return Err("audio datagram came from a different scoped NCM peer".to_owned());
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(format!("draining pre-flush audio failed: {error}")),
            }
        }
        Err("pre-flush audio drain exceeded its packet bound".to_owned())
    })();
    let restore = data
        .set_nonblocking(false)
        .map_err(|error| format!("ending audio flush drain failed: {error}"));
    match (drain, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

fn configure_audio_sockets(
    data: &UdpSocket,
    control: &UdpSocket,
    poll: Duration,
) -> Result<(), String> {
    data.set_nonblocking(false)
        .map_err(|error| format!("configuring blocking audio data I/O failed: {error}"))?;
    data.set_read_timeout(Some(poll))
        .map_err(|error| format!("setting audio data timeout failed: {error}"))?;
    control
        .set_nonblocking(true)
        .map_err(|error| format!("configuring audio control socket failed: {error}"))
}

#[derive(Debug, Default)]
struct AudioPreroll {
    frames: VecDeque<EncryptedAudioFrame>,
    payload_bytes: usize,
}

impl AudioPreroll {
    fn push(&mut self, frame: EncryptedAudioFrame) {
        let frame_bytes = frame.payload.len();
        if frame_bytes > MAXIMUM_AUDIO_PREROLL_BYTES {
            self.frames.clear();
            self.payload_bytes = 0;
            return;
        }
        while self
            .payload_bytes
            .checked_add(frame_bytes)
            .is_none_or(|total| total > MAXIMUM_AUDIO_PREROLL_BYTES)
        {
            let Some(dropped) = self.frames.pop_front() else {
                break;
            };
            self.payload_bytes = self.payload_bytes.saturating_sub(dropped.payload.len());
        }
        self.payload_bytes += frame_bytes;
        self.frames.push_back(frame);
    }

    fn drain(&mut self) -> impl Iterator<Item = EncryptedAudioFrame> + '_ {
        self.payload_bytes = 0;
        self.frames.drain(..)
    }
}

fn start_microphone(
    descriptor: &AudioDescriptor,
    microphone: &prelude_net::MicrophoneDescriptor,
    input_key: Option<&[u8; 32]>,
    route: &MicrophoneInputRoute,
    data: &UdpSocket,
) -> Result<(LinuxMicrophoneBackend, UdpSocket, MicrophoneConfig), String> {
    let key = input_key.ok_or_else(|| "microphone SETUP has no input key".to_owned())?;
    let config = microphone_config(descriptor, microphone)?;
    let packetizer = MicrophonePacketizer::new(*key, config)
        .map_err(|error| format!("initializing microphone packetizer failed: {error}"))?;
    let socket = bind_microphone_socket(data)?;
    let backend = LinuxMicrophoneBackend::start(LinuxMicrophoneBackendConfig::with_route(
        packetizer,
        route.clone(),
    ))
    .map_err(|error| format!("starting microphone input failed: {error}"))?;
    Ok((backend, socket, config))
}

fn poll_microphone(
    microphone: &mut Option<LinuxMicrophoneBackend>,
    socket: Option<&UdpSocket>,
    destination: Option<SocketAddrV6>,
    activity: &mut MicrophoneActivityGuard,
) {
    let Err(error) = pump_microphone(microphone.as_ref(), socket, destination, activity) else {
        return;
    };
    tracing::warn!(
        error,
        "CarPlay microphone uplink failed; keeping projection alive"
    );
    if let Some(failed) = microphone.take() {
        if let Err(error) = failed.shutdown() {
            tracing::warn!(error = %error, "stopping failed CarPlay microphone also failed");
        }
    }
    activity.capture_stopped();
}

fn pump_microphone(
    microphone: Option<&LinuxMicrophoneBackend>,
    socket: Option<&UdpSocket>,
    destination: Option<SocketAddrV6>,
    activity: &mut MicrophoneActivityGuard,
) -> Result<(), String> {
    let Some(microphone) = microphone else {
        return Ok(());
    };
    if let Some(failure) = microphone
        .try_error()
        .map_err(|error| format!("polling microphone backend failed: {error}"))?
    {
        return Err(format!("{} failed: {}", failure.component, failure.message));
    }
    while let Some(event) = microphone
        .try_event()
        .map_err(|error| format!("polling microphone packet failed: {error}"))?
    {
        match event {
            MicrophoneBackendEvent::Packet { packet, level } => {
                let socket = socket.ok_or_else(|| "microphone UDP socket is absent".to_owned())?;
                let destination = destination
                    .ok_or_else(|| "negotiated microphone destination is absent".to_owned())?;
                match socket.send_to(&packet.encoded, destination) {
                    Ok(sent) if sent == packet.encoded.len() => {
                        activity.packet_sent(sent, level);
                    }
                    Ok(_) => {
                        activity.packet_failed();
                        return Err("sending microphone packet was truncated".to_owned());
                    }
                    Err(error) => {
                        activity.packet_failed();
                        return Err(format!("sending microphone packet failed: {error}"));
                    }
                }
            }
            MicrophoneBackendEvent::Stopped => {
                return Err("microphone backend stopped unexpectedly".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_audio_descriptor(descriptor: &AudioDescriptor) -> Result<(), String> {
    let stream = audio_stream_id(descriptor.kind);
    if !OFFERED_FORMAT_IDS.contains(&(stream, descriptor.audio_format)) {
        return Err(format!(
            "negotiated audio format {:#x} was not advertised for {stream:?}",
            descriptor.audio_format
        ));
    }
    AudioFormatId::new(descriptor.audio_format)
        .codec()
        .map_err(|error| format!("unsupported negotiated audio format for {stream:?}: {error}"))?;
    AudioLatencyConfig::default()
        .effective_playout_latency_ms(
            AudioFormatId::new(descriptor.audio_format),
            descriptor.audio_latency_ms,
        )
        .map_err(|error| format!("invalid negotiated audio latency for {stream:?}: {error}"))?;
    if descriptor.microphone.is_some() && stream != AudioStreamId::Main {
        return Err("microphone uplink is valid only on MainAudio type 100".to_owned());
    }
    Ok(())
}

fn microphone_config(
    descriptor: &AudioDescriptor,
    microphone: &prelude_net::MicrophoneDescriptor,
) -> Result<MicrophoneConfig, String> {
    let codec = AudioFormatId::new(microphone.audio_format)
        .codec()
        .map_err(|error| format!("unsupported microphone format: {error}"))?;
    let (microphone_codec, sample_rate_hz, channels) = match codec {
        prelude_media::EncodedAudioCodec::PcmS16Be {
            sample_rate_hz,
            channels,
        } => (MicrophoneCodec::PcmS16, sample_rate_hz, channels),
        prelude_media::EncodedAudioCodec::Opus {
            sample_rate_hz,
            channels,
        } => (MicrophoneCodec::Opus, sample_rate_hz, channels),
        prelude_media::EncodedAudioCodec::AacLc { .. } => {
            return Err("AAC-LC microphone uplink is not supported".to_owned());
        }
    };
    if descriptor.kind != AudioStreamKind::Main || channels != 1 {
        return Err("microphone uplink requires negotiated mono MainAudio".to_owned());
    }
    let frame_duration_micros = microphone.frames_per_packet.map_or(
        prelude_media::DEFAULT_MICROPHONE_FRAME_MICROS,
        |frames| {
            u32::try_from(
                u64::from(frames)
                    .saturating_mul(1_000_000)
                    .checked_div(u64::from(sample_rate_hz))
                    .unwrap_or(0),
            )
            .unwrap_or(0)
        },
    );
    MicrophoneConfig {
        stream_id: AudioStreamId::Main,
        codec: microphone_codec,
        sample_rate_hz,
        channels,
        frame_duration_micros,
        maximum_encoded_frame_bytes: 64 * 1024,
        maximum_pcm_buffer_bytes: 256 * 1024,
    }
    .validate()
    .map_err(|error| format!("invalid microphone framing: {error}"))
}

fn preflight_microphone_config(codec: MicrophoneCodec) -> MicrophoneConfig {
    MicrophoneConfig {
        stream_id: AudioStreamId::Main,
        codec,
        sample_rate_hz: 48_000,
        channels: 1,
        frame_duration_micros: prelude_media::DEFAULT_MICROPHONE_FRAME_MICROS,
        maximum_encoded_frame_bytes: 64 * 1024,
        maximum_pcm_buffer_bytes: 256 * 1024,
    }
}

fn bind_microphone_socket(data: &UdpSocket) -> Result<UdpSocket, String> {
    let SocketAddr::V6(local) = data
        .local_addr()
        .map_err(|error| format!("reading local audio address failed: {error}"))?
    else {
        return Err("audio socket is not IPv6".to_owned());
    };
    UdpSocket::bind(SocketAddrV6::new(*local.ip(), 0, 0, local.scope_id()))
        .map_err(|error| format!("binding microphone uplink socket failed: {error}"))
}

fn matcher(config: &UsbDacConfig) -> Result<UsbDacMatcher, String> {
    config
        .matcher()
        .map_err(|error| format!("invalid USB DAC matcher: {error}"))
}

fn audio_stream_id(kind: AudioStreamKind) -> AudioStreamId {
    match kind {
        AudioStreamKind::Main => AudioStreamId::Main,
        AudioStreamKind::Alternate => AudioStreamId::Alternate,
        AudioStreamKind::High => AudioStreamId::High,
    }
}

fn limit_music_buffer(mut descriptor: StreamDescriptor, limit_ms: Option<u32>) -> StreamDescriptor {
    if let (StreamDescriptor::Audio(audio), Some(limit_ms)) = (&mut descriptor, limit_ms)
        && matches!(
            AudioFormatId::new(audio.audio_format).codec(),
            Ok(prelude_media::EncodedAudioCodec::AacLc { .. })
        )
    {
        let requested_ms = audio
            .audio_latency_ms
            .filter(|latency| *latency != 0)
            .unwrap_or(AudioLatencyConfig::default().aac_jitter_ms);
        if requested_ms > limit_ms {
            tracing::info!(
                requested_ms,
                limit_ms,
                "shortening the wireless CarPlay music buffer"
            );
            audio.audio_latency_ms = Some(limit_ms);
        }
    }
    descriptor
}

fn audio_stream_config(descriptor: &AudioDescriptor) -> AudioStreamConfig {
    AudioStreamConfig::with_negotiated_latency(
        audio_stream_id(descriptor.kind),
        AudioFormatId::new(descriptor.audio_format),
        descriptor.audio_latency_ms,
    )
}

fn persistent_audio_stream_configs(
    descriptors: &BTreeMap<StreamKey, AudioDescriptor>,
) -> Vec<AudioStreamConfig> {
    let wireless = descriptors.values().any(|descriptor| {
        !matches!(
            AudioFormatId::new(descriptor.audio_format).codec(),
            Ok(prelude_media::EncodedAudioCodec::PcmS16Be { .. })
        )
    });
    let placeholder = if wireless {
        OPUS_48K_MONO_FORMAT
    } else {
        PCM_48K_STEREO_FORMAT
    };
    let mut configs: Vec<_> = PERSISTENT_CARPLAY_AUDIO_STREAMS
        .into_iter()
        .map(|stream_id| {
            descriptors
                .values()
                .find(|descriptor| audio_stream_id(descriptor.kind) == stream_id)
                .map_or_else(
                    || AudioStreamConfig::new(stream_id, AudioFormatId::new(placeholder)),
                    audio_stream_config,
                )
        })
        .collect();
    if wireless {
        configs.push(
            descriptors
                .values()
                .find(|descriptor| descriptor.kind == AudioStreamKind::High)
                .map_or_else(
                    || {
                        AudioStreamConfig::new(
                            AudioStreamId::High,
                            AudioFormatId::new(AAC_LC_48K_STEREO),
                        )
                    },
                    audio_stream_config,
                ),
        );
    }
    configs
}

fn validate_local_scope(socket: &UdpSocket, expected_scope: u32) -> Result<(), String> {
    match socket
        .local_addr()
        .map_err(|error| format!("reading retained UDP address failed: {error}"))?
    {
        SocketAddr::V6(local)
            if local.ip().is_unicast_link_local() && local.scope_id() == expected_scope =>
        {
            Ok(())
        }
        _ => Err("retained UDP socket escaped the selected scoped NCM interface".to_owned()),
    }
}

fn same_scoped_peer(left: SocketAddrV6, right: SocketAddrV6) -> bool {
    left.ip() == right.ip() && left.scope_id() == right.scope_id()
}

fn same_scoped_peer_address(source: SocketAddr, expected: SocketAddrV6) -> bool {
    matches!(source, SocketAddr::V6(source) if same_scoped_peer(source, expected))
}

fn now_ntp() -> Result<NtpTimestamp, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_owned())?;
    let seconds = now
        .as_secs()
        .checked_add(NTP_UNIX_EPOCH_OFFSET)
        .ok_or_else(|| "NTP seconds overflow".to_owned())?;
    let seconds = u32::try_from(seconds).map_err(|_| "NTP era overflow".to_owned())?;
    let fractional = ((u64::from(now.subsec_nanos())) << 32) / 1_000_000_000;
    Ok(NtpTimestamp::from_parts(seconds, fractional as u32))
}

fn add_ntp_offset(timestamp: NtpTimestamp, offset_ns: i128) -> NtpTimestamp {
    const NTP_SCALE: i128 = 1_i128 << 32;
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    let offset = offset_ns.saturating_mul(NTP_SCALE) / NANOS_PER_SECOND;
    let modulus = 1_i128 << 64;
    let adjusted = (i128::from(timestamp.raw()) + offset).rem_euclid(modulus);
    NtpTimestamp::new(adjusted as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_teardown_retains_bounded_audio_evidence_only() {
        let progress = MediaProgress {
            timing_ready: true,
            recording: true,
            audio_pipeline_generation: 7,
            audio_frames_accepted: 89,
            audio_ingress_full_events: 2,
            audio_pipeline_failure_events: 3,
            main_pcm_level_observations: 4,
            main_pcm_mean_abs_q15: Some(1_500),
            main_pcm_peak_q15: Some(9_000),
            main_pcm_be_mean_channel_delta: Some(700),
            main_pcm_le_mean_abs_q15: Some(16_000),
            main_pcm_le_peak_q15: Some(32_000),
            main_pcm_le_mean_channel_delta: Some(18_000),
            audio_last_failure: Some(AudioFailureKind::Pipeline),
            ..MediaProgress::default()
        };
        let retained = retained_media_progress_after_session(progress);
        assert_eq!(retained.audio_pipeline_generation, 7);
        assert_eq!(retained.audio_frames_accepted, 89);
        assert_eq!(retained.audio_ingress_full_events, 2);
        assert_eq!(retained.audio_pipeline_failure_events, 3);
        assert_eq!(retained.main_pcm_level_observations, 4);
        assert_eq!(retained.main_pcm_mean_abs_q15, Some(1_500));
        assert_eq!(retained.main_pcm_peak_q15, Some(9_000));
        assert_eq!(retained.main_pcm_be_mean_channel_delta, Some(700));
        assert_eq!(retained.main_pcm_le_mean_abs_q15, Some(16_000));
        assert_eq!(retained.main_pcm_le_peak_q15, Some(32_000));
        assert_eq!(retained.main_pcm_le_mean_channel_delta, Some(18_000));
        assert_eq!(
            retained.audio_last_failure,
            Some(AudioFailureKind::Pipeline)
        );
        assert!(!retained.timing_ready);
        assert!(!retained.recording);
    }

    #[test]
    fn pipeline_generations_are_process_wide_and_monotonic() {
        let first = next_audio_pipeline_generation();
        let second = next_audio_pipeline_generation();
        assert!(second > first || first == u64::MAX);
    }

    #[test]
    fn a_new_pipeline_clears_levels_from_the_previous_session() {
        let previous_generation = next_audio_pipeline_generation();
        let mut progress = MediaProgress {
            audio_pipeline_generation: previous_generation,
            audio_frames_accepted: 100,
            main_pcm_level_observations: 4,
            main_pcm_mean_abs_q15: Some(1_500),
            main_pcm_peak_q15: Some(9_000),
            main_pcm_be_mean_channel_delta: Some(700),
            main_pcm_le_mean_abs_q15: Some(16_000),
            main_pcm_le_peak_q15: Some(32_000),
            main_pcm_le_mean_channel_delta: Some(18_000),
            ..MediaProgress::default()
        };
        mark_audio_pipeline_started(&mut progress);
        assert!(
            progress.audio_pipeline_generation > previous_generation
                || previous_generation == u64::MAX
        );
        assert_eq!(progress.audio_frames_accepted, 100);
        assert_eq!(progress.main_pcm_level_observations, 0);
        assert_eq!(progress.main_pcm_mean_abs_q15, None);
        assert_eq!(progress.main_pcm_peak_q15, None);
        assert_eq!(progress.main_pcm_be_mean_channel_delta, None);
        assert_eq!(progress.main_pcm_le_mean_abs_q15, None);
        assert_eq!(progress.main_pcm_le_peak_q15, None);
        assert_eq!(progress.main_pcm_le_mean_channel_delta, None);
    }

    #[test]
    fn pcm_coherence_probe_distinguishes_byte_order_without_audio_data() {
        assert_eq!(pcm_s16_statistics(&[], 1, PcmEndian::Big), None);
        assert_eq!(pcm_s16_statistics(&[0], 1, PcmEndian::Big), None);
        assert_eq!(pcm_s16_statistics(&[0, 0], 2, PcmEndian::Big), None);

        let samples = [0_i16, 100, 200, 300];
        let payload = samples
            .into_iter()
            .flat_map(i16::to_be_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            pcm_s16_statistics(&payload, 1, PcmEndian::Big),
            Some(PcmStatistics {
                mean_abs_q15: 150,
                peak_q15: 300,
                mean_channel_delta: 100,
            })
        );
        assert_eq!(
            pcm_s16_statistics(&payload, 1, PcmEndian::Little),
            Some(PcmStatistics {
                mean_abs_q15: 12_800,
                peak_q15: 25_600,
                mean_channel_delta: 30_379,
            })
        );
    }

    #[test]
    fn navigation_teardown_cannot_clear_an_active_siri_microphone() {
        let progress = Arc::new(Mutex::new(MediaProgress {
            microphone_negotiated: true,
            ..MediaProgress::default()
        }));
        let navigation = MicrophoneActivityGuard::new(Arc::clone(&progress));
        let mut siri = MicrophoneActivityGuard::new(Arc::clone(&progress));
        siri.capture_started(preflight_microphone_config(MicrophoneCodec::PcmS16));
        siri.packet_sent(704, None);
        let before_navigation_teardown = *progress.lock().expect("progress");
        drop(navigation);
        assert_eq!(
            *progress.lock().expect("progress"),
            before_navigation_teardown,
            "a downlink-only worker does not own microphone state"
        );
        siri.packet_sent(704, None);
        let active = *progress.lock().expect("progress");
        let mut status = AudioRuntimeSnapshot::waiting(AudioRuntimeRoute::UsbDac);
        status.microphone = MicrophoneRuntimeStatus {
            negotiated: active.microphone_negotiated,
            capture_running: active.microphone_capture_running,
            ready: active.microphone_ready,
            packets_sent: active.microphone_packets_sent,
            bytes_sent: active.microphone_bytes_sent,
            ..MicrophoneRuntimeStatus::default()
        };
        status
            .validate()
            .expect("snapshot after navigation teardown");
        drop(siri);
        let stopped = *progress.lock().expect("progress");
        assert!(!stopped.microphone_capture_running);
        assert!(!stopped.microphone_ready);
        assert_eq!(stopped.microphone_packets_sent, 2);
    }

    #[test]
    fn microphone_readiness_requires_a_successful_uplink_packet() {
        let progress = Arc::new(Mutex::new(MediaProgress::default()));
        {
            let mut activity = MicrophoneActivityGuard::new(Arc::clone(&progress));
            activity.capture_started(preflight_microphone_config(MicrophoneCodec::PcmS16));
            let starting = *progress.lock().expect("progress");
            assert!(starting.microphone_negotiated);
            assert!(starting.microphone_capture_running);
            assert!(!starting.microphone_ready);
            activity.packet_sent(
                704,
                Some(prelude_media::MicrophoneCaptureLevel {
                    rms_q15: 1_200,
                    peak_q15: 3_000,
                }),
            );
            let ready = *progress.lock().expect("progress");
            assert!(ready.microphone_ready);
            assert_eq!(ready.microphone_packets_sent, 1);
            assert_eq!(ready.microphone_bytes_sent, 704);
            assert_eq!(ready.microphone_rms_q15, Some(1_200));
            assert_eq!(ready.microphone_peak_q15, Some(3_000));
        }
        let stopped = *progress.lock().expect("progress");
        assert!(!stopped.microphone_capture_running);
        assert!(!stopped.microphone_ready);
        assert_eq!(stopped.microphone_packets_sent, 1);
    }

    fn audio_frame(sequence: u16, payload_bytes: usize) -> EncryptedAudioFrame {
        EncryptedAudioFrame {
            stream_id: AudioStreamId::Main,
            format_id: AudioFormatId::new(0x4),
            codec: AudioFormatId::new(0x4).codec().expect("PCM codec"),
            sequence,
            timestamp: u32::from(sequence),
            ssrc: 7,
            nonce_counter: u64::from(sequence),
            payload: vec![sequence as u8; payload_bytes],
            rtp_packet: vec![0; 12 + payload_bytes],
        }
    }

    #[test]
    fn offered_formats_are_all_consumable_and_match_pinned_masks() {
        for &(_, format) in OFFERED_FORMAT_IDS {
            AudioFormatId::new(format).codec().expect("offered format");
        }
        assert_eq!(PCM_VOICE_FORMATS | PCM_MEDIA_48K_FORMATS, 0xc3fc);
        assert_eq!(PCM_MONO_48K_FORMATS, 0x4154);
    }

    #[test]
    fn only_an_absent_configured_usb_dac_degrades_to_screen_only() {
        assert!(
            classify_audio_route_preflight(AudioBackend::AlsaUsb, Ok(())).expect("ready route")
        );
        assert!(
            !classify_audio_route_preflight(
                AudioBackend::AlsaUsb,
                Err(MediaError::NotReady {
                    component: "USB DAC",
                    state: "not connected",
                }),
            )
            .expect("absent optional DAC")
        );
        assert!(
            classify_audio_route_preflight(
                AudioBackend::AlsaUsb,
                Err(MediaError::Backend(
                    "multiple matching USB DAC devices were found".to_owned(),
                )),
            )
            .is_err()
        );
    }

    #[test]
    fn manual_route_preflight_remains_strict_when_startup_can_degrade() {
        assert!(require_configured_audio_output(true, true).is_ok());
        assert!(require_configured_audio_output(false, false).is_ok());
        assert_eq!(
            require_configured_audio_output(true, false),
            Err("audio route preflight failed: configured audio output is unavailable".to_owned())
        );
    }

    #[test]
    fn scoped_peer_comparison_does_not_invent_source_port_contract() {
        let left = SocketAddrV6::new("fe80::1".parse().expect("ip"), 1234, 0, 9);
        let same_phone_other_port = SocketAddrV6::new("fe80::1".parse().expect("ip"), 5678, 0, 9);
        let other_scope = SocketAddrV6::new("fe80::1".parse().expect("ip"), 1234, 0, 10);
        assert!(same_scoped_peer(left, same_phone_other_port));
        assert!(!same_scoped_peer(left, other_scope));
    }

    #[test]
    fn recording_waits_for_negotiated_audio_when_audio_is_advertised() {
        assert_eq!(
            recording_audio_requirement(false, 0).expect("screen-only recording"),
            Some(false)
        );
        assert_eq!(
            recording_audio_requirement(true, 1).expect("audio recording"),
            Some(true)
        );
        assert_eq!(
            recording_audio_requirement(true, 0).expect("deferred audio recording"),
            None
        );
        assert!(recording_audio_requirement(false, 1).is_err());
    }

    #[test]
    fn wired_carplay_mixer_preprovisions_pcm_navigation() {
        let main = AudioDescriptor {
            kind: AudioStreamKind::Main,
            connection_id: 41,
            audio_type: "media".to_owned(),
            audio_format: PCM_48K_STEREO_FORMAT,
            audio_latency_ms: Some(32),
            frames_per_packet: None,
            microphone: None,
        };
        validate_audio_descriptor(&main).expect("advertised Main PCM");
        let key = StreamDescriptor::Audio(main.clone()).key();
        let streams = persistent_audio_stream_configs(&BTreeMap::from([(key, main)]));
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].stream_id, AudioStreamId::Main);
        assert_eq!(streams[0].negotiated_latency_ms, Some(32));
        assert_eq!(streams[1].stream_id, AudioStreamId::Alternate);
        assert_eq!(streams[1].format_id.get(), PCM_48K_STEREO_FORMAT);
        assert_eq!(streams[1].negotiated_latency_ms, None);

        let unadvertised = AudioDescriptor {
            kind: AudioStreamKind::High,
            connection_id: 42,
            audio_type: "media".to_owned(),
            audio_format: PCM_48K_STEREO_FORMAT,
            audio_latency_ms: None,
            frames_per_packet: None,
            microphone: None,
        };
        assert!(validate_audio_descriptor(&unadvertised).is_err());
    }

    #[test]
    fn low_latency_music_shortens_only_the_wireless_music_buffer() {
        let audio = |audio_format, audio_latency_ms| {
            StreamDescriptor::Audio(AudioDescriptor {
                kind: AudioStreamKind::High,
                connection_id: 44,
                audio_type: "media".to_owned(),
                audio_format,
                audio_latency_ms,
                frames_per_packet: None,
                microphone: None,
            })
        };
        let latency = |descriptor: StreamDescriptor| match descriptor {
            StreamDescriptor::Audio(audio) => audio.audio_latency_ms,
            _ => unreachable!("audio descriptor"),
        };
        let limit = Some(LOW_LATENCY_MUSIC_BUFFER_MS);
        for requested in [Some(1_000), Some(0), None] {
            assert_eq!(
                latency(limit_music_buffer(
                    audio(AAC_LC_48K_STEREO, requested),
                    limit
                )),
                Some(LOW_LATENCY_MUSIC_BUFFER_MS)
            );
        }
        assert_eq!(
            latency(limit_music_buffer(
                audio(AAC_LC_48K_STEREO, Some(300)),
                limit
            )),
            Some(300)
        );
        assert_eq!(
            latency(limit_music_buffer(
                audio(AAC_LC_48K_STEREO, Some(1_000)),
                None
            )),
            Some(1_000)
        );
        assert_eq!(
            latency(limit_music_buffer(
                audio(OPUS_48K_MONO_FORMAT, Some(1_000)),
                limit
            )),
            Some(1_000)
        );
    }

    #[test]
    fn wireless_carplay_mixer_provisions_opus_and_aac_streams() {
        let media = AudioDescriptor {
            kind: AudioStreamKind::High,
            connection_id: 43,
            audio_type: "media".to_owned(),
            audio_format: AAC_LC_48K_STEREO,
            audio_latency_ms: Some(730),
            frames_per_packet: None,
            microphone: None,
        };
        validate_audio_descriptor(&media).expect("advertised wireless media AAC-LC");
        let key = StreamDescriptor::Audio(media.clone()).key();
        let streams = persistent_audio_stream_configs(&BTreeMap::from([(key, media)]));
        assert_eq!(
            streams
                .iter()
                .map(|stream| (stream.stream_id, stream.format_id.get()))
                .collect::<Vec<_>>(),
            vec![
                (AudioStreamId::Main, OPUS_48K_MONO_FORMAT),
                (AudioStreamId::Alternate, OPUS_48K_MONO_FORMAT),
                (AudioStreamId::High, AAC_LC_48K_STEREO),
            ]
        );
        assert_eq!(streams[2].negotiated_latency_ms, Some(730));
        prelude_media::AudioMixerPlan::with_stream_options(
            &streams,
            prelude_media::AudioMixPolicy::carplay(),
            AudioLatencyConfig::default(),
        )
        .expect("wireless mixer plan");

        let telephony = AudioDescriptor {
            kind: AudioStreamKind::Main,
            connection_id: 44,
            audio_type: "telephony".to_owned(),
            audio_format: 0x1000_0000,
            audio_latency_ms: None,
            frames_per_packet: None,
            microphone: None,
        };
        validate_audio_descriptor(&telephony).expect("advertised wireless telephony Opus");
    }

    #[test]
    fn clean_media_worker_completion_is_not_escalated_to_a_session_failure() {
        let worker = std::thread::spawn(|| Ok(()));
        assert_eq!(join_media_worker(worker, "media stream worker"), Ok(()));
    }

    #[test]
    fn failed_media_worker_completion_preserves_the_worker_error() {
        let worker = std::thread::spawn(|| Err("screen peer failed".to_owned()));
        assert_eq!(
            join_media_worker(worker, "media stream worker"),
            Err("screen peer failed".to_owned())
        );
    }

    #[test]
    fn latest_typed_audio_failure_is_not_inferred_from_historical_counters() {
        let historical_ingress_then_pipeline = MediaProgress {
            audio_ingress_full_events: 7,
            audio_pipeline_failure_events: 1,
            audio_last_failure: Some(AudioFailureKind::Pipeline),
            ..MediaProgress::default()
        };
        assert_eq!(
            reported_audio_failure(true, historical_ingress_then_pipeline, None),
            Some(AudioFailureKind::Pipeline)
        );

        let latest_bus_failure = MediaProgress {
            audio_last_failure: Some(AudioFailureKind::Bus),
            ..historical_ingress_then_pipeline
        };
        assert_eq!(
            reported_audio_failure(true, latest_bus_failure, Some(AudioFailureKind::Endpoint)),
            Some(AudioFailureKind::Bus)
        );
    }

    #[test]
    fn usb_video_delay_is_uncompensated() {
        let usb = video_backend_config(
            crate::VideoBackend::AmlogicV4l2,
            AudioOutputDelay::uncompensated(),
        )
        .expect("USB video configuration");
        assert_eq!(usb.output_delay, AudioOutputDelay::uncompensated());

        let status = video_synchronization_status(VideoSynchronizationTelemetrySnapshot {
            output_delay: usb.output_delay,
            presented_frames: 10,
            presentation_deadline_miss_events: 1,
            total_scheduled_wait_ns: 2,
            maximum_presentation_lateness_ns: 3,
        });
        assert_eq!(
            status.output_delay_source,
            AudioOutputDelaySourceStatus::Uncompensated
        );
        assert_eq!(status.output_delay_ms, 0);
    }

    #[test]
    fn feedback_uses_the_same_negotiated_latency_as_the_mixer_plan() {
        let descriptor = AudioDescriptor {
            kind: AudioStreamKind::High,
            connection_id: 77,
            audio_type: "media".to_owned(),
            audio_format: AAC_LC_48K_STEREO,
            audio_latency_ms: Some(730),
            frames_per_packet: None,
            microphone: None,
        };
        let key = StreamDescriptor::Audio(descriptor.clone()).key();
        let clock = FeedbackClock::new();
        clock.add_stream(key, &descriptor).expect("feedback stream");
        assert_eq!(
            clock
                .state
                .lock()
                .expect("clock state")
                .streams
                .get(&key)
                .expect("stream")
                .playout_latency_ms,
            730
        );
        let plan = prelude_media::AudioMixerPlan::with_stream_options(
            &[AudioStreamConfig::with_negotiated_latency(
                AudioStreamId::High,
                AudioFormatId::new(AAC_LC_48K_STEREO),
                descriptor.audio_latency_ms,
            )],
            prelude_media::AudioMixPolicy::default(),
            AudioLatencyConfig::default(),
        )
        .expect("mixer plan");
        assert_eq!(
            plan.stream(AudioStreamId::High)
                .expect("high stream")
                .jitter_latency_ms,
            Some(730)
        );
    }

    #[test]
    fn audio_preroll_is_bounded_and_preserves_retained_order() {
        let mut preroll = AudioPreroll::default();
        preroll.push(audio_frame(1, 300 * 1024));
        preroll.push(audio_frame(2, 300 * 1024));
        preroll.push(audio_frame(3, 64));

        let retained = preroll
            .drain()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>();
        assert_eq!(retained, [2, 3]);
        assert_eq!(preroll.payload_bytes, 0);
        assert!(preroll.frames.is_empty());
    }

    #[test]
    fn oversized_audio_preroll_frame_clears_stale_audio() {
        let mut preroll = AudioPreroll::default();
        preroll.push(audio_frame(1, 64));
        preroll.push(audio_frame(2, MAXIMUM_AUDIO_PREROLL_BYTES + 1));

        assert!(preroll.drain().next().is_none());
    }

    #[test]
    fn audio_data_socket_blocks_after_prebound_nonblocking_setup() {
        let data = UdpSocket::bind("[::1]:0").expect("data socket");
        let control = UdpSocket::bind("[::1]:0").expect("control socket");
        data.set_nonblocking(true)
            .expect("prebound nonblocking data");
        control
            .set_nonblocking(true)
            .expect("prebound nonblocking control");
        configure_audio_sockets(&data, &control, Duration::from_millis(250))
            .expect("worker socket setup");

        let destination = data.local_addr().expect("data address");
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let socket = UdpSocket::bind("[::1]:0").expect("sender socket");
            socket.send_to(b"audio", destination).expect("send audio");
        });
        let mut packet = [0_u8; 16];
        let started = Instant::now();
        let (length, _) = data.recv_from(&mut packet).expect("blocking receive");
        sender.join().expect("sender thread");

        assert_eq!(&packet[..length], b"audio");
        assert!(started.elapsed() >= Duration::from_millis(10));
        let mut control_packet = [0_u8; 1];
        assert_eq!(
            control
                .recv_from(&mut control_packet)
                .expect_err("control remains nonblocking")
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn steered_clock_steps_large_residuals_and_slews_small_ones() {
        let clock = FeedbackClock::new();
        let local = NtpTimestamp::from_parts(4_000_000_000, 0);
        clock
            .update_timing(-2_207_213_528_000_000_000, 20_000_000)
            .expect("first candidate");
        clock
            .update_timing(-2_207_213_529_000_000_000, 10_000_000)
            .expect("first step");
        assert_eq!(
            clock
                .synchronize_timestamp(local)
                .expect("synchronized timestamp"),
            NtpTimestamp::from_parts(1_792_786_471, 0)
        );

        clock
            .update_timing(90_000_000, 20_000_000)
            .expect("small candidate");
        clock
            .update_timing(80_000_000, 10_000_000)
            .expect("small slew");
        let slewed_offset = -2_207_213_529_000_000_000_i128 + 10_000_000;
        assert_eq!(
            clock
                .state
                .lock()
                .expect("clock state")
                .synchronized_offset_ns,
            Some(slewed_offset)
        );
        assert_eq!(
            clock
                .synchronize_timestamp(local)
                .expect("slewed timestamp"),
            add_ntp_offset(local, slewed_offset)
        );

        clock
            .update_timing(300_000_000, 20_000_000)
            .expect("large candidate");
        clock
            .update_timing(256_000_000, 10_000_000)
            .expect("large step");
        assert_eq!(
            clock
                .synchronize_timestamp(local)
                .expect("stepped timestamp"),
            add_ntp_offset(local, slewed_offset + 256_000_000)
        );
    }

    #[test]
    fn timing_filter_rejects_a_higher_delay_pick() {
        let clock = FeedbackClock::new();
        clock.update_timing(64_000_000, 5_000_000).expect("pick 1");
        clock.update_timing(64_000_000, 6_000_000).expect("pick 1");
        clock.update_timing(0, 5_000_000).expect("baseline");
        clock.update_timing(0, 6_000_000).expect("baseline");
        let first = clock
            .synchronize_timestamp(NtpTimestamp::ZERO)
            .expect("first offset");
        clock
            .update_timing(120_000_000, 50_000_000)
            .expect("delayed pick");
        clock
            .update_timing(120_000_000, 60_000_000)
            .expect("delayed pick");
        assert_eq!(
            clock
                .synchronize_timestamp(NtpTimestamp::ZERO)
                .expect("unchanged offset"),
            first
        );
    }
}
