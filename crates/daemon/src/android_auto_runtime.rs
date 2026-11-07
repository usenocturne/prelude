#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-media")),
    allow(dead_code, unused_imports)
)]

#[cfg(all(target_os = "linux", feature = "linux-media"))]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use platform_superbird::{
    AmbientLightState, DeviceRole, KnobInputButton, LocalInputAction, MonotonicMicros,
    PlatformInputEvent, PlatformInputEventKind, ResetReason, RotaryDirection, TouchInputPhase,
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use prelude_android_auto::{
    AccessoryIdentity, AoapCandidate, AoapTimeouts, RusbBackend, open_accessory,
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use prelude_android_auto_protocol::{
    AudioChannel as ProtocolAudioChannel, DisconnectReason, Event, Session, SessionConfig,
};
use prelude_android_auto_protocol::{
    Button as ProtocolButton, Command, MediaKey, SessionState, TouchAction, TouchPointer,
};
use prelude_control::{ImplementationStatus, ProjectionCapabilities};
use prelude_media::{
    AudioFormatId, AudioStreamId, EncodedAudioCodec, EncryptedAudioFrame, EncryptedVideoFrame,
    NalUnitDescriptor, VideoCodecConfiguration, WireVideoCodec,
};

#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
use std::thread::{self, JoinHandle};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use std::{
    mem,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
use platform_superbird::{LinuxInputBackend, LinuxInputConfig};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use prelude_media::{
    AudioBackendFailureKind, AudioBackendTelemetrySnapshot, AudioOutputDelaySource,
    AudioStreamConfig, LinuxAudioBackend, LinuxAudioBackendConfig, LinuxDisplayHandle,
    LinuxMicrophoneBackend, LinuxMicrophoneBackendConfig, LinuxVideoBackend,
    LinuxVideoBackendConfig, MicrophoneBackendEvent, MicrophoneCodec, MicrophoneConfig,
    MicrophoneInputRoute, MicrophonePacketizer, VideoBackendEvent, VideoFrameDisposition,
    VideoSynchronizationTelemetry, VideoSynchronizationTelemetrySnapshot,
};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::first_video::{FirstVideoAction, FirstVideoWatchdog};
use crate::hands_free::HandsFree;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::hands_free::{CallAudio, CallBridge, CallSubscription};
use crate::{
    AndroidAutoConfig, AudioBackend, AudioPipelineState, AudioRuntimeRoute, AudioRuntimeSnapshot,
    ConnectionStep, DaemonConfig, DisplayPlatform, DisplayStatus, Error, HealthState, MediaConfig,
    PresetAction, PresetBindings, ProjectionDevice, ProjectionMode, ProjectionRuntime, Result,
    RuntimeProbe, persist_last_display_platform,
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::{
    AudioBackendStatus, AudioBaseSinkStatus, AudioFailureKind, AudioLatencyStatus,
    AudioOutputDelaySourceStatus, VideoBackend, VideoSynchronizationStatus,
};

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAXIMUM: Duration = Duration::from_secs(15);
const WORKER_JOIN_TIMEOUT_DETAIL: &str = "Android Auto worker did not terminate cleanly";
const MAX_H264_ACCESS_UNIT_BYTES: usize = 8 * 1024 * 1024;
const MAX_H264_NAL_UNITS: usize = 4_096;
const AUDIO_MEDIA_FORMAT: AudioFormatId = AudioFormatId::new(0x8000);
const AUDIO_VOICE_FORMAT: AudioFormatId = AudioFormatId::new(0x10);
const AUDIO_MEDIA_RATE: u32 = 48_000;
const AUDIO_VOICE_RATE: u32 = 16_000;
const AUDIO_MEDIA_CHANNELS: u8 = 2;
const AUDIO_VOICE_CHANNELS: u8 = 1;
const AUDIO_PLAYOUT_LATENCY_MS: u32 = 200;
const MICROPHONE_FRAME_DURATION_MICROS: u32 = 20_000;
const MICROPHONE_MAXIMUM_FRAME_BYTES: usize = 4_096;
const MICROPHONE_MAXIMUM_BUFFER_BYTES: usize = 16 * 1024;
const SYSTEM_CLOCK: usize = 2;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
const BLUETOOTH_PASSKEY_WAIT: Duration = Duration::from_secs(60);
const BLUETOOTH_READY_WAIT: Duration = Duration::from_secs(8);
const TEST_TONE_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_ANDROID_AUTO_WORKER_STOP_WAIT: Duration = Duration::from_secs(5);
const WORKER_STOP_POLL: Duration = Duration::from_millis(10);
#[cfg(all(target_os = "linux", feature = "linux-media"))]
const VIDEO_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(all(target_os = "linux", feature = "linux-media"))]
const WIRELESS_PING_TIMEOUT: Duration = Duration::from_secs(12);

type SharedProgress = Arc<Mutex<WorkerProgress>>;
type SharedAudioStatus = Arc<Mutex<AudioRuntimeSnapshot>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerStage {
    OpeningAoap,
    OpeningProtocol,
    Negotiating,
    StartingMedia,
    Running,
    ShuttingDown,
    Stopped,
    Failed,
}

impl WorkerStage {
    const fn transport_ready(self) -> bool {
        !matches!(self, Self::OpeningAoap | Self::Failed)
    }

    const fn protocol_ready(self) -> bool {
        matches!(self, Self::Running | Self::ShuttingDown | Self::Stopped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerProgress {
    stage: WorkerStage,
    session_state: Option<SessionState>,
    audio_ready: bool,
    input_ready: bool,
    video_ready: bool,
    microphone_negotiated: bool,
    microphone_ready: bool,
    protocol_events: u64,
    video_frames: u64,
    audio_frames: u64,
    input_events: u64,
    microphone_packets: u64,
    keyframe_requests: u64,
}

impl Default for WorkerProgress {
    fn default() -> Self {
        Self {
            stage: WorkerStage::OpeningAoap,
            session_state: None,
            audio_ready: false,
            input_ready: false,
            video_ready: false,
            microphone_negotiated: false,
            microphone_ready: false,
            protocol_events: 0,
            video_frames: 0,
            audio_frames: 0,
            input_events: 0,
            microphone_packets: 0,
            keyframe_requests: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ShutdownSignal(Arc<AtomicBool>);

impl ShutdownSignal {
    fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct RetrySchedule {
    failures: u32,
    retry_at: Option<Instant>,
}

impl RetrySchedule {
    const fn new() -> Self {
        Self {
            failures: 0,
            retry_at: None,
        }
    }

    fn record_failure(&mut self, now: Instant) {
        let shift = self.failures.min(31);
        let factor = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = RETRY_INITIAL.saturating_mul(factor).min(RETRY_MAXIMUM);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + delay);
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.retry_at
            .and_then(|retry_at| retry_at.checked_duration_since(now))
    }
}

#[derive(Debug, Clone)]
struct RuntimeFailure {
    detail: String,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
#[derive(Debug)]
enum WorkerControl {
    TestTone(SyncSender<std::result::Result<(), String>>),
}

#[derive(Debug)]
enum RuntimeConnection {
    Idle,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    Worker {
        handle: JoinHandle<std::result::Result<(), String>>,
        progress: SharedProgress,
        shutdown: ShutdownSignal,
        setup_cancellation: prelude_android_auto::CancellationToken,
        controls: SyncSender<WorkerControl>,
    },
}

pub struct AndroidAutoRuntime {
    config: AndroidAutoConfig,
    media: MediaConfig,
    state_dir: std::path::PathBuf,
    #[cfg_attr(
        not(all(target_os = "linux", feature = "linux-media")),
        allow(dead_code)
    )]
    platform: crate::PlatformConfig,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display: Option<LinuxDisplayHandle>,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display_status: Option<DisplayStatus>,
    #[cfg_attr(
        not(all(target_os = "linux", feature = "linux-media")),
        allow(dead_code)
    )]
    ambient_light: AmbientLightState,
    connection: RuntimeConnection,
    phone_present: bool,
    missing_audio_output: bool,
    retry: RetrySchedule,
    last_failure: Option<RuntimeFailure>,
    audio_status: SharedAudioStatus,
    hands_free: Option<HandsFree>,
}

impl fmt::Debug for AndroidAutoRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidAutoRuntime")
            .field("enabled", &self.config.enable_live_session)
            .field("connection", &self.connection)
            .field("retry", &self.retry)
            .field("has_failure", &self.last_failure.is_some())
            .finish_non_exhaustive()
    }
}

impl AndroidAutoRuntime {
    pub fn from_config(config: &DaemonConfig) -> Result<Self> {
        Self::build(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            None,
            AmbientLightState::default(),
        )
    }

    pub fn from_config_with_display(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            LinuxDisplayHandle,
        >,
    ) -> Result<Self> {
        Self::from_config_with_display_and_ambient(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            AmbientLightState::default(),
        )
    }

    pub fn from_config_with_display_and_ambient(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self> {
        Self::build(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            ambient_light,
        )
    }

    fn build(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self> {
        config.validate()?;
        if config.projection.mode != ProjectionMode::Automatic {
            return Err(Error::Config(
                "Android Auto runtime requires automatic projection mode".to_owned(),
            ));
        }
        if !config.android_auto.enable_live_session {
            return Err(Error::Config(
                "Android Auto live session is disabled".to_owned(),
            ));
        }
        if config.android_auto.aoap_control_timeout_ms == 0
            || config.android_auto.aoap_control_timeout_ms > 60_000
        {
            return Err(Error::Config(
                "android_auto.aoap_control_timeout_ms must be within 1..=60000".to_owned(),
            ));
        }
        if !(250..=5_000).contains(&config.android_auto.ping_interval_ms)
            || config.android_auto.ping_timeout_ms <= config.android_auto.ping_interval_ms
            || config.android_auto.ping_timeout_ms > 30_000
        {
            return Err(Error::Config(
                "Android Auto ping timing is outside the protocol bounds".to_owned(),
            ));
        }
        let aoap_description = format!(
            "{} wired Android Auto host",
            config.android_auto.head_unit_name
        );
        if aoap_description.len() + 1 > 255 {
            return Err(Error::Config(
                "android_auto.head_unit_name is too long for the AOAP identity".to_owned(),
            ));
        }
        let requested_route = requested_audio_route(&config.android_auto, &config.media);
        Ok(Self {
            config: config.android_auto.clone(),
            media: config.media.clone(),
            state_dir: config.state_dir.clone(),
            platform: config.platform.clone(),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display_status: None,
            ambient_light,
            connection: RuntimeConnection::Idle,
            phone_present: false,
            missing_audio_output: false,
            retry: RetrySchedule::new(),
            last_failure: None,
            hands_free: None,
            audio_status: Arc::new(Mutex::new(
                if requested_route == AudioRuntimeRoute::Disabled {
                    AudioRuntimeSnapshot::disabled()
                } else {
                    AudioRuntimeSnapshot::waiting(requested_route)
                },
            )),
        })
    }

    pub(crate) fn attach_hands_free(&mut self, hands_free: HandsFree) {
        self.hands_free = Some(hands_free);
    }

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    fn refresh(&mut self) {
        let running = match &self.connection {
            RuntimeConnection::Worker { progress, .. } => progress
                .lock()
                .is_ok_and(|progress| progress.stage == WorkerStage::Running),
            RuntimeConnection::Idle => false,
        };
        if running {
            self.last_failure = None;
            self.retry.record_success();
        }

        let finished = matches!(
            &self.connection,
            RuntimeConnection::Worker { handle, .. } if handle.is_finished()
        );
        if !finished {
            return;
        }
        let RuntimeConnection::Worker { handle, .. } =
            mem::replace(&mut self.connection, RuntimeConnection::Idle)
        else {
            return;
        };
        match handle.join() {
            Ok(Ok(())) => {
                self.last_failure = None;
                self.retry.record_success();
            }
            Ok(Err(detail)) => self.record_failure(detail),
            Err(_) => self.record_failure("Android Auto worker panicked".to_owned()),
        }
    }

    #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
    fn refresh(&mut self) {}

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    fn set_display_status(&mut self, status: DisplayStatus) -> Result<()> {
        let Some(display) = self.display.as_ref() else {
            return Ok(());
        };
        let projection_active = status != DisplayStatus::WaitingForPhone;
        if self.display_status == Some(status) {
            display
                .set_projection_active(projection_active)
                .map_err(|error| {
                    Error::Runtime(format!(
                        "updating the Android Auto projection activity state failed: {error}"
                    ))
                })?;
            return Ok(());
        }
        let overlay = crate::status_screen::render(DisplayPlatform::AndroidAuto, status).map_err(
            |error| {
                Error::Runtime(format!(
                    "rendering the Android Auto status screen failed: {error}"
                ))
            },
        )?;
        display.set_waiting_overlay(overlay).map_err(|error| {
            Error::Runtime(format!(
                "setting the Android Auto status overlay failed: {error}"
            ))
        })?;
        display
            .set_projection_active(projection_active)
            .map_err(|error| {
                Error::Runtime(format!(
                    "updating the Android Auto projection activity state failed: {error}"
                ))
            })?;
        self.display_status = Some(status);
        Ok(())
    }

    fn record_failure(&mut self, detail: String) {
        self.missing_audio_output = is_missing_audio_output(&detail);
        self.last_failure = Some(RuntimeFailure { detail });
        self.retry.record_failure(Instant::now());
        update_audio_status(&self.audio_status, |status| {
            if status.requested_route != AudioRuntimeRoute::Disabled {
                status.pipeline_state = AudioPipelineState::Failed;
                status.active_route = None;
                status.pipeline_failure_events = status.pipeline_failure_events.saturating_add(1);
                status.last_failure = Some(crate::AudioFailureKind::MediaWorker);
                status.recording = false;
                status.microphone.capture_running = false;
                status.microphone.ready = false;
            }
        });
    }
}

impl ProjectionRuntime for AndroidAutoRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        self.refresh();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            let status = if !self.phone_present {
                Some(DisplayStatus::WaitingForPhone)
            } else if self.missing_audio_output {
                Some(DisplayStatus::MissingAudioOutput)
            } else {
                match &self.connection {
                    RuntimeConnection::Idle => None,
                    RuntimeConnection::Worker { progress, .. } => progress
                        .lock()
                        .ok()
                        .map(|progress| android_auto_display_status(*progress)),
                }
            };
            if let Some(status) = status {
                let _ = self.set_display_status(status);
            } else if let Some(display) = self.display.as_ref() {
                let _ = display.set_projection_active(false);
            }
        }
        match &self.connection {
            RuntimeConnection::Idle => idle_probe(self.last_failure.as_ref(), &self.retry),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            RuntimeConnection::Worker { progress, .. } => worker_probe(progress),
        }
    }

    fn set_phone_presence(&mut self, present: bool) {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let present = present || matches!(self.connection, RuntimeConnection::Worker { .. });
        self.phone_present = present;
        if !present {
            self.missing_audio_output = false;
        }
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        if !present {
            let _ = self.set_display_status(DisplayStatus::WaitingForPhone);
        }
    }

    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        self.refresh();
        self.audio_status.lock().map_or_else(
            |_| {
                let mut status = AudioRuntimeSnapshot::waiting(AudioRuntimeRoute::UsbDac);
                status.pipeline_state = AudioPipelineState::Failed;
                status.last_failure = Some(crate::AudioFailureKind::MediaWorker);
                status
            },
            |status| status.clone(),
        )
    }

    fn play_audio_test_tone(&mut self) -> Result<()> {
        self.refresh();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        if let RuntimeConnection::Worker { controls, .. } = &self.connection {
            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
            match controls.try_send(WorkerControl::TestTone(reply_sender)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    return Err(Error::Runtime(
                        "Android Auto media control queue is full".to_owned(),
                    ));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(Error::NotReady {
                        component: "Android Auto audio mixer",
                        state: "worker_stopped",
                    });
                }
            }
            return reply_receiver
                .recv_timeout(TEST_TONE_TIMEOUT)
                .map_err(|_| Error::Runtime("Android Auto test tone timed out".to_owned()))?
                .map_err(Error::Runtime);
        }
        Err(Error::NotReady {
            component: "Android Auto audio mixer",
            state: "not_running",
        })
    }

    fn start(&mut self, device: &ProjectionDevice) -> Result<()> {
        self.refresh();
        if matches!(
            device,
            ProjectionDevice::CarPlay(_) | ProjectionDevice::WirelessCarPlay(_)
        ) {
            return Err(Error::Unsupported(
                "Android Auto runtime cannot start an iPhone",
            ));
        }
        if !matches!(self.connection, RuntimeConnection::Idle) {
            return Ok(());
        }
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        {
            Err(Error::NotReady {
                component: "Android Auto runtime",
                state: "linux_media_required",
            })
        }

        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            self.phone_present = true;
            if requested_audio_route(&self.config, &self.media) == AudioRuntimeRoute::UsbDac {
                let available =
                    crate::media_runtime::configured_audio_output_available(&self.media, true)
                        .map_err(Error::Runtime)?;
                if !available {
                    self.missing_audio_output = true;
                    self.set_display_status(DisplayStatus::MissingAudioOutput)?;
                    return Err(Error::NotReady {
                        component: "audio output device",
                        state: "not_connected",
                    });
                }
            }
            if self.missing_audio_output {
                self.retry.record_success();
            }
            if let Some(remaining) = self.retry.remaining(Instant::now()) {
                return Err(Error::Runtime(format!(
                    "Android Auto reconnect backoff active for {}ms",
                    remaining.as_millis()
                )));
            }
            let progress = Arc::new(Mutex::new(WorkerProgress::default()));
            let shutdown = ShutdownSignal::default();
            let setup_cancellation = prelude_android_auto::CancellationToken::default();
            let (control_sender, control_receiver) = mpsc::sync_channel(4);
            let worker_progress = Arc::clone(&progress);
            let worker_shutdown = shutdown.clone();
            let worker_setup_cancellation = setup_cancellation.clone();
            let config = self.config.clone();
            let media = self.media.clone();
            let platform = self.platform.clone();
            let ambient_light = self.ambient_light.clone();
            let transport = match device {
                ProjectionDevice::AndroidAuto(candidate) => WorkerTransport::Usb(candidate.clone()),
                ProjectionDevice::WirelessAndroidAuto(phone) => {
                    let (stream, lease) = phone.claim().ok_or(Error::NotReady {
                        component: "wireless Android Auto phone",
                        state: "connection_already_claimed",
                    })?;
                    WorkerTransport::Wireless { stream, lease }
                }
                ProjectionDevice::CarPlay(_) | ProjectionDevice::WirelessCarPlay(_) => {
                    return Err(Error::Unsupported(
                        "Android Auto runtime cannot start an iPhone",
                    ));
                }
            };
            let audio_status = Arc::clone(&self.audio_status);
            let display = self.display.clone();
            let hands_free = self.hands_free.clone();
            persist_last_display_platform(&self.state_dir, DisplayPlatform::AndroidAuto).map_err(
                |error| {
                    Error::Runtime(format!(
                        "persisting the Android Auto display platform failed: {error}"
                    ))
                },
            )?;
            self.display_status = None;
            self.set_display_status(DisplayStatus::Connecting(None))?;
            self.missing_audio_output = false;
            let handle = match thread::Builder::new()
                .name("ct-android-auto".to_owned())
                .spawn(move || {
                    run_worker(
                        transport,
                        config,
                        media,
                        platform,
                        ambient_light,
                        display,
                        worker_progress,
                        audio_status,
                        worker_shutdown,
                        worker_setup_cancellation,
                        control_receiver,
                        hands_free,
                    )
                }) {
                Ok(handle) => handle,
                Err(error) => {
                    self.phone_present = false;
                    let _ = self.set_display_status(DisplayStatus::WaitingForPhone);
                    return Err(Error::Runtime(format!(
                        "failed to start Android Auto worker: {error}"
                    )));
                }
            };
            self.last_failure = None;
            self.connection = RuntimeConnection::Worker {
                handle,
                progress,
                shutdown,
                setup_cancellation,
                controls: control_sender,
            };
            Ok(())
        }
    }

    fn begin_handoff(&mut self) {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            self.display_status = None;
            let _ = self.set_display_status(DisplayStatus::Connecting(Some(
                ConnectionStep::SwitchingToUsb,
            )));
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.stop_for_handoff()?;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        self.set_display_status(DisplayStatus::WaitingForPhone)?;
        Ok(())
    }

    fn stop_for_handoff(&mut self) -> Result<()> {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            if let RuntimeConnection::Worker {
                handle,
                progress,
                shutdown,
                setup_cancellation,
                ..
            } = &self.connection
            {
                shutdown.request();
                if progress
                    .lock()
                    .is_ok_and(|progress| progress.stage == WorkerStage::OpeningAoap)
                {
                    setup_cancellation.cancel();
                }
                if !worker_finished_within(handle, MAX_ANDROID_AUTO_WORKER_STOP_WAIT) {
                    return Err(Error::Runtime(format!(
                        "{WORKER_JOIN_TIMEOUT_DETAIL} within {:?}; ownership was retained",
                        MAX_ANDROID_AUTO_WORKER_STOP_WAIT
                    )));
                }
            }
            let connection = mem::replace(&mut self.connection, RuntimeConnection::Idle);
            if let RuntimeConnection::Worker { handle, .. } = connection {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "Android Auto session ended with an error while stopping");
                    }
                    Err(_) => tracing::warn!("Android Auto worker panicked while stopping"),
                }
            }
        }
        self.last_failure = None;
        self.missing_audio_output = false;
        self.retry.record_success();
        let requested_route = requested_audio_route(&self.config, &self.media);
        update_audio_status(&self.audio_status, |status| {
            *status = if requested_route == AudioRuntimeRoute::Disabled {
                AudioRuntimeSnapshot::disabled()
            } else {
                AudioRuntimeSnapshot::waiting(requested_route)
            };
        });
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            self.display_status = None;
        }
        Ok(())
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
fn worker_finished_within<T>(handle: &JoinHandle<T>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(WORKER_STOP_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
    handle.is_finished()
}

impl Drop for AndroidAutoRuntime {
    fn drop(&mut self) {
        if self.stop().is_err() {
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            {
                let connection = mem::replace(&mut self.connection, RuntimeConnection::Idle);
                if let RuntimeConnection::Worker {
                    handle,
                    shutdown,
                    setup_cancellation,
                    ..
                } = connection
                {
                    shutdown.request();
                    setup_cancellation.cancel();
                    let _ = handle.join();
                }
            }
        }
    }
}

fn idle_probe(failure: Option<&RuntimeFailure>, retry: &RetrySchedule) -> RuntimeProbe {
    let (state, detail) = if let Some(failure) = failure {
        (HealthState::Failed, failure.detail.clone())
    } else if let Some(remaining) = retry.remaining(Instant::now()) {
        (
            HealthState::NotReady,
            format!(
                "Android Auto reconnect backoff active for {}ms",
                remaining.as_millis()
            ),
        )
    } else {
        (
            HealthState::NotReady,
            "Android Auto is waiting for a positively identified Android phone".to_owned(),
        )
    };
    RuntimeProbe {
        state,
        detail: "Android Auto is fail-closed until AOAP, authenticated AAP, and every configured media backend are live".to_owned(),
        transport_state: state,
        transport_detail: detail,
        link_state: HealthState::NotReady,
        link_detail: "Android Auto Protocol is not connected".to_owned(),
        control_state: HealthState::NotReady,
        control_detail: "Android Auto control channel is not running".to_owned(),
        authentication_state: HealthState::NotReady,
        authentication_detail: "Android Auto TLS authentication has not started".to_owned(),
        session_state: HealthState::NotReady,
        session_detail: "Android Auto media session is not running".to_owned(),
        implementation: ImplementationStatus::AndroidAuto,
        capabilities: ProjectionCapabilities::default(),
    }
}

fn worker_probe(progress: &SharedProgress) -> RuntimeProbe {
    let progress = progress.lock().map_or(
        WorkerProgress {
            stage: WorkerStage::Failed,
            ..WorkerProgress::default()
        },
        |progress| *progress,
    );
    probe_for_progress(progress)
}

fn probe_for_progress(progress: WorkerProgress) -> RuntimeProbe {
    let stage = progress.stage;
    let failed = stage == WorkerStage::Failed;
    let control_ready = stage == WorkerStage::Running;
    let transport_ready = stage.transport_ready();
    let protocol_ready = stage.protocol_ready();
    let capabilities = ProjectionCapabilities {
        control: control_ready,
        video: control_ready && progress.video_ready,
        audio: control_ready && progress.audio_ready,
        microphone: control_ready && progress.microphone_ready,
        input: control_ready && progress.input_ready,
    };
    let fully_ready = capabilities.complete();
    RuntimeProbe {
        state: if failed {
            HealthState::Failed
        } else if fully_ready {
            HealthState::Ready
        } else {
            HealthState::Starting
        },
        detail: if fully_ready {
            "wired Android Auto control, H.264, PCM, microphone, and input are live".to_owned()
        } else if failed {
            "Android Auto worker failed and will retry while the phone remains connected".to_owned()
        } else if control_ready {
            format!(
                "Android Auto protocol is live; factual media readiness is audio={}, input={}, video={}, microphone={} (negotiated={})",
                progress.audio_ready,
                progress.input_ready,
                progress.video_ready,
                progress.microphone_ready,
                progress.microphone_negotiated
            )
        } else {
            format!("Android Auto staged bring-up is in {stage:?}")
        },
        transport_state: if failed {
            HealthState::Failed
        } else if transport_ready {
            HealthState::Ready
        } else {
            HealthState::Starting
        },
        transport_detail: if transport_ready {
            "topology-bound AOAP bulk transport is claimed".to_owned()
        } else {
            "performing bounded AOAP mode switch and USB claim".to_owned()
        },
        link_state: if failed {
            HealthState::Failed
        } else if protocol_ready {
            HealthState::Ready
        } else {
            HealthState::Starting
        },
        link_detail: if protocol_ready {
            "Android Auto Protocol 1.7 link is active".to_owned()
        } else {
            "negotiating framed Android Auto Protocol 1.7".to_owned()
        },
        control_state: if control_ready {
            HealthState::Ready
        } else if failed {
            HealthState::Failed
        } else {
            HealthState::Starting
        },
        control_detail: if control_ready {
            format!(
                "Android Auto control loop is active ({} protocol events)",
                progress.protocol_events
            )
        } else {
            "Android Auto control loop is not yet active".to_owned()
        },
        authentication_state: if protocol_ready {
            HealthState::Ready
        } else if failed {
            HealthState::Failed
        } else {
            HealthState::Starting
        },
        authentication_detail: if protocol_ready {
            "Android Auto TLS authentication completed without MFi hardware".to_owned()
        } else {
            "performing Android Auto software TLS authentication".to_owned()
        },
        session_state: if fully_ready {
            HealthState::Ready
        } else if failed {
            HealthState::Failed
        } else {
            HealthState::Starting
        },
        session_detail: if fully_ready {
            format!(
                "media session active: {} video, {} audio, {} input, {} microphone packets",
                progress.video_frames,
                progress.audio_frames,
                progress.input_events,
                progress.microphone_packets
            )
        } else if control_ready {
            format!(
                "waiting for factual configured capabilities: audio={}, input={}, video={}, microphone={} (negotiated={})",
                progress.audio_ready,
                progress.input_ready,
                progress.video_ready,
                progress.microphone_ready,
                progress.microphone_negotiated
            )
        } else {
            "waiting for Android Auto service discovery and media startup".to_owned()
        },
        implementation: ImplementationStatus::AndroidAuto,
        capabilities,
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
fn android_auto_display_status(progress: WorkerProgress) -> DisplayStatus {
    let step = match progress.stage {
        WorkerStage::Running => return DisplayStatus::Connected,
        WorkerStage::OpeningAoap => Some(ConnectionStep::AccessoryMode),
        WorkerStage::OpeningProtocol => Some(ConnectionStep::OpeningConnection),
        WorkerStage::Negotiating => match progress.session_state {
            None | Some(SessionState::Created) => Some(ConnectionStep::OpeningConnection),
            Some(SessionState::Version) => Some(ConnectionStep::ExchangingVersions),
            Some(SessionState::TlsHandshake | SessionState::Auth) => {
                Some(ConnectionStep::SecuringConnection)
            }
            Some(SessionState::ServiceDiscovery) => Some(ConnectionStep::DiscoveringServices),
            Some(SessionState::ChannelSetup | SessionState::Running) => {
                Some(ConnectionStep::OpeningChannels)
            }
            Some(SessionState::Closing | SessionState::Closed) => None,
        },
        WorkerStage::StartingMedia => Some(ConnectionStep::StartingMedia),
        WorkerStage::ShuttingDown | WorkerStage::Stopped | WorkerStage::Failed => None,
    };
    DisplayStatus::Connecting(step)
}

fn requested_audio_route(config: &AndroidAutoConfig, media: &MediaConfig) -> AudioRuntimeRoute {
    if !config.disable_audio_output && media.audio == AudioBackend::AlsaUsb {
        AudioRuntimeRoute::UsbDac
    } else {
        AudioRuntimeRoute::Disabled
    }
}

fn is_missing_audio_output(detail: &str) -> bool {
    (detail.contains("USB DAC") && detail.contains("not connected"))
        || detail.contains("matched USB DAC has no ALSA playback PCM")
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn set_session_state(progress: &SharedProgress, state: SessionState) {
    if let Ok(mut progress) = progress.lock()
        && progress.session_state != Some(state)
    {
        progress.session_state = Some(state);
        tracing::info!(?state, "Android Auto session state");
    }
}

fn set_stage(progress: &SharedProgress, stage: WorkerStage) {
    if let Ok(mut progress) = progress.lock()
        && progress.stage != stage
    {
        progress.stage = stage;
        tracing::info!(?stage, "Android Auto worker stage");
    }
}

fn update_progress(progress: &SharedProgress, update: impl FnOnce(&mut WorkerProgress)) {
    if let Ok(mut progress) = progress.lock() {
        update(&mut progress);
    }
}

fn update_audio_status(status: &SharedAudioStatus, update: impl FnOnce(&mut AudioRuntimeSnapshot)) {
    if let Ok(mut status) = status.lock() {
        update(&mut status);
    }
}

#[derive(Debug, Default)]
struct H264Adapter {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    configuration: Option<VideoCodecConfiguration>,
    synthetic_timestamp: u64,
}

#[derive(Debug)]
struct AdaptedVideo {
    configuration: Option<VideoCodecConfiguration>,
    frame: Option<EncryptedVideoFrame>,
}

impl H264Adapter {
    fn adapt(
        &mut self,
        timestamp_ns: Option<u64>,
        annex_b: &[u8],
    ) -> std::result::Result<AdaptedVideo, String> {
        let units = annex_b_units(annex_b)?;
        let mut parameter_sets_changed = false;
        for unit in &units {
            match unit.first().map(|byte| byte & 0x1f) {
                Some(7) if self.sps.as_deref() != Some(*unit) => {
                    self.sps = Some((*unit).to_vec());
                    parameter_sets_changed = true;
                }
                Some(8) if self.pps.as_deref() != Some(*unit) => {
                    self.pps = Some((*unit).to_vec());
                    parameter_sets_changed = true;
                }
                Some(_) | None => {}
            }
        }

        let configuration = if parameter_sets_changed || self.configuration.is_none() {
            match (&self.sps, &self.pps) {
                (Some(sps), Some(pps)) => {
                    let configuration = h264_avcc(sps, pps)?;
                    self.configuration = Some(configuration.clone());
                    Some(configuration)
                }
                _ => None,
            }
        } else {
            None
        };

        let has_picture = units.iter().any(|unit| {
            unit.first()
                .is_some_and(|byte| matches!(byte & 0x1f, 1..=5))
        });
        let frame = if has_picture && self.configuration.is_some() {
            let mut data = Vec::with_capacity(annex_b.len());
            let mut descriptors = Vec::with_capacity(units.len());
            for unit in units {
                let length = u32::try_from(unit.len())
                    .map_err(|_| "H.264 NAL unit exceeds the 32-bit AVCC bound".to_owned())?;
                data.extend_from_slice(&length.to_be_bytes());
                let payload_offset = data.len();
                data.extend_from_slice(unit);
                descriptors.push(NalUnitDescriptor {
                    payload_offset,
                    payload_length: unit.len(),
                    unit_type: unit[0] & 0x1f,
                });
            }
            self.synthetic_timestamp = self.synthetic_timestamp.saturating_add(1);
            Some(EncryptedVideoFrame {
                data,
                codec: WireVideoCodec::H264,
                nal_units: descriptors,
                presentation_time_ns: timestamp_ns,
                nonce_counter: self.synthetic_timestamp,
            })
        } else {
            None
        };

        Ok(AdaptedVideo {
            configuration,
            frame,
        })
    }
}

fn annex_b_units(bytes: &[u8]) -> std::result::Result<Vec<&[u8]>, String> {
    if bytes.is_empty() || bytes.len() > MAX_H264_ACCESS_UNIT_BYTES {
        return Err("H.264 Annex-B access unit is empty or oversized".to_owned());
    }
    let mut starts = Vec::new();
    let mut cursor = 0;
    while cursor + 3 <= bytes.len() {
        let code_length = if cursor + 4 <= bytes.len() && bytes[cursor..cursor + 4] == [0, 0, 0, 1]
        {
            Some(4)
        } else if bytes[cursor..cursor + 3] == [0, 0, 1] {
            Some(3)
        } else {
            None
        };
        if let Some(code_length) = code_length {
            starts.push((cursor, code_length));
            if starts.len() > MAX_H264_NAL_UNITS {
                return Err("H.264 access unit exceeds the NAL-unit count bound".to_owned());
            }
            cursor += code_length;
        } else {
            cursor += 1;
        }
    }
    let Some((first_start, _)) = starts.first().copied() else {
        return Err("H.264 access unit has no Annex-B start code".to_owned());
    };
    if bytes[..first_start].iter().any(|byte| *byte != 0) {
        return Err("H.264 access unit has non-zero data before its first start code".to_owned());
    }

    let mut units = Vec::with_capacity(starts.len());
    for (index, (start, code_length)) in starts.iter().copied().enumerate() {
        let payload_start = start + code_length;
        let payload_end = starts.get(index + 1).map_or(bytes.len(), |(next, _)| *next);
        let mut unit = &bytes[payload_start..payload_end];
        while unit.last() == Some(&0) {
            unit = &unit[..unit.len() - 1];
        }
        if unit.is_empty() {
            return Err("H.264 access unit contains an empty NAL unit".to_owned());
        }
        units.push(unit);
    }
    Ok(units)
}

fn h264_avcc(sps: &[u8], pps: &[u8]) -> std::result::Result<VideoCodecConfiguration, String> {
    if sps.len() < 4 || sps[0] & 0x1f != 7 || pps.is_empty() || pps[0] & 0x1f != 8 {
        return Err("H.264 SPS/PPS parameter sets are malformed".to_owned());
    }
    let sps_length = u16::try_from(sps.len())
        .map_err(|_| "H.264 SPS exceeds the AVCC 16-bit bound".to_owned())?;
    let pps_length = u16::try_from(pps.len())
        .map_err(|_| "H.264 PPS exceeds the AVCC 16-bit bound".to_owned())?;
    let mut codec_data = Vec::with_capacity(11 + sps.len() + pps.len());
    codec_data.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
    codec_data.extend_from_slice(&sps_length.to_be_bytes());
    codec_data.extend_from_slice(sps);
    codec_data.push(1);
    codec_data.extend_from_slice(&pps_length.to_be_bytes());
    codec_data.extend_from_slice(pps);
    Ok(VideoCodecConfiguration {
        codec: WireVideoCodec::H264,
        codec_data,
        nal_length_bytes: 4,
    })
}

fn pcm_s16le_to_be(pcm: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if pcm.is_empty() || pcm.len() % 2 != 0 {
        return Err("Android Auto PCM is empty or not aligned to signed 16-bit samples".to_owned());
    }
    let mut converted = pcm.to_vec();
    for sample in converted.chunks_exact_mut(2) {
        sample.swap(0, 1);
    }
    Ok(converted)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioClock {
    sequence: u16,
    next_timestamp: u32,
}

impl AudioClock {
    pub(crate) const fn new() -> Self {
        Self {
            sequence: 0,
            next_timestamp: 0,
        }
    }

    pub(crate) fn frame(
        &mut self,
        stream_id: AudioStreamId,
        format_id: AudioFormatId,
        sample_rate_hz: u32,
        channels: u8,
        pcm_s16le: &[u8],
    ) -> std::result::Result<EncryptedAudioFrame, String> {
        let payload = pcm_s16le_to_be(pcm_s16le)?;
        let timestamp = self.next_timestamp;
        let sample_count = u32::try_from(payload.len() / 2 / usize::from(channels))
            .map_err(|_| "Android Auto PCM sample count overflows u32".to_owned())?;
        self.next_timestamp = timestamp.wrapping_add(sample_count);
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut rtp_packet = Vec::with_capacity(12 + payload.len());
        rtp_packet.extend_from_slice(&[
            0x80,
            stream_id as u8,
            (sequence >> 8) as u8,
            sequence as u8,
        ]);
        rtp_packet.extend_from_slice(&timestamp.to_be_bytes());
        rtp_packet.extend_from_slice(&0_u32.to_be_bytes());
        rtp_packet.extend_from_slice(&payload);
        Ok(EncryptedAudioFrame {
            stream_id,
            format_id,
            codec: EncodedAudioCodec::PcmS16Be {
                sample_rate_hz,
                channels,
            },
            sequence,
            timestamp,
            ssrc: 0,
            nonce_counter: u64::from(sequence),
            payload,
            rtp_packet,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AudioIngressWindow {
    packets: u64,
    bytes: u64,
    peak: u16,
    max_gap: Duration,
    last_arrival: Option<Instant>,
    first_phone_micros: Option<u64>,
    last_phone_micros: Option<u64>,
}

impl AudioIngressWindow {
    fn record(&mut self, now: Instant, phone_micros: Option<u64>, pcm_s16le: &[u8]) {
        self.packets = self.packets.saturating_add(1);
        self.bytes = self.bytes.saturating_add(pcm_s16le.len() as u64);
        for sample in pcm_s16le.chunks_exact(2) {
            self.peak = self
                .peak
                .max(i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs());
        }
        if let Some(last) = self.last_arrival {
            self.max_gap = self.max_gap.max(now.saturating_duration_since(last));
        }
        self.last_arrival = Some(now);
        if phone_micros.is_some() {
            self.first_phone_micros = self.first_phone_micros.or(phone_micros);
            self.last_phone_micros = phone_micros;
        }
    }

    fn audio_millis(&self, sample_rate_hz: u32, channels: u8) -> u64 {
        self.bytes * 1_000 / (u64::from(sample_rate_hz) * u64::from(channels) * 2)
    }

    fn phone_span_millis(&self) -> Option<u64> {
        Some(
            self.last_phone_micros?
                .saturating_sub(self.first_phone_micros?)
                / 1_000,
        )
    }

    fn restart(&mut self) {
        *self = Self {
            last_arrival: self.last_arrival,
            ..Self::default()
        };
    }
}

#[derive(Debug)]
struct InputMapper {
    touches: BTreeMap<u8, TouchPointer>,
    siri_longpress_sent: bool,
    presets: PresetBindings,
}

#[derive(Debug, PartialEq, Eq)]
enum InputDisposition {
    Protocol(Vec<Command>),
    LocalMute,
    Ignore,
    Reset(Vec<Command>),
}

impl InputMapper {
    fn new(presets: PresetBindings) -> Self {
        Self {
            touches: BTreeMap::new(),
            siri_longpress_sent: false,
            presets,
        }
    }

    fn map(&mut self, event: PlatformInputEvent) -> InputDisposition {
        let timestamp_micros = event.timestamp.0;
        match event.kind {
            PlatformInputEventKind::Touch {
                contact_id,
                phase,
                x,
                y,
            } => InputDisposition::Protocol(self.map_touch(
                timestamp_micros,
                contact_id,
                phase,
                x,
                y,
            )),
            PlatformInputEventKind::KnobButton { button, pressed } => {
                InputDisposition::Protocol(vec![Command::Button {
                    timestamp_micros,
                    button: match button {
                        KnobInputButton::Back => ProtocolButton::Back,
                        KnobInputButton::Select => ProtocolButton::DpadCenter,
                    },
                    down: pressed,
                    longpress: false,
                }])
            }
            PlatformInputEventKind::Rotary { direction, steps } => {
                let direction = match direction {
                    RotaryDirection::Clockwise => 1,
                    RotaryDirection::CounterClockwise => -1,
                };
                InputDisposition::Protocol(
                    (0..steps)
                        .map(|_| Command::Rotary {
                            timestamp_micros,
                            direction,
                        })
                        .collect(),
                )
            }
            PlatformInputEventKind::SiriButton { pressed } => {
                if pressed {
                    self.siri_longpress_sent = true;
                    InputDisposition::Protocol(vec![Command::Button {
                        timestamp_micros,
                        button: ProtocolButton::Search,
                        down: true,
                        longpress: true,
                    }])
                } else if self.siri_longpress_sent {
                    self.siri_longpress_sent = false;
                    InputDisposition::Protocol(Vec::new())
                } else {
                    InputDisposition::Protocol(vec![Command::Button {
                        timestamp_micros,
                        button: ProtocolButton::Search,
                        down: false,
                        longpress: false,
                    }])
                }
            }
            PlatformInputEventKind::VoiceAssistantCancel => {
                self.siri_longpress_sent = false;
                InputDisposition::Protocol(vec![Command::Button {
                    timestamp_micros,
                    button: ProtocolButton::Search,
                    down: false,
                    longpress: false,
                }])
            }
            PlatformInputEventKind::SettingsButton => InputDisposition::Ignore,
            PlatformInputEventKind::Local(LocalInputAction::Mute) => InputDisposition::LocalMute,
            PlatformInputEventKind::Local(LocalInputAction::Preset(preset)) => {
                let commands = match self.presets.action(preset) {
                    PresetAction::None => return InputDisposition::Ignore,
                    PresetAction::Mute => return InputDisposition::LocalMute,
                    PresetAction::PreviousTrack => {
                        media_key_pair(timestamp_micros, MediaKey::Previous)
                    }
                    PresetAction::PlayPause => {
                        media_key_pair(timestamp_micros, MediaKey::PlayPause)
                    }
                    PresetAction::NextTrack => media_key_pair(timestamp_micros, MediaKey::Next),
                    PresetAction::Home => button_pair(timestamp_micros, ProtocolButton::Home),
                    PresetAction::Back => button_pair(timestamp_micros, ProtocolButton::Back),
                    PresetAction::VoiceAssistant => {
                        button_pair(timestamp_micros, ProtocolButton::Search)
                    }
                    PresetAction::AnswerCall => {
                        button_pair(timestamp_micros, ProtocolButton::PhoneAccept)
                    }
                    PresetAction::EndCall => {
                        button_pair(timestamp_micros, ProtocolButton::PhoneDecline)
                    }
                };
                InputDisposition::Protocol(commands)
            }
            PlatformInputEventKind::Reset { .. } => {
                self.siri_longpress_sent = false;
                let mut commands = Vec::new();
                while let Some((&contact_id, _)) = self.touches.last_key_value() {
                    let pointers = self.touches.values().copied().collect::<Vec<_>>();
                    let action_index = u32::try_from(pointers.len().saturating_sub(1)).unwrap_or(0);
                    commands.push(Command::Touch {
                        timestamp_micros,
                        action: if pointers.len() == 1 {
                            TouchAction::Up
                        } else {
                            TouchAction::PointerUp
                        },
                        action_index,
                        pointers,
                    });
                    self.touches.remove(&contact_id);
                }
                InputDisposition::Reset(commands)
            }
        }
    }

    fn map_touch(
        &mut self,
        timestamp_micros: u64,
        contact_id: u8,
        phase: TouchInputPhase,
        x: u16,
        y: u16,
    ) -> Vec<Command> {
        let was_empty = self.touches.is_empty();
        self.touches.insert(
            contact_id,
            TouchPointer {
                id: u32::from(contact_id),
                x: u32::from(x),
                y: u32::from(y),
            },
        );
        let pointers = self.touches.values().cloned().collect::<Vec<_>>();
        let action_index = self
            .touches
            .keys()
            .position(|id| *id == contact_id)
            .and_then(|index| u32::try_from(index).ok())
            .unwrap_or(0);
        let action = match phase {
            TouchInputPhase::Down if was_empty => TouchAction::Down,
            TouchInputPhase::Down => TouchAction::PointerDown,
            TouchInputPhase::Move => TouchAction::Moved,
            TouchInputPhase::Up if self.touches.len() == 1 => TouchAction::Up,
            TouchInputPhase::Up => TouchAction::PointerUp,
        };
        if phase == TouchInputPhase::Up {
            self.touches.remove(&contact_id);
        }
        vec![Command::Touch {
            timestamp_micros,
            action,
            action_index,
            pointers,
        }]
    }
}

fn media_key_pair(timestamp_micros: u64, key: MediaKey) -> Vec<Command> {
    vec![
        Command::MediaKey {
            timestamp_micros,
            key,
            down: true,
        },
        Command::MediaKey {
            timestamp_micros,
            key,
            down: false,
        },
    ]
}

fn button_pair(timestamp_micros: u64, button: ProtocolButton) -> Vec<Command> {
    [true, false]
        .map(|down| Command::Button {
            timestamp_micros,
            button,
            down,
            longpress: false,
        })
        .to_vec()
}

fn apply_local_mute(
    current: bool,
    apply: impl FnOnce(bool) -> std::result::Result<(), String>,
) -> std::result::Result<bool, String> {
    let next = !current;
    apply(next)?;
    Ok(next)
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
struct LiveMedia {
    video: Option<LinuxVideoBackend>,
    video_configuration: Option<VideoCodecConfiguration>,
    video_adapter: H264Adapter,
    video_telemetry: VideoSynchronizationTelemetry,
    display: Option<LinuxDisplayHandle>,
    audio: Option<LinuxAudioBackend>,
    audio_clocks: [AudioClock; 3],
    audio_ingress: [AudioIngressWindow; 3],
    microphone: Option<LinuxMicrophoneBackend>,
    calls: Option<CallSubscription>,
    call: Option<ActiveCall>,
    input: LinuxInputBackend,
    input_mapper: InputMapper,
    input_suppressed: bool,
    last_input_timestamp: u64,
    local_muted: bool,
    status: SharedAudioStatus,
    progress: SharedProgress,
    next_video_diagnostic: Instant,
    audio_diagnostic_started: Instant,
    media_busy: Duration,
    media_busy_max: Duration,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
#[derive(Debug)]
struct ActiveCall {
    bridge: CallBridge,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
impl fmt::Debug for LiveMedia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveMedia")
            .field("video_started", &self.video.is_some())
            .field("audio", &self.audio)
            .field("microphone_started", &self.microphone.is_some())
            .field("call", &self.call)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
impl LiveMedia {
    fn start(
        config: &MediaConfig,
        audio_output: bool,
        display: Option<LinuxDisplayHandle>,
        status: SharedAudioStatus,
        progress: SharedProgress,
        hands_free: Option<&HandsFree>,
        presets: PresetBindings,
    ) -> std::result::Result<Self, String> {
        if config.video != VideoBackend::AmlogicV4l2
            || config.audio != AudioBackend::AlsaUsb
            || config.microphone != AudioBackend::BuiltInSuperbird
        {
            return Err(
                "Android Auto requires Amlogic H.264, USB ALSA, and built-in microphone backends"
                    .to_owned(),
            );
        }
        let video_config = LinuxVideoBackendConfig::default();
        LinuxVideoBackend::preflight_with_display(video_config, display.as_ref())
            .map_err(|error| format!("Android Auto video preflight failed: {error}"))?;
        let streams = [
            AudioStreamConfig::with_negotiated_latency(
                AudioStreamId::Main,
                AUDIO_MEDIA_FORMAT,
                Some(AUDIO_PLAYOUT_LATENCY_MS),
            ),
            AudioStreamConfig::with_negotiated_latency(
                AudioStreamId::Alternate,
                AUDIO_VOICE_FORMAT,
                Some(AUDIO_PLAYOUT_LATENCY_MS),
            ),
            AudioStreamConfig::with_negotiated_latency(
                AudioStreamId::High,
                AUDIO_VOICE_FORMAT,
                Some(AUDIO_PLAYOUT_LATENCY_MS),
            ),
        ];
        let audio =
            if audio_output {
                let matcher = config
                    .usb_dac
                    .as_ref()
                    .ok_or_else(|| "Android Auto USB DAC matcher is not configured".to_owned())?
                    .matcher()
                    .map_err(|error| format!("Android Auto USB DAC matcher is invalid: {error}"))?;
                let mut audio_config = LinuxAudioBackendConfig::new(
                    matcher,
                    streams
                        .iter()
                        .map(|stream| (stream.stream_id, stream.format_id))
                        .collect(),
                );
                audio_config.equalizer = config.equalizer.clone();
                audio_config.command_capacity = 64;
                Some(LinuxAudioBackend::start(audio_config).map_err(|error| {
                    format!("starting Android Auto audio backend failed: {error}")
                })?)
            } else {
                None
            };
        let input = LinuxInputBackend::discover(LinuxInputConfig {
            poll_interval: Duration::from_millis(config.socket_poll_interval_ms.clamp(1, 1_000)),
            ..LinuxInputConfig::default()
        })
        .map_err(|error| format!("opening Android Auto input backend failed: {error}"))?;
        let video_telemetry = VideoSynchronizationTelemetry::new(video_config.output_delay);
        let calls = audio.as_ref().and(hands_free).map(HandsFree::subscribe);
        update_audio_status(&status, |status| {
            if audio.is_none() {
                *status = AudioRuntimeSnapshot::disabled();
                return;
            }
            *status = AudioRuntimeSnapshot::waiting(AudioRuntimeRoute::UsbDac);
            status.pipeline_state = AudioPipelineState::Playing;
            status.active_route = Some(AudioRuntimeRoute::UsbDac);
            status.pipeline_generation = status.pipeline_generation.saturating_add(1).max(1);
            status.negotiated_streams = streams.len();
            status.recording = true;
            status.microphone.negotiated = false;
            status.microphone.capture_running = false;
            status.microphone.ready = false;
        });
        update_progress(&progress, |progress| {
            progress.audio_ready = true;
            progress.input_ready = true;
            progress.microphone_ready |= !audio_output;
        });
        Ok(Self {
            video: None,
            video_configuration: None,
            video_adapter: H264Adapter::default(),
            video_telemetry,
            display,
            audio,
            audio_clocks: [AudioClock::new(), AudioClock::new(), AudioClock::new()],
            audio_ingress: [AudioIngressWindow::default(); 3],
            microphone: None,
            calls,
            call: None,
            input,
            input_mapper: InputMapper::new(presets),
            input_suppressed: false,
            last_input_timestamp: 0,
            local_muted: false,
            status,
            progress,
            next_video_diagnostic: Instant::now() + VIDEO_DIAGNOSTIC_INTERVAL,
            audio_diagnostic_started: Instant::now(),
            media_busy: Duration::ZERO,
            media_busy_max: Duration::ZERO,
        })
    }

    fn handle_event<T: std::io::Read + std::io::Write + Send>(
        &mut self,
        event: Event,
        session: &mut Session<T>,
    ) -> std::result::Result<bool, String> {
        match event {
            Event::Connected => Ok(true),
            Event::Disconnected { .. } => Ok(false),
            Event::Video { timestamp, annex_b } => {
                self.handle_video(timestamp, &annex_b, session)?;
                Ok(true)
            }
            Event::Audio {
                channel,
                timestamp,
                pcm_s16le,
            } => {
                self.handle_audio(channel, timestamp, &pcm_s16le)?;
                Ok(true)
            }
            Event::Microphone {
                open,
                sample_rate,
                channels,
            } => {
                self.handle_microphone(open, sample_rate, channels)?;
                Ok(true)
            }
            Event::VoiceSession { active } => {
                if let Some(audio) = self.audio.as_ref() {
                    audio.set_ducking(active).map_err(|error| {
                        format!("applying Android Auto voice-session ducking failed: {error}")
                    })?;
                    update_audio_status(&self.status, |status| status.ducked = active);
                }
                Ok(true)
            }
            Event::Battery { .. }
            | Event::BluetoothPairing { .. }
            | Event::BluetoothAuthenticated { .. } => Ok(true),
        }
    }

    fn handle_video<T: std::io::Read + std::io::Write + Send>(
        &mut self,
        timestamp: Option<u64>,
        annex_b: &[u8],
        session: &mut Session<T>,
    ) -> std::result::Result<(), String> {
        let adapted = self.video_adapter.adapt(timestamp, annex_b)?;
        if let Some(configuration) = adapted.configuration {
            if let Some(video) = self.video.as_ref() {
                video
                    .submit_configuration(configuration.clone())
                    .map_err(|error| {
                        format!("updating Android Auto H.264 configuration failed: {error}")
                    })?;
            } else {
                self.video = Some(
                    LinuxVideoBackend::start_with_display(
                        LinuxVideoBackendConfig::default(),
                        configuration.clone(),
                        self.video_telemetry.clone(),
                        self.display.clone(),
                    )
                    .map_err(|error| {
                        format!("starting Android Auto video backend failed: {error}")
                    })?,
                );
            }
            self.video_configuration = Some(configuration);
        }
        let Some(frame) = adapted.frame else {
            if self.video_configuration.is_none() {
                session.send(Command::RequestKeyFrame).map_err(|error| {
                    format!("requesting initial Android Auto keyframe failed: {error}")
                })?;
                update_progress(&self.progress, |progress| {
                    progress.keyframe_requests = progress.keyframe_requests.saturating_add(1);
                });
            }
            return Ok(());
        };
        let video = self.video.as_ref().ok_or_else(|| {
            "Android Auto video frame arrived before a complete SPS/PPS configuration".to_owned()
        })?;
        let disposition = video
            .submit_frame(frame)
            .map_err(|error| format!("submitting Android Auto H.264 frame failed: {error}"))?;
        if matches!(
            disposition,
            VideoFrameDisposition::Queued | VideoFrameDisposition::ReplacedStaleFrame
        ) {
            update_progress(&self.progress, |progress| {
                progress.video_ready = true;
            });
        }
        if matches!(
            disposition,
            VideoFrameDisposition::DroppedAwaitingKeyframe
                | VideoFrameDisposition::DroppedQueueFull
        ) {
            session.send(Command::RequestKeyFrame).map_err(|error| {
                format!("requesting Android Auto recovery keyframe failed: {error}")
            })?;
            update_progress(&self.progress, |progress| {
                progress.keyframe_requests = progress.keyframe_requests.saturating_add(1);
            });
        }
        update_progress(&self.progress, |progress| {
            progress.video_frames = progress.video_frames.saturating_add(1);
        });
        Ok(())
    }

    fn handle_audio(
        &mut self,
        channel: ProtocolAudioChannel,
        timestamp_micros: Option<u64>,
        pcm_s16le: &[u8],
    ) -> std::result::Result<(), String> {
        let Some(audio) = self.audio.as_ref() else {
            return Ok(());
        };
        if self.call.is_some() && matches!(channel, ProtocolAudioChannel::System) {
            return Ok(());
        }
        let (clock_index, stream_id, format_id, sample_rate_hz, channels) = match channel {
            ProtocolAudioChannel::Media => (
                0,
                AudioStreamId::Main,
                AUDIO_MEDIA_FORMAT,
                AUDIO_MEDIA_RATE,
                AUDIO_MEDIA_CHANNELS,
            ),
            ProtocolAudioChannel::Guidance => (
                1,
                AudioStreamId::Alternate,
                AUDIO_VOICE_FORMAT,
                AUDIO_VOICE_RATE,
                AUDIO_VOICE_CHANNELS,
            ),
            ProtocolAudioChannel::System => (
                SYSTEM_CLOCK,
                AudioStreamId::High,
                AUDIO_VOICE_FORMAT,
                AUDIO_VOICE_RATE,
                AUDIO_VOICE_CHANNELS,
            ),
        };
        self.audio_ingress[clock_index].record(Instant::now(), timestamp_micros, pcm_s16le);
        let frame = self.audio_clocks[clock_index].frame(
            stream_id,
            format_id,
            sample_rate_hz,
            channels,
            pcm_s16le,
        )?;
        audio.try_submit(frame).map_err(|error| {
            update_audio_status(&self.status, |status| {
                status.ingress_full_events = status.ingress_full_events.saturating_add(1);
                status.last_failure = Some(AudioFailureKind::IngressBackpressure);
            });
            format!("submitting Android Auto PCM failed: {error}")
        })?;
        update_audio_status(&self.status, |status| {
            status.frames_accepted = status.frames_accepted.saturating_add(1);
        });
        update_progress(&self.progress, |progress| {
            progress.audio_frames = progress.audio_frames.saturating_add(1);
        });
        Ok(())
    }

    fn handle_microphone(
        &mut self,
        open: bool,
        sample_rate: u32,
        channels: u8,
    ) -> std::result::Result<(), String> {
        if !open {
            if let Some(microphone) = self.microphone.take() {
                microphone
                    .shutdown()
                    .map_err(|error| format!("stopping Android Auto microphone failed: {error}"))?;
            }
            update_audio_status(&self.status, |status| {
                status.microphone.capture_running = false;
                status.microphone.ready = false;
            });
            return Ok(());
        }
        if sample_rate != AUDIO_VOICE_RATE || channels != AUDIO_VOICE_CHANNELS {
            return Err(format!(
                "phone requested unsupported Android Auto microphone format {sample_rate}Hz/{channels}ch"
            ));
        }
        if self.call.is_some() {
            tracing::info!("the microphone belongs to the Bluetooth call; ignoring Android Auto");
            return Ok(());
        }
        if self.microphone.is_some() {
            return Ok(());
        }
        let packetizer = MicrophonePacketizer::new_raw_pcm_s16le(MicrophoneConfig {
            stream_id: AudioStreamId::Main,
            codec: MicrophoneCodec::RawPcmS16Le,
            sample_rate_hz: AUDIO_VOICE_RATE,
            channels: AUDIO_VOICE_CHANNELS,
            frame_duration_micros: MICROPHONE_FRAME_DURATION_MICROS,
            maximum_encoded_frame_bytes: MICROPHONE_MAXIMUM_FRAME_BYTES,
            maximum_pcm_buffer_bytes: MICROPHONE_MAXIMUM_BUFFER_BYTES,
        })
        .map_err(|error| {
            format!("configuring Android Auto microphone packetizer failed: {error}")
        })?;
        let microphone =
            match LinuxMicrophoneBackend::start(LinuxMicrophoneBackendConfig::with_route(
                packetizer,
                MicrophoneInputRoute::BuiltInSuperbird,
            )) {
                Ok(microphone) => microphone,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "Android Auto microphone capture could not start; keeping projection alive"
                    );
                    update_audio_status(&self.status, |status| {
                        status.microphone.capture_running = false;
                        status.microphone.ready = false;
                    });
                    return Ok(());
                }
            };
        self.microphone = Some(microphone);
        update_audio_status(&self.status, |status| {
            status.microphone.negotiated = true;
            status.microphone.capture_running = true;
            status.microphone.ready = false;
        });
        update_progress(&self.progress, |progress| {
            progress.microphone_negotiated = true;
        });
        Ok(())
    }

    fn poll<T: std::io::Read + std::io::Write + Send>(
        &mut self,
        session: &mut Session<T>,
        controls: &Receiver<WorkerControl>,
    ) -> std::result::Result<(), String> {
        loop {
            match controls.try_recv() {
                Ok(WorkerControl::TestTone(reply)) => {
                    let result = self.audio.as_ref().map_or_else(
                        || Err("Android Auto audio stays on the phone".to_owned()),
                        |audio| {
                            audio.play_test_tone().map_err(|error| {
                                format!("playing Android Auto test tone failed: {error}")
                            })
                        },
                    );
                    let _ = reply.try_send(result);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let batch = self
            .input
            .poll()
            .map_err(|error| format!("polling Android Auto input failed: {error}"))?;
        for event in &batch.events {
            self.last_input_timestamp = self.last_input_timestamp.max(event.timestamp.0);
        }
        let input_suppressed = self
            .display
            .as_ref()
            .is_some_and(LinuxDisplayHandle::host_ui_active);
        if input_suppressed && !self.input_suppressed {
            let reset = self.input_mapper.map(PlatformInputEvent {
                timestamp: MonotonicMicros(self.last_input_timestamp),
                kind: PlatformInputEventKind::Reset {
                    role: DeviceRole::Touch,
                    reason: ResetReason::Cancelled,
                },
            });
            let InputDisposition::Reset(commands) = reset else {
                return Err(
                    "Android Auto input reset did not produce a reset disposition".to_owned(),
                );
            };
            for command in commands {
                session.send(command).map_err(|error| {
                    format!("releasing Android Auto input for host settings failed: {error}")
                })?;
                update_progress(&self.progress, |progress| {
                    progress.input_events = progress.input_events.saturating_add(1);
                });
            }
        }
        self.input_suppressed = input_suppressed;
        for event in batch.events {
            if input_suppressed {
                continue;
            }
            match self.input_mapper.map(event) {
                InputDisposition::Protocol(commands) | InputDisposition::Reset(commands) => {
                    for command in commands {
                        session.send(command).map_err(|error| {
                            format!("sending Android Auto input failed: {error}")
                        })?;
                        update_progress(&self.progress, |progress| {
                            progress.input_events = progress.input_events.saturating_add(1);
                        });
                    }
                }
                InputDisposition::LocalMute => {
                    let Some(audio) = self.audio.as_ref() else {
                        continue;
                    };
                    let muted = apply_local_mute(self.local_muted, |muted| {
                        audio.set_muted(muted).map_err(|error| {
                            format!("applying local Android Auto USB-DAC mute failed: {error}")
                        })
                    })?;
                    self.local_muted = muted;
                    update_audio_status(&self.status, |status| status.muted = muted);
                    update_progress(&self.progress, |progress| {
                        progress.input_events = progress.input_events.saturating_add(1);
                    });
                }
                InputDisposition::Ignore => {}
            }
        }
        if let Some(terminal) = batch.terminal {
            return Err(format!("Android Auto input backend stopped: {terminal:?}"));
        }

        if let Some(video) = self.video.as_ref() {
            if let Some(error) = video
                .try_error()
                .map_err(|error| format!("polling Android Auto video errors failed: {error}"))?
            {
                return Err(format!(
                    "Android Auto video backend {} failed: {}",
                    error.component, error.message
                ));
            }
            while let Some(event) = video
                .try_event()
                .map_err(|error| format!("polling Android Auto video events failed: {error}"))?
            {
                match event {
                    VideoBackendEvent::RequestKeyFrame => {
                        session.send(Command::RequestKeyFrame).map_err(|error| {
                            format!("sending Android Auto backend keyframe request failed: {error}")
                        })?;
                        update_progress(&self.progress, |progress| {
                            progress.keyframe_requests =
                                progress.keyframe_requests.saturating_add(1);
                        });
                    }
                    VideoBackendEvent::FirstFrameRendered => {
                        tracing::info!("the first Android Auto video frame is on screen");
                    }
                }
            }
        }
        let audio_failure = self
            .audio
            .as_ref()
            .map(|audio| {
                audio
                    .try_error()
                    .map_err(|error| format!("polling Android Auto audio errors failed: {error}"))
            })
            .transpose()?
            .flatten();
        if let Some(error) = audio_failure {
            return Err(format!(
                "Android Auto audio backend {} failed: {}",
                error.component, error.message
            ));
        }
        let microphone_failure = self
            .microphone
            .as_ref()
            .map(|microphone| {
                microphone.try_error().map_err(|error| {
                    format!("polling Android Auto microphone errors failed: {error}")
                })
            })
            .transpose()?
            .flatten();
        if let Some(error) = microphone_failure {
            tracing::warn!(
                component = error.component,
                error = %error.message,
                "Android Auto microphone backend stopped; keeping projection alive"
            );
            if let Some(microphone) = self.microphone.take()
                && let Err(shutdown_error) = microphone.shutdown()
            {
                tracing::warn!(
                    error = %shutdown_error,
                    "stopping failed Android Auto microphone backend also failed"
                );
            }
            update_audio_status(&self.status, |status| {
                status.microphone.capture_running = false;
                status.microphone.ready = false;
                status.microphone.send_failure_events =
                    status.microphone.send_failure_events.saturating_add(1);
            });
        }
        if let Some(microphone) = self.microphone.as_ref() {
            while let Some(event) = microphone.try_event().map_err(|error| {
                format!("polling Android Auto microphone events failed: {error}")
            })? {
                match event {
                    MicrophoneBackendEvent::Packet { packet, level } => {
                        let encoded_len = packet.encoded.len();
                        match session.send(Command::MicrophonePcm {
                            timestamp: microphone_timestamp_micros()?,
                            pcm_s16le: packet.encoded,
                        }) {
                            Ok(()) => {
                                update_audio_status(&self.status, |status| {
                                    status.microphone.packets_sent =
                                        status.microphone.packets_sent.saturating_add(1);
                                    status.microphone.bytes_sent = status
                                        .microphone
                                        .bytes_sent
                                        .saturating_add(encoded_len as u64);
                                    status.microphone.ready = true;
                                    if let Some(level) = level {
                                        status.microphone.rms_q15 = Some(level.rms_q15);
                                        status.microphone.peak_q15 = Some(level.peak_q15);
                                    }
                                });
                                update_progress(&self.progress, |progress| {
                                    progress.microphone_packets =
                                        progress.microphone_packets.saturating_add(1);
                                    progress.microphone_ready = true;
                                });
                            }
                            Err(prelude_android_auto_protocol::Error::Backpressure) => {
                                update_audio_status(&self.status, |status| {
                                    status.microphone.send_failure_events =
                                        status.microphone.send_failure_events.saturating_add(1);
                                });
                                tracing::debug!(
                                    "Android Auto microphone flow-control window is full; dropping one capture frame"
                                );
                            }
                            Err(error) => {
                                update_audio_status(&self.status, |status| {
                                    status.microphone.send_failure_events =
                                        status.microphone.send_failure_events.saturating_add(1);
                                });
                                return Err(format!(
                                    "sending Android Auto microphone PCM failed: {error}"
                                ));
                            }
                        }
                    }
                    MicrophoneBackendEvent::Stopped => {
                        update_audio_status(&self.status, |status| {
                            status.microphone.capture_running = false;
                            status.microphone.ready = false;
                        });
                    }
                }
            }
        }
        self.poll_call();
        self.log_audio_diagnostic();
        self.log_video_diagnostic();
        self.refresh_status();
        Ok(())
    }

    fn poll_call(&mut self) {
        if self
            .call
            .as_ref()
            .is_some_and(|call| call.bridge.is_finished())
        {
            self.end_call();
        }
        if let Some(audio) = self.calls.as_ref().and_then(CallSubscription::try_next) {
            self.end_call();
            self.start_call(audio);
        }
    }

    fn start_call(&mut self, audio: CallAudio) {
        let Some(frames) = self
            .audio
            .as_ref()
            .and_then(|audio| audio.frame_sender(AudioStreamId::High))
        else {
            return;
        };
        if let Some(microphone) = self.microphone.take() {
            if let Err(error) = microphone.shutdown() {
                tracing::warn!(%error, "stopping the assistant microphone for a call failed");
            }
            update_audio_status(&self.status, |status| {
                status.microphone.capture_running = false;
                status.microphone.ready = false;
            });
        }
        let mtu = audio.mtu();
        let codec = audio.codec();
        let microphone = match start_call_microphone(audio.sample_rate()) {
            Ok(microphone) => Some(microphone),
            Err(error) => {
                tracing::warn!(%error, "the call microphone could not start; the caller hears silence");
                None
            }
        };
        let has_microphone = microphone.is_some();
        let bridge =
            match CallBridge::start(audio, frames, self.audio_clocks[SYSTEM_CLOCK], microphone) {
                Ok(bridge) => bridge,
                Err(error) => {
                    tracing::warn!(%error, "starting Bluetooth call audio failed");
                    return;
                }
            };
        tracing::info!(
            peer = bridge.peer(),
            ?codec,
            mtu,
            microphone = has_microphone,
            "playing Bluetooth call audio on the DAC"
        );
        self.call = Some(ActiveCall { bridge });
    }

    fn end_call(&mut self) {
        let Some(call) = self.call.take() else {
            return;
        };
        tracing::info!(peer = call.bridge.peer(), "Bluetooth call audio ended");
        if let Some(clock) = call.bridge.finish() {
            self.audio_clocks[SYSTEM_CLOCK] = clock;
        }
    }

    fn record_busy(&mut self, started: Instant) {
        let busy = started.elapsed();
        self.media_busy += busy;
        self.media_busy_max = self.media_busy_max.max(busy);
    }

    fn log_audio_diagnostic(&mut self) {
        let now = Instant::now();
        let window = now.saturating_duration_since(self.audio_diagnostic_started);
        if window < VIDEO_DIAGNOSTIC_INTERVAL {
            return;
        }
        self.audio_diagnostic_started = now;
        tracing::info!(
            window_ms = window.as_millis() as u64,
            media_busy_ms = self.media_busy.as_millis() as u64,
            media_busy_max_ms = self.media_busy_max.as_millis() as u64,
            "Android Auto media loop diagnostic"
        );
        self.media_busy = Duration::ZERO;
        self.media_busy_max = Duration::ZERO;
        for (ingress, (channel, sample_rate_hz, channels)) in self.audio_ingress.iter_mut().zip([
            ("media", AUDIO_MEDIA_RATE, AUDIO_MEDIA_CHANNELS),
            ("guidance", AUDIO_VOICE_RATE, AUDIO_VOICE_CHANNELS),
            ("system", AUDIO_VOICE_RATE, AUDIO_VOICE_CHANNELS),
        ]) {
            if ingress.packets != 0 {
                tracing::info!(
                    channel,
                    window_ms = window.as_millis() as u64,
                    packets = ingress.packets,
                    packet_bytes = ingress.bytes / ingress.packets,
                    audio_ms = ingress.audio_millis(sample_rate_hz, channels),
                    phone_span_ms = ingress.phone_span_millis(),
                    max_gap_ms = ingress.max_gap.as_millis() as u64,
                    peak = ingress.peak,
                    "Android Auto audio diagnostic"
                );
            }
            ingress.restart();
        }
    }

    fn log_video_diagnostic(&mut self) {
        let now = Instant::now();
        if now < self.next_video_diagnostic {
            return;
        }
        self.next_video_diagnostic = now + VIDEO_DIAGNOSTIC_INTERVAL;
        let Some(video) = self.video.as_ref() else {
            return;
        };
        let ingress = video.ingress_stats().ok();
        let presented_frames = self
            .video_telemetry
            .snapshot()
            .ok()
            .map(|snapshot| snapshot.presented_frames);
        let progress = self.progress.lock().ok().map(|progress| *progress);
        tracing::info!(
            received_frames = progress.map(|progress| progress.video_frames),
            keyframe_requests = progress.map(|progress| progress.keyframe_requests),
            queued_frames = ingress.map(|stats| stats.queued_frames),
            replaced_frames = ingress.map(|stats| stats.replaced_frames),
            dropped_frames = ingress.map(|stats| stats.dropped_frames),
            presented_frames,
            "Android Auto video diagnostic"
        );
    }

    fn refresh_status(&self) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        let telemetry = audio.telemetry().ok();
        let latency = audio.latency().ok();
        let video = self.video_telemetry.snapshot().ok();
        update_audio_status(&self.status, |status| {
            status.backend = telemetry.map(audio_backend_status);
            status.latency = latency.map(|latency| AudioLatencyStatus {
                default_opus_jitter_ms: latency.configured.opus_jitter_ms,
                default_aac_jitter_ms: latency.configured.aac_jitter_ms,
                sink_buffer_micros: latency.configured.sink_buffer_micros,
                sink_period_micros: latency.configured.sink_period_micros,
                gstreamer_query_live: latency.gstreamer_query_live,
                gstreamer_query_minimum_ns: latency.gstreamer_query_minimum_ns,
                gstreamer_query_maximum_ns: latency.gstreamer_query_maximum_ns,
            });
            status.video_synchronization = video
                .map(video_synchronization_status)
                .unwrap_or_else(VideoSynchronizationStatus::uncompensated);
            status.last_failure = telemetry
                .and_then(|telemetry| telemetry.last_failure)
                .map(audio_failure_kind);
        });
    }

    fn shutdown(mut self) -> std::result::Result<(), String> {
        self.end_call();
        self.calls = None;
        let mut first_error = None;
        if let Some(microphone) = self.microphone.take() {
            if let Err(error) = microphone.shutdown() {
                first_error = Some(format!("stopping Android Auto microphone failed: {error}"));
            }
        }
        if let Some(video) = self.video.take() {
            if let Err(error) = video.shutdown() {
                first_error.get_or_insert_with(|| {
                    format!("stopping Android Auto video backend failed: {error}")
                });
            }
        }
        let audio_output = self.audio.is_some();
        if let Some(audio) = self.audio.take()
            && let Err(error) = audio.shutdown()
        {
            first_error.get_or_insert_with(|| {
                format!("stopping Android Auto audio backend failed: {error}")
            });
        }
        update_audio_status(&self.status, |status| {
            *status = if audio_output {
                AudioRuntimeSnapshot::waiting(AudioRuntimeRoute::UsbDac)
            } else {
                AudioRuntimeSnapshot::disabled()
            };
        });
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn start_call_microphone(
    sample_rate_hz: u32,
) -> std::result::Result<LinuxMicrophoneBackend, String> {
    let packetizer = MicrophonePacketizer::new_raw_pcm_s16le(MicrophoneConfig {
        stream_id: AudioStreamId::Main,
        codec: MicrophoneCodec::RawPcmS16Le,
        sample_rate_hz,
        channels: 1,
        frame_duration_micros: MICROPHONE_FRAME_DURATION_MICROS,
        maximum_encoded_frame_bytes: MICROPHONE_MAXIMUM_FRAME_BYTES,
        maximum_pcm_buffer_bytes: MICROPHONE_MAXIMUM_BUFFER_BYTES,
    })
    .map_err(|error| format!("configuring the call microphone failed: {error}"))?;
    LinuxMicrophoneBackend::start(LinuxMicrophoneBackendConfig::with_route(
        packetizer,
        MicrophoneInputRoute::BuiltInSuperbird,
    ))
    .map_err(|error| format!("starting the call microphone failed: {error}"))
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn audio_failure_kind(kind: AudioBackendFailureKind) -> AudioFailureKind {
    match kind {
        AudioBackendFailureKind::Pipeline => AudioFailureKind::Pipeline,
        AudioBackendFailureKind::Bus => AudioFailureKind::Bus,
        AudioBackendFailureKind::EndpointRevalidation => AudioFailureKind::Endpoint,
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
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

#[cfg(all(target_os = "linux", feature = "linux-media"))]
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

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn microphone_timestamp_micros() -> std::result::Result<u64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_owned())?;
    u64::try_from(elapsed.as_micros())
        .map_err(|_| "system clock timestamp exceeds the Android Auto u64 bound".to_owned())
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
#[derive(Debug)]
enum WorkerTransport {
    Usb(AoapCandidate),
    Wireless {
        stream: std::net::TcpStream,
        lease: crate::WirelessLease,
    },
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
struct WorkerContext {
    config: AndroidAutoConfig,
    media_config: MediaConfig,
    ambient_light: AmbientLightState,
    display: Option<LinuxDisplayHandle>,
    progress: SharedProgress,
    audio_status: SharedAudioStatus,
    shutdown: ShutdownSignal,
    controls: Receiver<WorkerControl>,
    ended_by_phone: AtomicBool,
    ping_timeout: Duration,
    hands_free: Option<HandsFree>,
    presets: PresetBindings,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
#[allow(clippy::too_many_arguments)]
fn run_worker(
    transport: WorkerTransport,
    config: AndroidAutoConfig,
    media_config: MediaConfig,
    platform: crate::PlatformConfig,
    ambient_light: AmbientLightState,
    display: Option<LinuxDisplayHandle>,
    progress: SharedProgress,
    audio_status: SharedAudioStatus,
    shutdown: ShutdownSignal,
    setup_cancellation: prelude_android_auto::CancellationToken,
    controls: Receiver<WorkerControl>,
    hands_free: Option<HandsFree>,
) -> std::result::Result<(), String> {
    let ping_timeout = match &transport {
        WorkerTransport::Usb(_) => Duration::from_millis(config.ping_timeout_ms),
        WorkerTransport::Wireless { .. } => {
            Duration::from_millis(config.ping_timeout_ms).max(WIRELESS_PING_TIMEOUT)
        }
    };
    let context = WorkerContext {
        config,
        media_config,
        ambient_light,
        display,
        progress: Arc::clone(&progress),
        audio_status,
        shutdown,
        controls,
        ended_by_phone: AtomicBool::new(false),
        ping_timeout,
        hands_free,
        presets: platform.presets,
    };
    let result = match transport {
        WorkerTransport::Usb(candidate) => {
            run_usb_worker(&candidate, &context, &setup_cancellation)
        }
        WorkerTransport::Wireless { stream, lease } => {
            set_stage(&progress, WorkerStage::OpeningProtocol);
            let bluetooth_address = car_bluetooth_address(&context);
            let result =
                run_protocol(stream, &context, bluetooth_address).and_then(|(result, stream)| {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    result
                });
            if context.ended_by_phone.load(Ordering::Acquire) {
                lease.ended_by_phone();
            }
            drop(lease);
            result
        }
    };
    set_stage(
        &progress,
        if result.is_ok() {
            WorkerStage::Stopped
        } else {
            WorkerStage::Failed
        },
    );
    result
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn car_bluetooth_address(context: &WorkerContext) -> Option<String> {
    context
        .hands_free
        .as_ref()
        .filter(|_| !context.config.disable_audio_output)
        .and_then(|hands_free| {
            hands_free
                .wait_for_car_address(BLUETOOTH_READY_WAIT, || context.shutdown.is_requested())
        })
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn run_usb_worker(
    candidate: &AoapCandidate,
    context: &WorkerContext,
    setup_cancellation: &prelude_android_auto::CancellationToken,
) -> std::result::Result<(), String> {
    let WorkerContext {
        config,
        progress,
        shutdown,
        ..
    } = context;
    let _usb_session = context.hands_free.as_ref().map(HandsFree::usb_session);
    let bluetooth_address = car_bluetooth_address(context);
    if shutdown.is_requested() {
        return Ok(());
    }
    set_stage(progress, WorkerStage::OpeningAoap);
    let mut backend = RusbBackend;
    let identity = AccessoryIdentity {
        description: format!("{} wired Android Auto host", config.head_unit_name),
        ..AccessoryIdentity::default()
    };
    let aoap_timeouts = AoapTimeouts {
        control: Duration::from_millis(config.aoap_control_timeout_ms),
        reenumeration: Duration::from_millis(config.aoap_reenumeration_timeout_ms),
        transfer_poll_interval: Duration::from_millis(
            config
                .session_io_timeout_ms
                .min(config.usb_transfer_timeout_ms)
                .min(250),
        ),
        ..AoapTimeouts::default()
    };
    let mut transport = match open_accessory(
        &mut backend,
        candidate,
        &identity,
        aoap_timeouts,
        setup_cancellation,
    ) {
        Ok(transport) => transport,
        Err(_) if shutdown.is_requested() => return Ok(()),
        Err(error) => {
            return Err(format!(
                "opening topology-bound AOAP transport failed: {error}"
            ));
        }
    };

    set_stage(progress, WorkerStage::OpeningProtocol);
    if shutdown.is_requested() {
        transport.set_reset_on_close(false);
        transport.close().map_err(|error| {
            format!("releasing cancelled Android Auto USB transport failed: {error}")
        })?;
        return Ok(());
    }
    let io_cancellation = prelude_android_auto::CancellationToken::default();
    let io = transport.into_io(
        io_cancellation,
        Duration::from_millis(config.session_io_timeout_ms.min(250)),
        Duration::from_millis(config.usb_transfer_timeout_ms),
    );
    let (result, io) = run_protocol(io, context, bluetooth_address)?;
    let mut transport = io.into_transport();
    if result.is_ok() {
        transport.set_reset_on_close(false);
    }
    let transport_result = transport
        .close()
        .map_err(|error| format!("closing Android Auto USB transport failed: {error}"));
    result.and(transport_result)
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn run_protocol<T: std::io::Read + std::io::Write + Send>(
    io: T,
    context: &WorkerContext,
    bluetooth_address: Option<String>,
) -> std::result::Result<(std::result::Result<(), String>, T), String> {
    let WorkerContext {
        config,
        media_config,
        ambient_light,
        display,
        progress,
        audio_status,
        shutdown,
        controls,
        ended_by_phone,
        ping_timeout,
        hands_free,
        presets,
    } = context;
    let audio_output = !config.disable_audio_output;
    if let Some(address) = &bluetooth_address {
        tracing::info!(address, "offering Bluetooth hands-free calls to the phone");
    }
    let session_config = SessionConfig {
        head_unit_name: config.head_unit_name.clone(),
        video_density_dpi: config.ui_density_dpi,
        ping_interval: Duration::from_millis(config.ping_interval_ms),
        ping_timeout: *ping_timeout,
        initial_night_mode: ambient_light.night_mode(),
        audio_output,
        bluetooth_address,
        ..SessionConfig::default()
    };
    let mut session = Session::new(io, session_config)
        .map_err(|error| format!("creating Android Auto Protocol session failed: {error}"))?;
    session
        .start()
        .map_err(|error| format!("starting Android Auto Protocol session failed: {error}"))?;
    set_session_state(progress, session.state());
    set_stage(progress, WorkerStage::Negotiating);

    let mut media = None;
    let mut disconnect_reason = None;
    let mut shutdown_started = false;
    let mut pending_voice_session = None;
    let mut pending_microphone = None;
    let mut first_video = None;
    let mut pairing_phone: Option<(String, Instant, bool)> = None;
    let session_result = (|| {
        while session.is_alive() {
            if shutdown.is_requested() && !shutdown_started {
                set_stage(progress, WorkerStage::ShuttingDown);
                shutdown_started = true;
                let events = session
                    .cancel_at(Instant::now())
                    .map_err(|error| format!("requesting Android Auto ByeBye failed: {error}"))?;
                for event in events {
                    if let Event::Disconnected { reason } = event {
                        disconnect_reason = Some(reason);
                    }
                }
                if !session.is_alive() {
                    break;
                }
            }

            if media.is_none() {
                drain_pre_media_controls(controls);
            }
            let events = session
                .poll()
                .map_err(|error| format!("polling Android Auto Protocol failed: {error}"))?;
            if media.is_none() {
                set_session_state(progress, session.state());
            }
            for event in events {
                update_progress(progress, |progress| {
                    progress.protocol_events = progress.protocol_events.saturating_add(1);
                });
                match event {
                    Event::Connected => {
                        if media.is_none() {
                            set_stage(progress, WorkerStage::StartingMedia);
                            media = Some(LiveMedia::start(
                                media_config,
                                audio_output,
                                display.clone(),
                                Arc::clone(audio_status),
                                Arc::clone(progress),
                                hands_free.as_ref(),
                                presets.clone(),
                            )?);
                            if let Some(active) = pending_voice_session.take() {
                                media
                                    .as_mut()
                                    .ok_or_else(|| "media startup lost its owner".to_owned())?
                                    .handle_event(Event::VoiceSession { active }, &mut session)?;
                            }
                            if let Some((open, sample_rate, channels)) = pending_microphone.take() {
                                media
                                    .as_mut()
                                    .ok_or_else(|| "media startup lost its owner".to_owned())?
                                    .handle_event(
                                        Event::Microphone {
                                            open,
                                            sample_rate,
                                            channels,
                                        },
                                        &mut session,
                                    )?;
                            }
                            set_stage(progress, WorkerStage::Running);
                            first_video = Some(FirstVideoWatchdog::new(Instant::now()));
                        }
                    }
                    Event::Disconnected { reason } => {
                        disconnect_reason = Some(reason);
                    }
                    Event::VoiceSession { active } => {
                        if let Some(media) = media.as_mut() {
                            media.handle_event(Event::VoiceSession { active }, &mut session)?;
                        } else {
                            pending_voice_session = Some(active);
                        }
                    }
                    Event::Battery { .. } => {}
                    Event::BluetoothPairing { phone_address } => {
                        let phone = phone_address.as_deref().unwrap_or("unknown");
                        let paired = phone_address
                            .as_deref()
                            .zip(hands_free.as_ref())
                            .and_then(|(address, hands_free)| hands_free.is_paired(address))
                            .unwrap_or(false);
                        tracing::info!(
                            phone,
                            paired,
                            "the phone asked to pair Bluetooth for calls"
                        );
                        session.answer_bluetooth_pairing(paired).map_err(|error| {
                            format!("answering the Bluetooth pairing request failed: {error}")
                        })?;
                        if let Some((address, hands_free)) =
                            phone_address.as_deref().zip(hands_free.as_ref())
                        {
                            hands_free.remember_phone(address);
                        }
                        if paired
                            && let Some((address, hands_free)) =
                                phone_address.as_deref().zip(hands_free.as_ref())
                            && hands_free.connect_phone(address)
                        {
                            tracing::info!(
                                phone = address,
                                "connecting Bluetooth hands-free to the paired phone"
                            );
                        }
                        pairing_phone = phone_address.map(|address| {
                            (address, Instant::now() + BLUETOOTH_PASSKEY_WAIT, paired)
                        });
                    }
                    Event::BluetoothAuthenticated { status } => {
                        tracing::info!(?status, "the phone finished Bluetooth pairing for calls");
                    }
                    Event::Microphone {
                        open,
                        sample_rate,
                        channels,
                    } if media.is_none() => {
                        pending_microphone = Some((open, sample_rate, channels));
                    }
                    event => {
                        let media = media.as_mut().ok_or_else(|| {
                            "Android Auto media arrived before the Connected event".to_owned()
                        })?;
                        let started = Instant::now();
                        media.handle_event(event, &mut session)?;
                        media.record_busy(started);
                    }
                }
            }
            if let Some((address, deadline, paired)) = pairing_phone.take() {
                match hands_free
                    .as_ref()
                    .and_then(|hands_free| hands_free.take_passkey(&address))
                {
                    Some(passkey) => {
                        tracing::info!(
                            phone = address,
                            "sending the Bluetooth pairing passkey to the phone"
                        );
                        session.send_bluetooth_passkey(passkey).map_err(|error| {
                            format!("sending the Bluetooth pairing passkey failed: {error}")
                        })?;
                    }
                    None if Instant::now() < deadline => {
                        pairing_phone = Some((address, deadline, paired));
                    }
                    None if paired => {}
                    None => tracing::warn!(
                        phone = address,
                        "the phone never started Bluetooth pairing with Prelude"
                    ),
                }
            }
            if let Some(media) = media.as_mut()
                && session.state() == SessionState::Running
            {
                let started = Instant::now();
                media.poll(&mut session, controls)?;
                media.record_busy(started);
                if progress
                    .lock()
                    .is_ok_and(|progress| progress.video_frames > 0)
                {
                    first_video = None;
                }
                if let Some(watchdog) = first_video.as_mut() {
                    match watchdog.check(Instant::now()) {
                        FirstVideoAction::Wait => {}
                        FirstVideoAction::RequestKeyFrame => {
                            tracing::warn!(
                                "Android Auto phone has sent no video since connecting; requesting a keyframe"
                            );
                            session.send(Command::RequestKeyFrame).map_err(|error| {
                                format!("requesting first Android Auto keyframe failed: {error}")
                            })?;
                            update_progress(progress, |progress| {
                                progress.keyframe_requests =
                                    progress.keyframe_requests.saturating_add(1);
                            });
                        }
                        FirstVideoAction::Expired => {
                            tracing::warn!(
                                "Android Auto phone sent no video after connecting; restarting the session"
                            );
                            return Err(format!(
                                "Android Auto phone sent no video within {:?} of connecting",
                                FirstVideoWatchdog::TIMEOUT
                            ));
                        }
                    }
                }
            }
            let desired_night_mode = ambient_light.night_mode();
            if session
                .night_mode()
                .is_some_and(|current| current != desired_night_mode)
            {
                session
                    .set_night_mode(desired_night_mode)
                    .map_err(|error| {
                        format!(
                            "updating Android Auto night mode from ambient light failed: {error}"
                        )
                    })?;
                tracing::debug!(
                    enabled = desired_night_mode,
                    "updated Android Auto night mode from ambient light"
                );
            }
        }
        if disconnect_reason == Some(DisconnectReason::PeerShutdown) {
            ended_by_phone.store(true, Ordering::Release);
        }
        if shutdown.is_requested()
            || matches!(
                disconnect_reason,
                Some(DisconnectReason::LocalShutdown | DisconnectReason::Cancelled)
            )
        {
            Ok(())
        } else {
            let trace = session.trace();
            Err(format!(
                "Android Auto session disconnected unexpectedly ({:?}) during {:?}; last phone message {}; opened channels {:?}",
                disconnect_reason.unwrap_or(DisconnectReason::PeerClosed),
                trace.closed_in,
                trace.last_message.map_or_else(
                    || "none".to_owned(),
                    |(channel, message)| format!("{channel}/{message:#06x}")
                ),
                trace.opened_channels,
            ))
        }
    })();

    let media_result = media.map_or(Ok(()), LiveMedia::shutdown);
    Ok((session_result.and(media_result), session.into_inner()))
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn drain_pre_media_controls(controls: &Receiver<WorkerControl>) {
    while let Ok(WorkerControl::TestTone(reply)) = controls.try_recv() {
        let _ = reply.try_send(Err("Android Auto audio mixer is not running yet".to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use platform_superbird::{DeviceRole, LocalPreset, MonotonicMicros, ResetReason};

    use super::*;

    fn input(timestamp: u64, kind: PlatformInputEventKind) -> PlatformInputEvent {
        PlatformInputEvent {
            timestamp: MonotonicMicros(timestamp),
            kind,
        }
    }

    #[test]
    fn annex_b_parameter_sets_produce_avcc_and_length_prefixed_access_unit() {
        let sps = [0x67, 0x42, 0x00, 0x1e, 0xaa];
        let pps = [0x68, 0xce, 0x06, 0xe2];
        let idr = [0x65, 0x88, 0x99];
        let mut annex_b = vec![0, 0, 0, 1];
        annex_b.extend_from_slice(&sps);
        annex_b.extend_from_slice(&[0, 0, 1]);
        annex_b.extend_from_slice(&pps);
        annex_b.extend_from_slice(&[0, 0, 0, 1]);
        annex_b.extend_from_slice(&idr);

        let mut adapter = H264Adapter::default();
        let adapted = adapter
            .adapt(Some(9_876_543), &annex_b)
            .expect("valid Annex-B access unit");
        let configuration = adapted.configuration.expect("AVCC configuration");
        assert_eq!(configuration.codec, WireVideoCodec::H264);
        assert_eq!(configuration.nal_length_bytes, 4);
        assert_eq!(
            configuration.codec_data,
            vec![
                1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, 5, 0x67, 0x42, 0, 0x1e, 0xaa, 1, 0, 4, 0x68,
                0xce, 0x06, 0xe2,
            ]
        );
        let frame = adapted.frame.expect("picture frame");
        assert_eq!(frame.presentation_time_ns, Some(9_876_543));
        assert_eq!(frame.nal_units.len(), 3);
        assert_eq!(frame.nal_units[0].payload_offset, 4);
        assert_eq!(frame.nal_units[0].payload_length, sps.len());
        assert_eq!(frame.nal_units[2].unit_type, 5);
        let mut expected = Vec::new();
        for unit in [&sps[..], &pps[..], &idr[..]] {
            expected.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            expected.extend_from_slice(unit);
        }
        assert_eq!(frame.data, expected);
    }

    #[test]
    fn annex_b_parser_rejects_malformed_empty_and_oversized_units() {
        assert!(annex_b_units(b"not-annex-b").is_err());
        assert!(annex_b_units(&[0, 0, 1, 0, 0, 1, 0x65]).is_err());
        assert!(annex_b_units(&vec![0; MAX_H264_ACCESS_UNIT_BYTES + 1]).is_err());
        let oversized_sps = vec![0x67; usize::from(u16::MAX) + 1];
        assert!(h264_avcc(&oversized_sps, &[0x68, 1]).is_err());
    }

    #[test]
    fn little_endian_pcm_is_converted_for_mono_and_stereo_and_rejects_misalignment() {
        assert_eq!(
            pcm_s16le_to_be(&[0x34, 0x12, 0xcc, 0xff]).expect("mono PCM"),
            [0x12, 0x34, 0xff, 0xcc]
        );
        assert_eq!(
            pcm_s16le_to_be(&[1, 0, 2, 0, 3, 0, 4, 0]).expect("stereo PCM"),
            [0, 1, 0, 2, 0, 3, 0, 4]
        );
        assert!(pcm_s16le_to_be(&[]).is_err());
        assert!(pcm_s16le_to_be(&[1, 2, 3]).is_err());

        let mut clock = AudioClock::new();
        let first = clock
            .frame(
                AudioStreamId::Main,
                AUDIO_MEDIA_FORMAT,
                AUDIO_MEDIA_RATE,
                AUDIO_MEDIA_CHANNELS,
                &[1, 0, 2, 0, 3, 0, 4, 0],
            )
            .expect("first PCM frame");
        assert_eq!(first.timestamp, 0);
        assert_eq!(first.sequence, 0);
        let second = clock
            .frame(
                AudioStreamId::Main,
                AUDIO_MEDIA_FORMAT,
                AUDIO_MEDIA_RATE,
                AUDIO_MEDIA_CHANNELS,
                &[5, 0, 6, 0, 7, 0, 8, 0],
            )
            .expect("continued PCM frame");
        assert_eq!(second.timestamp, 2);
        assert_eq!(second.sequence, 1);
    }

    #[test]
    fn audio_ingress_window_reports_throughput_gaps_and_phone_clock_span() {
        let start = Instant::now();
        let mut window = AudioIngressWindow::default();
        window.record(start, Some(5_000_000), &[0x00, 0x80, 0x10, 0x00]);
        window.record(
            start + Duration::from_millis(30),
            Some(5_021_000),
            &[0x20, 0x00, 0x30, 0x00],
        );
        assert_eq!(window.packets, 2);
        assert_eq!(window.peak, 32_768);
        assert_eq!(window.max_gap, Duration::from_millis(30));
        assert_eq!(window.phone_span_millis(), Some(21));
        assert_eq!(window.audio_millis(1_000, 1), 4);
        window.restart();
        assert_eq!(window.packets, 0);
        assert_eq!(window.phone_span_millis(), None);
        window.record(start + Duration::from_millis(100), None, &[0, 0]);
        assert_eq!(window.max_gap, Duration::from_millis(70));
    }

    #[test]
    fn input_mapper_preserves_touch_state_and_maps_knob_siri_rotary_and_local_actions() {
        let mut mapper = InputMapper::new(PresetBindings::default());
        let down = mapper.map(input(
            10,
            PlatformInputEventKind::Touch {
                contact_id: 1,
                phase: TouchInputPhase::Down,
                x: 100,
                y: 200,
            },
        ));
        assert!(matches!(
            down,
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [Command::Touch {
                    timestamp_micros: 10,
                    action: TouchAction::Down,
                    action_index: 0,
                    pointers,
                }] if pointers == &[TouchPointer { x: 100, y: 200, id: 1 }]
            )
        ));
        let second = mapper.map(input(
            11,
            PlatformInputEventKind::Touch {
                contact_id: 2,
                phase: TouchInputPhase::Down,
                x: 300,
                y: 400,
            },
        ));
        assert!(matches!(
            second,
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [Command::Touch {
                    action: TouchAction::PointerDown,
                    action_index: 1,
                    pointers,
                    ..
                }] if pointers.len() == 2
            )
        ));
        let reset = mapper.map(input(
            12,
            PlatformInputEventKind::Reset {
                role: DeviceRole::Touch,
                reason: ResetReason::Cancelled,
            },
        ));
        let InputDisposition::Reset(reset) = reset else {
            panic!("reset must remain distinct from phone-bound input");
        };
        assert_eq!(reset.len(), 2);
        assert!(matches!(
            reset[0],
            Command::Touch {
                action: TouchAction::PointerUp,
                action_index: 1,
                ..
            }
        ));
        assert!(matches!(
            reset[1],
            Command::Touch {
                action: TouchAction::Up,
                action_index: 0,
                ..
            }
        ));

        assert!(matches!(
            mapper.map(input(
                20,
                PlatformInputEventKind::KnobButton {
                    button: KnobInputButton::Back,
                    pressed: true,
                }
            )),
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [Command::Button {
                    button: ProtocolButton::Back,
                    down: true,
                    ..
                }]
            )
        ));
        assert!(matches!(
            mapper.map(input(
                21,
                PlatformInputEventKind::SiriButton { pressed: true }
            )),
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [Command::Button {
                    button: ProtocolButton::Search,
                    down: true,
                    longpress: true,
                    ..
                }]
            )
        ));
        assert_eq!(
            mapper.map(input(
                22,
                PlatformInputEventKind::SiriButton { pressed: false }
            )),
            InputDisposition::Protocol(Vec::new())
        );
        assert!(matches!(
            mapper.map(input(23, PlatformInputEventKind::VoiceAssistantCancel)),
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [Command::Button {
                    button: ProtocolButton::Search,
                    down: false,
                    longpress: false,
                    ..
                }]
            )
        ));
        assert!(matches!(
            mapper.map(input(
                24,
                PlatformInputEventKind::Rotary {
                    direction: RotaryDirection::CounterClockwise,
                    steps: 3,
                }
            )),
            InputDisposition::Protocol(commands) if commands.len() == 3
        ));
        assert_eq!(
            mapper.map(input(
                25,
                PlatformInputEventKind::Local(LocalInputAction::Mute)
            )),
            InputDisposition::LocalMute
        );
        assert!(matches!(
            mapper.map(input(
                26,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::ONE))
            )),
            InputDisposition::Protocol(commands) if matches!(
                commands.as_slice(),
                [
                    Command::MediaKey {
                        key: MediaKey::Previous,
                        down: true,
                        ..
                    },
                    Command::MediaKey {
                        key: MediaKey::Previous,
                        down: false,
                        ..
                    }
                ]
            )
        ));
        assert_eq!(
            mapper.map(input(
                27,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::FOUR))
            )),
            InputDisposition::LocalMute
        );
    }

    #[test]
    fn remapped_presets_send_buttons_and_follow_live_updates() {
        let presets = PresetBindings::default();
        let mut mapper = InputMapper::new(presets.clone());
        presets.set([
            PresetAction::Home,
            PresetAction::VoiceAssistant,
            PresetAction::AnswerCall,
            PresetAction::None,
        ]);
        for (preset, expected) in [
            (LocalPreset::ONE, ProtocolButton::Home),
            (LocalPreset::TWO, ProtocolButton::Search),
            (LocalPreset::THREE, ProtocolButton::PhoneAccept),
        ] {
            assert!(matches!(
                mapper.map(input(1, PlatformInputEventKind::Local(LocalInputAction::Preset(preset)))),
                InputDisposition::Protocol(commands) if matches!(
                    commands.as_slice(),
                    [
                        Command::Button { button: down, down: true, longpress: false, .. },
                        Command::Button { button: up, down: false, longpress: false, .. },
                    ] if *down == expected && *up == expected
                )
            ));
        }
        assert_eq!(
            mapper.map(input(
                2,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::FOUR))
            )),
            InputDisposition::Ignore
        );
    }

    #[test]
    fn local_mute_toggles_only_after_backend_success() {
        let mut applied = Vec::new();
        let muted = apply_local_mute(false, |next| {
            applied.push(next);
            Ok(())
        })
        .expect("mute backend command");
        assert!(muted);
        let muted = apply_local_mute(muted, |next| {
            applied.push(next);
            Ok(())
        })
        .expect("unmute backend command");
        assert!(!muted);
        assert_eq!(applied, [true, false]);

        let current = false;
        let failed = apply_local_mute(current, |_| Err("backend rejected mute".to_owned()));
        assert!(failed.is_err());
        assert!(!current, "caller state remains unchanged on failure");
    }

    #[test]
    fn probe_is_strictly_staged_and_only_running_claims_capabilities() {
        let opening = probe_for_progress(WorkerProgress::default());
        assert_eq!(opening.state, HealthState::Starting);
        assert_eq!(opening.transport_state, HealthState::Starting);
        assert_eq!(opening.implementation, ImplementationStatus::AndroidAuto);
        assert_eq!(opening.capabilities, ProjectionCapabilities::default());

        let live_media_without_video_or_microphone = WorkerProgress {
            stage: WorkerStage::Running,
            session_state: Some(SessionState::Running),
            audio_ready: true,
            input_ready: true,
            video_ready: false,
            microphone_negotiated: false,
            microphone_ready: false,
            protocol_events: 5,
            video_frames: 0,
            audio_frames: 3,
            input_events: 2,
            microphone_packets: 0,
            keyframe_requests: 0,
        };
        let media_only = probe_for_progress(live_media_without_video_or_microphone);
        assert_eq!(media_only.state, HealthState::Starting);
        assert_eq!(media_only.control_state, HealthState::Ready);
        assert_eq!(media_only.authentication_state, HealthState::Ready);
        assert!(media_only.capabilities.control);
        assert!(media_only.capabilities.audio);
        assert!(media_only.capabilities.input);
        assert!(!media_only.capabilities.video);
        assert!(!media_only.capabilities.microphone);

        let video_only = probe_for_progress(WorkerProgress {
            video_ready: true,
            video_frames: 1,
            ..live_media_without_video_or_microphone
        });
        assert_eq!(video_only.state, HealthState::Starting);
        assert!(video_only.capabilities.video);
        assert!(!video_only.capabilities.microphone);

        let fully_ready = probe_for_progress(WorkerProgress {
            video_ready: true,
            microphone_negotiated: true,
            microphone_ready: true,
            video_frames: 4,
            microphone_packets: 1,
            ..live_media_without_video_or_microphone
        });
        assert_eq!(fully_ready.state, HealthState::Ready);
        assert_eq!(fully_ready.session_state, HealthState::Ready);
        assert!(fully_ready.capabilities.complete());

        let failed = probe_for_progress(WorkerProgress {
            stage: WorkerStage::Failed,
            ..WorkerProgress::default()
        });
        assert_eq!(failed.state, HealthState::Failed);
        assert_eq!(failed.session_state, HealthState::Failed);
    }

    #[test]
    fn display_status_follows_the_protocol_session_until_video_is_due() {
        let status = |stage, session_state| {
            android_auto_display_status(WorkerProgress {
                stage,
                session_state,
                ..WorkerProgress::default()
            })
        };
        assert_eq!(
            status(WorkerStage::OpeningAoap, None),
            DisplayStatus::Connecting(Some(ConnectionStep::AccessoryMode))
        );
        assert_eq!(
            status(WorkerStage::Negotiating, Some(SessionState::TlsHandshake)),
            DisplayStatus::Connecting(Some(ConnectionStep::SecuringConnection))
        );
        assert_eq!(
            status(
                WorkerStage::Negotiating,
                Some(SessionState::ServiceDiscovery)
            ),
            DisplayStatus::Connecting(Some(ConnectionStep::DiscoveringServices))
        );
        assert_eq!(
            status(WorkerStage::StartingMedia, Some(SessionState::Running)),
            DisplayStatus::Connecting(Some(ConnectionStep::StartingMedia))
        );
        assert_eq!(
            status(WorkerStage::Running, Some(SessionState::Running)),
            DisplayStatus::Connected
        );
        assert_eq!(
            status(WorkerStage::Failed, Some(SessionState::Closed)),
            DisplayStatus::Connecting(None)
        );
    }

    #[test]
    fn missing_audio_output_failures_are_classified_without_masking_other_failures() {
        assert!(is_missing_audio_output(
            "starting Android Auto audio backend failed: USB DAC is not ready (state: not connected)"
        ));
        assert!(is_missing_audio_output(
            "starting Android Auto audio backend failed: matched USB DAC has no ALSA playback PCM"
        ));
        assert!(!is_missing_audio_output(
            "starting Android Auto audio backend failed: multiple USB DAC devices were found"
        ));
        assert!(!is_missing_audio_output(
            "opening Android Auto input backend failed: input device is unavailable"
        ));
    }

    #[test]
    fn retry_schedule_is_exponential_and_capped() {
        let now = Instant::now();
        let mut retry = RetrySchedule::new();
        for expected in [1, 2, 4, 8, 15, 15] {
            retry.record_failure(now);
            assert_eq!(
                retry.retry_at.expect("retry deadline") - now,
                Duration::from_secs(expected)
            );
        }
        retry.record_success();
        assert!(retry.remaining(now).is_none());
    }

    #[test]
    fn cancellation_joins_cleanup_and_timeout_keeps_join_handle_owned() {
        struct Cleanup(Arc<AtomicUsize>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        let shutdown = ShutdownSignal::default();
        let worker_shutdown = shutdown.clone();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let worker_cleaned = Arc::clone(&cleaned);
        let handle = thread::spawn(move || {
            let _cleanup = Cleanup(worker_cleaned);
            while !worker_shutdown.is_requested() {
                thread::sleep(Duration::from_millis(1));
            }
        });
        shutdown.request();
        assert!(worker_finished_within(&handle, Duration::from_secs(1)));
        handle.join().expect("worker cleanup");
        assert_eq!(cleaned.load(Ordering::Acquire), 1);

        let slow = thread::spawn(|| thread::sleep(Duration::from_millis(25)));
        assert!(!worker_finished_within(&slow, Duration::from_millis(1)));
        assert!(
            !slow.is_finished(),
            "join handle remains owned after timeout"
        );
        slow.join().expect("slow worker cleanup");
    }
}
