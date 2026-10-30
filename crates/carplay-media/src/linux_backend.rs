use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::FileTypeExt,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    AlsaEndpoint, AudioFormatId, AudioLatencyConfig, AudioLatencySnapshot, AudioMixPolicy,
    AudioMixerPlan, AudioOutputDelay, AudioStreamConfig, AudioStreamId, CapsPlan, CapsValue,
    EncodedAudioCodec, EncryptedAudioFrame, EncryptedVideoFrame, Error, LinuxDisplayHandle,
    MICROPHONE_CAPTURE_PERIOD_MICROS, MIXER_OUTPUT_BUFFER_MICROS, MIXER_TEST_TONE_SAMPLES,
    MicrophoneCodec, MicrophoneConfig, MicrophonePacket, MicrophonePacketizer, Result,
    TARGET_VIDEO_DEVICE, TARGET_VIDEO_DRIVER_ATTRIBUTE, UNITY_GAIN_Q15, UsbDacMatcher,
    VideoBackpressureConfig, VideoBackpressureStats, VideoCodecConfiguration,
    VideoFrameDisposition, VideoIngressBuffer, VideoPipelinePlan,
    backend_plan::AudioGainRamp,
    backpressure::{DecodeStallWatchdog, StartupSampleDisposition, VideoStartupRenderGate},
    pipeline_state::{PipelineLifecycleState, require_settled_state, video_start_is_settled},
    presentation_timing::{VideoPresentationClock, VideoPresentationSchedule},
    resolve_builtin_microphone, resolve_usb_dac, revalidate_usb_dac, validate_video_decoder_target,
    wayland_presenter::WaylandPresenter,
    worker_lifecycle::receive_startup_or_join,
};
use gstreamer as gst;
use gstreamer::glib::{self, prelude::ToValue};
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::{VideoFrameExt, VideoFrameRef, VideoInfo};

#[path = "output_equalizer.rs"]
mod output_equalizer;
use output_equalizer::OutputEqualizer;

const WORKER_POLL: Duration = Duration::from_millis(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const GST_STATE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(2);
const DEFAULT_COMMAND_CAPACITY: usize = 64;
const MAXIMUM_COMMAND_CAPACITY: usize = 1024;
const AUDIO_CONTROL_CAPACITY: usize = 16;
const AUDIO_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const VIDEO_FRAME_DURATION_NS: u64 = 1_000_000_000 / 30;
const HARDWARE_STARTUP_RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const HARDWARE_STARTUP_KEYFRAME_RETRY: Duration = Duration::from_secs(1);
const HARDWARE_DECODER_PREROLL_BUFFERS: u32 = 4;
const USB_MIXER_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const AUDIO_OUTPUT_PRIME_DURATION: Duration = Duration::from_millis(250);
const TEST_TONE_DURATION: Duration = Duration::from_millis(750);

type PipelineControl = Arc<Mutex<Option<gst::Pipeline>>>;

struct PipelineRegistration(PipelineControl);

impl PipelineRegistration {
    fn new(control: &PipelineControl) -> Self {
        Self(Arc::clone(control))
    }
}

impl Drop for PipelineRegistration {
    fn drop(&mut self) {
        let _ = clear_pipeline(&self.0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendFailure {
    pub component: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackendFailureKind {
    Pipeline,
    Bus,
    EndpointRevalidation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSinkStats {
    pub rendered_buffers: u64,
    pub dropped_buffers: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioBackendTelemetrySnapshot {
    pub pipeline_start_events: u64,
    pub pipeline_stop_events: u64,
    pub frames_pushed: u64,
    pub frame_push_failure_events: u64,
    pub bus_poll_events: u64,
    pub bus_failure_events: u64,
    pub endpoint_revalidation_events: u64,
    pub endpoint_revalidation_failure_events: u64,
    pub sink_stats_poll_events: u64,
    pub sink_stats: Option<AudioSinkStats>,
    pub last_failure: Option<AudioBackendFailureKind>,
}

#[derive(Debug, Default)]
struct AudioBackendTelemetry {
    snapshot: AudioBackendTelemetrySnapshot,
    pipeline_running: bool,
}

impl AudioBackendTelemetry {
    fn pipeline_started(&mut self) {
        if !self.pipeline_running {
            saturating_increment(&mut self.snapshot.pipeline_start_events);
            self.pipeline_running = true;
        }
    }

    fn pipeline_stopped(&mut self) {
        if self.pipeline_running {
            saturating_increment(&mut self.snapshot.pipeline_stop_events);
            self.pipeline_running = false;
        }
    }

    fn frame_pushed(&mut self) {
        saturating_increment(&mut self.snapshot.frames_pushed);
    }

    fn frame_push_failed(&mut self) {
        saturating_increment(&mut self.snapshot.frame_push_failure_events);
        self.snapshot.last_failure = Some(AudioBackendFailureKind::Pipeline);
    }

    fn pipeline_failed(&mut self) {
        self.snapshot.last_failure = Some(AudioBackendFailureKind::Pipeline);
    }

    fn bus_polled(&mut self) {
        saturating_increment(&mut self.snapshot.bus_poll_events);
    }

    fn bus_failed(&mut self) {
        saturating_increment(&mut self.snapshot.bus_failure_events);
        self.snapshot.last_failure = Some(AudioBackendFailureKind::Bus);
    }

    fn endpoint_revalidated(&mut self, succeeded: bool) {
        saturating_increment(&mut self.snapshot.endpoint_revalidation_events);
        if !succeeded {
            saturating_increment(&mut self.snapshot.endpoint_revalidation_failure_events);
            self.snapshot.last_failure = Some(AudioBackendFailureKind::EndpointRevalidation);
        }
    }

    fn sampled_sink(&mut self, stats: Option<AudioSinkStats>) {
        saturating_increment(&mut self.snapshot.sink_stats_poll_events);
        let Some(stats) = stats else {
            return;
        };
        self.snapshot.sink_stats = Some(stats);
    }
}

const fn saturating_increment(value: &mut u64) {
    *value = value.saturating_add(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoBackendEvent {
    RequestKeyFrame,
    FirstFrameRendered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSynchronizationTelemetrySnapshot {
    pub output_delay: AudioOutputDelay,
    pub presented_frames: u64,
    pub presentation_deadline_miss_events: u64,
    pub total_scheduled_wait_ns: u64,
    pub maximum_presentation_lateness_ns: u64,
}

impl VideoSynchronizationTelemetrySnapshot {
    const fn new(output_delay: AudioOutputDelay) -> Self {
        Self {
            output_delay,
            presented_frames: 0,
            presentation_deadline_miss_events: 0,
            total_scheduled_wait_ns: 0,
            maximum_presentation_lateness_ns: 0,
        }
    }
}

#[derive(Clone)]
pub struct VideoSynchronizationTelemetry {
    snapshot: Arc<Mutex<VideoSynchronizationTelemetrySnapshot>>,
}

impl std::fmt::Debug for VideoSynchronizationTelemetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSynchronizationTelemetry")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl VideoSynchronizationTelemetry {
    pub fn new(output_delay: AudioOutputDelay) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(VideoSynchronizationTelemetrySnapshot::new(
                output_delay,
            ))),
        }
    }

    pub fn snapshot(&self) -> Result<VideoSynchronizationTelemetrySnapshot> {
        self.snapshot
            .lock()
            .map(|snapshot| *snapshot)
            .map_err(|_| Error::Backend("video synchronization telemetry lock is poisoned".into()))
    }

    fn record_presentation(&self, schedule: VideoPresentationSchedule, actual_lateness_ns: u64) {
        let Ok(mut snapshot) = self.snapshot.lock() else {
            return;
        };
        saturating_increment(&mut snapshot.presented_frames);
        snapshot.total_scheduled_wait_ns = snapshot
            .total_scheduled_wait_ns
            .saturating_add(schedule.wait_ns);
        let actual_lateness_ns = actual_lateness_ns.max(schedule.lateness_ns);
        snapshot.maximum_presentation_lateness_ns = snapshot
            .maximum_presentation_lateness_ns
            .max(actual_lateness_ns);
        if actual_lateness_ns > VIDEO_FRAME_DURATION_NS {
            saturating_increment(&mut snapshot.presentation_deadline_miss_events);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinuxVideoBackendConfig {
    pub backpressure: VideoBackpressureConfig,
    pub output_delay: AudioOutputDelay,
}

pub struct LinuxVideoBackend {
    ingress: VideoIngressBuffer,
    cancel: Arc<AtomicBool>,
    pipeline_control: PipelineControl,
    errors: Mutex<Receiver<BackendFailure>>,
    events: Mutex<Receiver<VideoBackendEvent>>,
    synchronization_telemetry: VideoSynchronizationTelemetry,
    worker: Option<JoinHandle<Result<VideoBackpressureStats>>>,
}

impl std::fmt::Debug for LinuxVideoBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxVideoBackend")
            .field("ingress", &self.ingress)
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl LinuxVideoBackend {
    pub fn preflight() -> Result<()> {
        Self::preflight_with_config(LinuxVideoBackendConfig::default())
    }

    pub fn preflight_with_config(_config: LinuxVideoBackendConfig) -> Result<()> {
        Self::preflight_with_display(_config, None)
    }

    pub fn preflight_with_display(
        _config: LinuxVideoBackendConfig,
        display: Option<&LinuxDisplayHandle>,
    ) -> Result<()> {
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        probe_target_video_decoder()?;
        require_factory("v4l2h264dec")?;
        for factory in ["appsrc", "h264parse", "queue", "appsink"] {
            require_factory(factory)?;
        }
        if let Some(display) = display {
            display.check()?;
        } else {
            WaylandPresenter::connect(
                usize::from(crate::SCREEN_WIDTH),
                usize::from(crate::SCREEN_HEIGHT),
            )
            .map_err(Error::Backend)?;
        }
        Ok(())
    }

    pub fn start(
        config: LinuxVideoBackendConfig,
        initial_configuration: VideoCodecConfiguration,
    ) -> Result<Self> {
        let telemetry = VideoSynchronizationTelemetry::new(config.output_delay);
        Self::start_with_telemetry(config, initial_configuration, telemetry)
    }

    pub fn start_with_telemetry(
        config: LinuxVideoBackendConfig,
        initial_configuration: VideoCodecConfiguration,
        synchronization_telemetry: VideoSynchronizationTelemetry,
    ) -> Result<Self> {
        Self::start_with_display(
            config,
            initial_configuration,
            synchronization_telemetry,
            None,
        )
    }

    pub fn start_with_display(
        config: LinuxVideoBackendConfig,
        initial_configuration: VideoCodecConfiguration,
        synchronization_telemetry: VideoSynchronizationTelemetry,
        display: Option<LinuxDisplayHandle>,
    ) -> Result<Self> {
        if synchronization_telemetry.snapshot()?.output_delay != config.output_delay {
            return Err(Error::Backend(
                "video synchronization telemetry has a different output delay".to_owned(),
            ));
        }
        probe_target_video_decoder()?;
        VideoPipelinePlan::h264(&initial_configuration)?;
        let ingress = VideoIngressBuffer::new(config.backpressure)?;
        ingress.submit_configuration(initial_configuration)?;
        let worker_ingress = ingress.clone();
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let (error_tx, error_rx) = mpsc::sync_channel(8);
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let pipeline_control = new_pipeline_control();
        let worker_pipeline_control = Arc::clone(&pipeline_control);
        let worker_synchronization_telemetry = synchronization_telemetry.clone();
        let worker = thread::Builder::new()
            .name("carplay-video-renderer".to_owned())
            .spawn(move || {
                let worker_errors = error_tx.clone();
                let result = video_worker(VideoWorkerContext {
                    ingress: worker_ingress,
                    cancel: worker_cancel,
                    pipeline_control: worker_pipeline_control,
                    startup: startup_tx,
                    errors: error_tx,
                    events: event_tx,
                    synchronization_telemetry: worker_synchronization_telemetry,
                    output_delay: config.output_delay,
                    display,
                });
                if let Err(error) = &result {
                    report_failure(&worker_errors, "video renderer", error);
                }
                result
            })
            .map_err(|error| Error::Backend(format!("cannot start video renderer: {error}")))?;
        let wake_ingress = ingress.clone();
        let worker = receive_startup_or_join(
            startup_rx,
            STARTUP_TIMEOUT,
            "video renderer",
            &cancel,
            move || {
                let _ = wake_ingress.close();
            },
            worker,
        )?;
        Ok(Self {
            ingress,
            cancel,
            pipeline_control,
            errors: Mutex::new(error_rx),
            events: Mutex::new(event_rx),
            synchronization_telemetry,
            worker: Some(worker),
        })
    }

    pub fn submit_configuration(&self, configuration: VideoCodecConfiguration) -> Result<()> {
        self.ingress.submit_configuration(configuration)
    }

    pub fn submit_frame(&self, frame: EncryptedVideoFrame) -> Result<VideoFrameDisposition> {
        self.ingress.submit_frame(frame)
    }

    pub fn try_error(&self) -> Result<Option<BackendFailure>> {
        try_receive(&self.errors, "video error channel")
    }

    pub fn try_event(&self) -> Result<Option<VideoBackendEvent>> {
        try_receive(&self.events, "video event channel")
    }

    pub fn synchronization_telemetry(&self) -> Result<VideoSynchronizationTelemetrySnapshot> {
        self.synchronization_telemetry.snapshot()
    }

    pub fn ingress_stats(&self) -> Result<VideoBackpressureStats> {
        self.ingress.stats()
    }

    pub fn shutdown(mut self) -> Result<VideoBackpressureStats> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<VideoBackpressureStats> {
        self.cancel.store(true, Ordering::Release);
        let close_result = self.ingress.close();
        let interrupt_result = interrupt_pipeline(&self.pipeline_control);
        let Some(worker) = self.worker.take() else {
            close_result?;
            interrupt_result?;
            return self.ingress.stats();
        };
        let joined = worker
            .join()
            .map_err(|_| Error::Backend("video renderer thread panicked".to_owned()))?;
        close_result?;
        interrupt_result?;
        joined
    }
}

impl Drop for LinuxVideoBackend {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

struct VideoWorkerContext {
    ingress: VideoIngressBuffer,
    cancel: Arc<AtomicBool>,
    pipeline_control: PipelineControl,
    startup: SyncSender<Result<()>>,
    errors: SyncSender<BackendFailure>,
    events: SyncSender<VideoBackendEvent>,
    synchronization_telemetry: VideoSynchronizationTelemetry,
    output_delay: AudioOutputDelay,
    display: Option<LinuxDisplayHandle>,
}

struct DisplayVideoRegistration {
    display: Option<LinuxDisplayHandle>,
    source: Option<u64>,
}

impl DisplayVideoRegistration {
    fn new(display: Option<LinuxDisplayHandle>) -> Result<Self> {
        let source = display
            .as_ref()
            .map(LinuxDisplayHandle::register_video_source)
            .transpose()?;
        Ok(Self { display, source })
    }

    const fn source(&self) -> Option<u64> {
        self.source
    }
}

impl Drop for DisplayVideoRegistration {
    fn drop(&mut self) {
        if let (Some(display), Some(source)) = (&self.display, self.source) {
            let _ = display.unregister_video_source(source);
        }
    }
}

fn video_worker(context: VideoWorkerContext) -> Result<VideoBackpressureStats> {
    let VideoWorkerContext {
        ingress,
        cancel,
        pipeline_control,
        startup,
        errors,
        events,
        synchronization_telemetry,
        output_delay,
        display,
    } = context;
    let display_registration = match DisplayVideoRegistration::new(display.clone()) {
        Ok(registration) => registration,
        Err(error) => {
            let _ = startup.send(Err(Error::Backend(error.to_string())));
            return ingress.stats();
        }
    };
    let mut presenter = if display.is_some() {
        None
    } else {
        let initialized = WaylandPresenter::connect(
            usize::from(crate::SCREEN_WIDTH),
            usize::from(crate::SCREEN_HEIGHT),
        )
        .map_err(Error::Backend);
        if let Err(error) = initialized {
            let _ = startup.send(Err(error));
            return ingress.stats();
        }
        Some(initialized?)
    };
    let Some(crate::backpressure::VideoCommand::Configuration(initial_configuration)) =
        ingress.pop_timeout(STARTUP_TIMEOUT)?
    else {
        let error = Error::Backend("video renderer received no initial configuration".to_owned());
        let _ = startup.send(Err(error));
        return ingress.stats();
    };
    let mut active_configuration = initial_configuration;
    let mut pipeline = match VideoGstPipeline::start(&active_configuration, output_delay) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            let _ = startup.send(Err(error));
            return ingress.stats();
        }
    };
    if let Err(error) = register_pipeline(&pipeline_control, &pipeline.pipeline) {
        let _ = startup.send(Err(error));
        pipeline.stop()?;
        return ingress.stats();
    }
    let _registration = PipelineRegistration::new(&pipeline_control);
    if startup.send(Ok(())).is_err() {
        pipeline.stop()?;
        return ingress.stats();
    }
    let mut stall_watchdog = DecodeStallWatchdog::default();
    let mut first_frame_rendered = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        if presenter.as_ref().is_some_and(WaylandPresenter::is_closed) {
            break;
        }
        if let Some(display) = &display {
            display.check()?;
        }
        match ingress.pop_timeout(WORKER_POLL)? {
            Some(crate::backpressure::VideoCommand::Configuration(configuration)) => {
                pipeline.stop()?;
                active_configuration = configuration;
                match VideoGstPipeline::start(&active_configuration, output_delay) {
                    Ok(started) => {
                        if let Err(error) = register_pipeline(&pipeline_control, &started.pipeline)
                        {
                            report_failure(&errors, "video", &error);
                            started.stop()?;
                            return Err(error);
                        }
                        pipeline = started;
                        stall_watchdog = DecodeStallWatchdog::default();
                    }
                    Err(error) => {
                        report_failure(&errors, "video", &error);
                        return Err(error);
                    }
                }
            }
            Some(crate::backpressure::VideoCommand::Frame(frame)) => match pipeline.push(frame) {
                Ok(()) => {
                    if pipeline.startup_gate.is_none() {
                        stall_watchdog.record_input(Instant::now());
                    }
                }
                Err(error) => {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    report_failure(&errors, "video", &error);
                    return Err(error);
                }
            },
            Some(crate::backpressure::VideoCommand::Stop) => break,
            None => {}
        }
        if let Err(error) = pipeline.check_bus() {
            report_failure(&errors, "hardware video decoder", &error);
            return Err(error);
        }
        while let Some(sample) = pipeline.sink.try_pull_sample(gst::ClockTime::ZERO) {
            let disposition = match pipeline.sample_disposition(&sample) {
                Ok(disposition) => disposition,
                Err(error) => {
                    report_failure(&errors, "video startup gate", &error);
                    return Err(error);
                }
            };
            match disposition {
                StartupSampleDisposition::Suppress => {}
                StartupSampleDisposition::SuppressAndRequestKeyFrame => {
                    if let Err(error) = send_keyframe_request(&events) {
                        report_failure(&errors, "video startup gate", &error);
                        return Err(error);
                    }
                }
                StartupSampleDisposition::Render => {
                    let schedule = pipeline.presentation_schedule(&sample)?;
                    if !pipeline.wait_for_presentation(schedule.deadline_ns, &cancel)? {
                        break;
                    }
                    if let Err(error) = render_sample(
                        &sample,
                        presenter.as_mut(),
                        display.as_ref(),
                        display_registration.source(),
                    ) {
                        report_failure(&errors, "video renderer", &error);
                        return Err(error);
                    }
                    let actual_lateness_ns = pipeline
                        .clock_time_ns()?
                        .saturating_sub(schedule.deadline_ns);
                    synchronization_telemetry.record_presentation(schedule, actual_lateness_ns);
                    stall_watchdog.record_render();
                    if !first_frame_rendered {
                        first_frame_rendered = true;
                        let _ = events.try_send(VideoBackendEvent::FirstFrameRendered);
                    }
                }
            }
        }
        if (ingress.take_keyframe_request()? || stall_watchdog.keyframe_due(Instant::now()))
            && let Err(error) = send_keyframe_request(&events)
        {
            report_failure(&errors, "video decoder recovery", &error);
            return Err(error);
        }
        if pipeline.startup_keyframe_retry_due() {
            if let Err(error) = send_keyframe_request(&events) {
                report_failure(&errors, "video startup gate", &error);
                return Err(error);
            }
        }
        if let Err(error) = pipeline.check_startup_recovery_deadline() {
            report_failure(&errors, "video startup gate", &error);
            return Err(error);
        }
    }
    pipeline.stop()?;
    ingress.stats()
}

fn send_keyframe_request(events: &SyncSender<VideoBackendEvent>) -> Result<()> {
    match events.try_send(VideoBackendEvent::RequestKeyFrame) {
        Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
        Err(error) => Err(Error::Backend(format!(
            "cannot request a clean recovery keyframe: {error}"
        ))),
    }
}

struct VideoGstPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    sink: gst_app::AppSink,
    decoder_queue: gst::Element,
    clock: gst::Clock,
    presentation_clock: VideoPresentationClock,
    next_pts_ns: u64,
    startup_gate: Option<VideoStartupRenderGate>,
    startup_recovery_deadline: Option<Instant>,
    last_keyframe_request: Option<Instant>,
}

impl VideoGstPipeline {
    fn start(
        configuration: &VideoCodecConfiguration,
        output_delay: AudioOutputDelay,
    ) -> Result<Self> {
        let plan = VideoPipelinePlan::h264(configuration)?;
        let pipeline = gst::Pipeline::new();
        let clock = gst::SystemClock::obtain();
        pipeline.use_clock(Some(&clock));
        let source_element = make("appsrc", "screen-source")?;
        let parser = make("h264parse", "h264-parser")?;
        set_checked(&parser, "config-interval", -1_i32)?;
        let parser_caps = make("capsfilter", "h264-parser-output-caps")?;
        set_checked(
            &parser_caps,
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        )?;
        let decoder_queue = make("queue", "decoder-input-queue")?;
        set_checked(&decoder_queue, "max-size-time", 2_000_000_000_u64)?;
        set_checked(&decoder_queue, "max-size-bytes", 0_u32)?;
        set_checked(&decoder_queue, "max-size-buffers", 0_u32)?;
        set_checked(
            &decoder_queue,
            "min-threshold-buffers",
            HARDWARE_DECODER_PREROLL_BUFFERS,
        )?;
        let decoder = make("v4l2h264dec", "hardware-decoder")?;
        require_string_property_equals(&decoder, "device", TARGET_VIDEO_DEVICE)?;
        set_enum_checked(&decoder, "output-io-mode", "mmap")?;
        set_enum_checked(&decoder, "capture-io-mode", "mmap")?;
        set_checked(&decoder, "discard-corrupted-frames", true)?;
        install_h264_decoder_caps_adapter(&parser)?;
        let renderer_queue = make("queue", "renderer-queue")?;
        set_enum_checked(&renderer_queue, "leaky", "downstream")?;
        set_checked(&renderer_queue, "max-size-buffers", 2_u32)?;
        set_checked(&renderer_queue, "max-size-bytes", 0_u32)?;
        set_checked(&renderer_queue, "max-size-time", 0_u64)?;
        let sink_element = make("appsink", "screen-sink")?;
        pipeline
            .add_many([
                &source_element,
                &parser,
                &parser_caps,
                &decoder_queue,
                &decoder,
                &renderer_queue,
                &sink_element,
            ])
            .map_err(gst_error("cannot add video pipeline elements"))?;
        gst::Element::link_many([
            &source_element,
            &parser,
            &parser_caps,
            &decoder_queue,
            &decoder,
            &renderer_queue,
            &sink_element,
        ])
        .map_err(gst_error("cannot link video pipeline"))?;
        let source = source_element
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| Error::Backend("appsrc has the wrong runtime type".to_owned()))?;
        source.set_caps(Some(&caps(&plan.input_caps)?));
        source.set_format(gst::Format::Time);
        source.set_is_live(true);
        source.set_do_timestamp(false);
        source.set_block(true);
        source.set_max_bytes((crate::MAXIMUM_BACKEND_FRAME_BYTES * 8) as u64);
        let sink = sink_element
            .downcast::<gst_app::AppSink>()
            .map_err(|_| Error::Backend("appsink has the wrong runtime type".to_owned()))?;
        sink.set_caps(Some(&caps(&plan.output_caps)?));
        sink.set_sync(false);
        sink.set_drop(true);
        sink.set_max_buffers(video_sink_capacity(output_delay));
        match start_video_pipeline(&pipeline) {
            Ok(_) => {}
            Err(error) => {
                let _ = stop_pipeline(&pipeline, "video pipeline");
                return Err(error);
            }
        }
        Ok(Self {
            pipeline,
            source,
            sink,
            decoder_queue,
            clock,
            presentation_clock: VideoPresentationClock::new(output_delay),
            next_pts_ns: 0,
            startup_gate: Some(VideoStartupRenderGate::default()),
            startup_recovery_deadline: None,
            last_keyframe_request: None,
        })
    }

    fn push(&mut self, frame: EncryptedVideoFrame) -> Result<()> {
        let idr = frame
            .nal_units
            .iter()
            .any(|descriptor| descriptor.unit_type == 5);
        let pts_ns = frame.presentation_time_ns.unwrap_or(self.next_pts_ns);
        if self.next_pts_ns != 0
            && pts_ns < self.next_pts_ns.saturating_sub(VIDEO_FRAME_DURATION_NS)
        {
            return Err(Error::InvalidVideo(
                "video source presentation timestamp moved backwards".to_owned(),
            ));
        }
        self.next_pts_ns = pts_ns
            .checked_add(VIDEO_FRAME_DURATION_NS)
            .ok_or_else(|| Error::Backend("video presentation time overflowed".to_owned()))?;
        if let Some(gate) = self.startup_gate.as_mut() {
            gate.register_input(idr, pts_ns);
            if self.startup_recovery_deadline.is_none() {
                let now = Instant::now();
                self.startup_recovery_deadline = Some(now + HARDWARE_STARTUP_RECOVERY_TIMEOUT);
                self.last_keyframe_request = Some(now);
            }
        }
        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        {
            let buffer = buffer
                .get_mut()
                .ok_or_else(|| Error::Backend("video buffer is unexpectedly shared".to_owned()))?;
            let pts = gst::ClockTime::from_nseconds(pts_ns);
            buffer.set_pts(pts);
            buffer.set_dts(pts);
            buffer.set_duration(gst::ClockTime::from_nseconds(VIDEO_FRAME_DURATION_NS));
        }
        self.source
            .push_buffer(buffer)
            .map_err(|error| Error::Backend(format!("video appsrc rejected a frame: {error:?}")))?;
        Ok(())
    }

    fn sample_disposition(&mut self, sample: &gst::Sample) -> Result<StartupSampleDisposition> {
        let Some(gate) = self.startup_gate.as_mut() else {
            return Ok(StartupSampleDisposition::Render);
        };
        let pts_ns = sample
            .buffer()
            .and_then(gst::BufferRef::pts)
            .map(gst::ClockTime::nseconds);
        let disposition = gate.observe_output(pts_ns)?;
        if matches!(
            disposition,
            StartupSampleDisposition::SuppressAndRequestKeyFrame
        ) {
            let now = Instant::now();
            self.startup_recovery_deadline = Some(now + HARDWARE_STARTUP_RECOVERY_TIMEOUT);
            self.last_keyframe_request = Some(now);
        }
        if gate.is_open() {
            self.startup_gate = None;
            self.startup_recovery_deadline = None;
            self.last_keyframe_request = None;
            set_checked(&self.decoder_queue, "min-threshold-buffers", 0_u32)?;
        }
        Ok(disposition)
    }

    fn startup_keyframe_retry_due(&mut self) -> bool {
        let now = Instant::now();
        let due = self.startup_recovery_deadline.is_some()
            && self
                .last_keyframe_request
                .is_some_and(|last| now.duration_since(last) >= HARDWARE_STARTUP_KEYFRAME_RETRY);
        if due {
            self.last_keyframe_request = Some(now);
        }
        due
    }

    fn check_startup_recovery_deadline(&self) -> Result<()> {
        if self
            .startup_recovery_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::Backend(
                "CarPlay did not provide a clean recovery keyframe after decoder warm-up"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn presentation_schedule(&mut self, sample: &gst::Sample) -> Result<VideoPresentationSchedule> {
        let presentation_time_ns = sample
            .buffer()
            .and_then(gst::BufferRef::pts)
            .map(gst::ClockTime::nseconds)
            .ok_or_else(|| {
                Error::Backend("decoded video sample has no presentation time".to_owned())
            })?;
        self.presentation_clock
            .schedule(presentation_time_ns, self.clock_time_ns()?)
    }

    fn clock_time_ns(&self) -> Result<u64> {
        self.clock
            .time()
            .map(gst::ClockTime::nseconds)
            .ok_or_else(|| Error::Backend("GStreamer system clock has no time".to_owned()))
    }

    fn wait_for_presentation(&self, deadline_ns: u64, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Acquire) {
                return Ok(false);
            }
            let remaining_ns = deadline_ns.saturating_sub(self.clock_time_ns()?);
            if remaining_ns == 0 {
                return Ok(true);
            }
            thread::sleep(Duration::from_nanos(remaining_ns).min(WORKER_POLL));
        }
    }

    fn check_bus(&self) -> Result<()> {
        check_bus(&self.pipeline, "video pipeline").map_err(|error| match error {
            Error::Backend(message) => Error::Backend(format!(
                "{message}; stream caps {}; decoded caps {}",
                pad_caps(&self.pipeline, "h264-parser-output-caps", "src"),
                pad_caps(&self.pipeline, "hardware-decoder", "src"),
            )),
            error => error,
        })
    }

    fn stop(self) -> Result<()> {
        stop_pipeline(&self.pipeline, "video pipeline")
    }
}

fn pad_caps(pipeline: &gst::Pipeline, element: &str, pad: &str) -> String {
    pipeline
        .by_name(element)
        .and_then(|element| element.static_pad(pad))
        .and_then(|pad| pad.current_caps())
        .map_or_else(|| "none".to_owned(), |caps| caps.to_string())
}

fn video_sink_capacity(output_delay: AudioOutputDelay) -> u32 {
    let delay_ns = output_delay.nanoseconds();
    let delayed_frames = delay_ns.div_ceil(VIDEO_FRAME_DURATION_NS);
    u32::try_from(delayed_frames)
        .unwrap_or(u32::MAX)
        .saturating_add(2)
}

fn install_h264_decoder_caps_adapter(parser: &gst::Element) -> Result<()> {
    let source_pad = parser
        .static_pad("src")
        .ok_or_else(|| Error::Backend("H.264 parser has no source pad".to_owned()))?;
    source_pad
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::EventView::Caps(caps_event) = event.view() else {
                return gst::PadProbeReturn::Ok;
            };
            let caps = caps_event.caps_owned();
            let Some(adapted_caps) = h264_caps_without_unsupported_colorimetry(&caps) else {
                return gst::PadProbeReturn::Ok;
            };
            info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(
                &adapted_caps,
            )));
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| Error::Backend("cannot install H.264 decoder caps adapter".to_owned()))?;
    Ok(())
}

fn h264_caps_without_unsupported_colorimetry(caps: &gst::CapsRef) -> Option<gst::Caps> {
    if caps.size() != 1 {
        return None;
    }
    let structure = caps.structure(0)?;
    if structure.name() != "video/x-h264"
        || structure.get::<u32>("bit-depth-luma").ok()? != 8
        || structure.get::<u32>("bit-depth-chroma").ok()? != 8
        || structure.get::<&str>("chroma-format").ok()? != "4:2:0"
        || structure.get::<&str>("colorimetry").ok()? == "bt709"
    {
        return None;
    }
    let mut adapted = caps.to_owned();
    adapted
        .make_mut()
        .structure_mut(0)?
        .remove_field("colorimetry");
    Some(adapted)
}

impl Drop for VideoGstPipeline {
    fn drop(&mut self) {
        let _ = stop_pipeline(&self.pipeline, "video pipeline");
    }
}

fn render_sample(
    sample: &gst::Sample,
    presenter: Option<&mut WaylandPresenter>,
    display: Option<&LinuxDisplayHandle>,
    display_source: Option<u64>,
) -> Result<()> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| Error::Backend("decoded video sample has no buffer".to_owned()))?;
    let caps = sample
        .caps()
        .ok_or_else(|| Error::Backend("decoded video sample has no caps".to_owned()))?;
    let info = VideoInfo::from_caps(caps)
        .map_err(|error| Error::Backend(format!("decoded video caps are invalid: {error}")))?;
    if info.format() != gstreamer_video::VideoFormat::Nv12
        || info.width() != u32::from(crate::SCREEN_WIDTH)
        || info.height() != u32::from(crate::SCREEN_HEIGHT)
        || info.n_planes() != 2
    {
        return Err(Error::Backend(
            "decoded video sample does not match the negotiated NV12 geometry".to_owned(),
        ));
    }
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|error| Error::Backend(format!("cannot map decoded video frame: {error}")))?;
    let y_plane = frame
        .plane_data(0)
        .map_err(|error| Error::Backend(format!("cannot map decoded NV12 Y plane: {error}")))?;
    let uv_plane = frame
        .plane_data(1)
        .map_err(|error| Error::Backend(format!("cannot map decoded NV12 UV plane: {error}")))?;
    let y_pitch = usize::try_from(frame.plane_stride()[0])
        .map_err(|_| Error::Backend("decoded NV12 Y plane has a negative stride".to_owned()))?;
    let uv_pitch = usize::try_from(frame.plane_stride()[1])
        .map_err(|_| Error::Backend("decoded NV12 UV plane has a negative stride".to_owned()))?;
    match (presenter, display, display_source) {
        (Some(presenter), None, None) => presenter
            .present_nv12(y_plane, uv_plane, y_pitch, uv_pitch)
            .map_err(Error::Backend),
        (None, Some(display), Some(display_source)) => {
            let width = usize::from(crate::SCREEN_WIDTH);
            let height = usize::from(crate::SCREEN_HEIGHT);
            let mut packed = vec![0_u8; width * height * 3 / 2];
            for row in 0..height {
                let source = row * y_pitch;
                let target = row * width;
                packed[target..target + width].copy_from_slice(&y_plane[source..source + width]);
            }
            let uv_offset = width * height;
            for row in 0..height / 2 {
                let source = row * uv_pitch;
                let target = uv_offset + row * width;
                packed[target..target + width].copy_from_slice(&uv_plane[source..source + width]);
            }
            display.submit_video_for_source(display_source, packed)
        }
        _ => Err(Error::Backend(
            "video renderer has an invalid display ownership state".to_owned(),
        )),
    }
}

#[derive(Debug, Clone)]
pub struct LinuxAudioBackendConfig {
    pub equalizer: crate::EqualizerControl,
    pub route: AudioOutputRoute,
    pub streams: Vec<AudioStreamConfig>,
    pub command_capacity: usize,
    pub mix_policy: AudioMixPolicy,
    pub latency: AudioLatencyConfig,
    pub echo_reference: Option<crate::EchoReference>,
}

impl LinuxAudioBackendConfig {
    pub fn new(usb_dac: UsbDacMatcher, streams: Vec<(AudioStreamId, AudioFormatId)>) -> Self {
        Self::with_route(AudioOutputRoute::UsbAlsa(usb_dac), streams)
    }

    pub fn with_route(
        route: AudioOutputRoute,
        streams: Vec<(AudioStreamId, AudioFormatId)>,
    ) -> Self {
        Self::with_negotiated_route(
            route,
            streams
                .into_iter()
                .map(|(stream_id, format_id)| AudioStreamConfig::new(stream_id, format_id))
                .collect(),
        )
    }

    pub fn with_negotiated_route(route: AudioOutputRoute, streams: Vec<AudioStreamConfig>) -> Self {
        Self {
            equalizer: Default::default(),
            route,
            streams,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            mix_policy: AudioMixPolicy::default(),
            latency: AudioLatencyConfig::default(),
            echo_reference: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOutputRoute {
    UsbAlsa(UsbDacMatcher),
    #[cfg(test)]
    TestAlsa(String),
}

#[derive(Debug, Clone)]
#[cfg_attr(test, allow(clippy::large_enum_variant))]
enum ResolvedAudioOutput {
    Usb {
        matcher: UsbDacMatcher,
        endpoint: AlsaEndpoint,
        device: String,
    },
    #[cfg(test)]
    Test { device: String },
}

impl ResolvedAudioOutput {
    fn resolve(route: AudioOutputRoute) -> Result<Self> {
        match route {
            AudioOutputRoute::UsbAlsa(matcher) => {
                let endpoint = resolve_usb_dac(Path::new("/sys"), &matcher)?;
                prepare_usb_playback_controls(endpoint.card_id())?;
                let device = endpoint.alsa_device();
                Ok(Self::Usb {
                    matcher,
                    endpoint,
                    device,
                })
            }
            #[cfg(test)]
            AudioOutputRoute::TestAlsa(device) => Ok(Self::Test { device }),
        }
    }

    fn device(&self) -> &str {
        match self {
            Self::Usb { device, .. } => device,
            #[cfg(test)]
            Self::Test { device } => device,
        }
    }

    fn revalidate(&self) -> Result<()> {
        match self {
            Self::Usb {
                matcher, endpoint, ..
            } => revalidate_usb_dac(Path::new("/sys"), matcher, endpoint),
            #[cfg(test)]
            Self::Test { .. } => Ok(()),
        }
    }
}

fn prepare_usb_playback_controls(card_id: &str) -> Result<()> {
    for control in ["Master", "PCM", "Speaker", "Headphone", "Headset"] {
        let _ = set_usb_playback_control(card_id, control, "unmute")?;
        let _ = set_usb_playback_control(card_id, control, "100%")?;
    }
    Ok(())
}

fn set_usb_playback_control(card_id: &str, control: &str, value: &str) -> Result<bool> {
    let mut child = Command::new("/usr/bin/amixer")
        .args(["--quiet", "--card", card_id, "sset", control, value])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            Error::Backend(format!("cannot start USB mixer initialization: {error}"))
        })?;
    let deadline = Instant::now() + USB_MIXER_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.try_wait();
                return Err(Error::Backend(
                    "USB mixer initialization exceeded its deadline".to_owned(),
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.try_wait();
                return Err(Error::Backend(format!(
                    "cannot observe USB mixer initialization: {error}"
                )));
            }
        }
    }
}

#[derive(Debug)]
enum AudioControlCommand {
    SetDucking {
        active: bool,
        main_gain_q15: Option<u16>,
        duration: Duration,
    },
    ConfigureStream {
        stream: AudioStreamConfig,
        generation: u64,
        reply: SyncSender<std::result::Result<(), String>>,
    },
    DeactivateStream {
        stream_id: AudioStreamId,
        generation: u64,
        reply: SyncSender<std::result::Result<(), String>>,
    },
    SetMuted(bool),
    TestTone,
    Flush(SyncSender<std::result::Result<(), String>>),
}

#[derive(Debug)]
struct QueuedAudioFrame {
    generation: Option<u64>,
    frame: EncryptedAudioFrame,
}

#[derive(Debug, Clone)]
pub struct AudioFrameSender {
    stream_id: AudioStreamId,
    sender: SyncSender<QueuedAudioFrame>,
}

impl AudioFrameSender {
    pub fn try_submit(&self, frame: EncryptedAudioFrame) -> Result<()> {
        if frame.stream_id != self.stream_id {
            return Err(Error::InvalidAudio(
                "audio frame belongs to a different stream".to_owned(),
            ));
        }
        queue_audio_frame(
            &self.sender,
            QueuedAudioFrame {
                generation: None,
                frame,
            },
        )
    }
}

fn queue_audio_frame(
    sender: &SyncSender<QueuedAudioFrame>,
    queued: QueuedAudioFrame,
) -> Result<()> {
    match sender.try_send(queued) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(Error::Backend(
            "audio mixer ingress is full; no audio stream was dropped".to_owned(),
        )),
        Err(TrySendError::Disconnected(_)) => Err(Error::NotReady {
            component: "audio mixer",
            state: "stopped",
        }),
    }
}

pub struct LinuxAudioBackend {
    frames: BTreeMap<AudioStreamId, SyncSender<QueuedAudioFrame>>,
    controls: SyncSender<AudioControlCommand>,
    cancel: Arc<AtomicBool>,
    flushing: Arc<AtomicBool>,
    errors: Mutex<Receiver<BackendFailure>>,
    latency: Arc<Mutex<AudioLatencySnapshot>>,
    telemetry: Arc<Mutex<AudioBackendTelemetry>>,
    pipeline_control: PipelineControl,
    worker: Option<JoinHandle<Result<()>>>,
}

impl std::fmt::Debug for LinuxAudioBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxAudioBackend")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl LinuxAudioBackend {
    pub fn preflight(config: &LinuxAudioBackendConfig) -> Result<()> {
        validate_command_capacity(config.command_capacity)?;
        let plan = AudioMixerPlan::with_stream_options(
            &config.streams,
            config.mix_policy,
            config.latency,
        )?;
        let output = ResolvedAudioOutput::resolve(config.route.clone())?;
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        for factory in [
            "audiomixer",
            "audiotestsrc",
            "audioconvert",
            "audioresample",
            "capsfilter",
            "alsasink",
        ] {
            require_factory(factory)?;
        }
        for stream in &plan.streams {
            for element in &stream.elements {
                require_factory(element.factory)?;
            }
        }
        probe_resolved_audio_output(&output)?;
        Ok(())
    }

    pub fn preflight_formats(
        usb_dac: &UsbDacMatcher,
        formats: &[(AudioStreamId, AudioFormatId)],
    ) -> Result<AlsaEndpoint> {
        if formats.is_empty() {
            return Err(Error::InvalidAudio(
                "audio preflight format set is empty".to_owned(),
            ));
        }
        let endpoint = resolve_usb_dac(Path::new("/sys"), usb_dac)?;
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        for factory in [
            "audiomixer",
            "audiotestsrc",
            "audioconvert",
            "audioresample",
            "capsfilter",
            "alsasink",
        ] {
            require_factory(factory)?;
        }
        for &(stream_id, format_id) in formats {
            let plan = crate::AudioStreamPlan::new(stream_id, format_id)?;
            for element in &plan.elements {
                require_factory(element.factory)?;
            }
        }
        probe_alsa_output(&endpoint.alsa_device())?;
        Ok(endpoint)
    }

    pub fn preflight_route_formats(
        route: &AudioOutputRoute,
        formats: &[(AudioStreamId, AudioFormatId)],
    ) -> Result<()> {
        if formats.is_empty() {
            return Err(Error::InvalidAudio(
                "audio preflight format set is empty".to_owned(),
            ));
        }
        let output = ResolvedAudioOutput::resolve(route.clone())?;
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        for factory in [
            "audiomixer",
            "audiotestsrc",
            "audioconvert",
            "audioresample",
            "capsfilter",
            "alsasink",
        ] {
            require_factory(factory)?;
        }
        for &(stream_id, format_id) in formats {
            let plan = crate::AudioStreamPlan::new(stream_id, format_id)?;
            for element in &plan.elements {
                require_factory(element.factory)?;
            }
        }
        probe_resolved_audio_output(&output)?;
        Ok(())
    }

    pub fn start(config: LinuxAudioBackendConfig) -> Result<Self> {
        validate_command_capacity(config.command_capacity)?;
        let plan = AudioMixerPlan::with_stream_options(
            &config.streams,
            config.mix_policy,
            config.latency,
        )?;
        let output = ResolvedAudioOutput::resolve(config.route)?;
        let mut frame_senders = BTreeMap::new();
        let mut frame_receivers = BTreeMap::new();
        for stream_id in [
            AudioStreamId::Main,
            AudioStreamId::Alternate,
            AudioStreamId::High,
        ] {
            let (sender, receiver) = mpsc::sync_channel(config.command_capacity);
            frame_senders.insert(stream_id, sender);
            frame_receivers.insert(stream_id, receiver);
        }
        let (control_tx, control_rx) = mpsc::sync_channel(AUDIO_CONTROL_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let (error_tx, error_rx) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let flushing = Arc::new(AtomicBool::new(false));
        let latency = Arc::new(Mutex::new(AudioLatencySnapshot::unmeasured(config.latency)));
        let worker_latency = Arc::clone(&latency);
        let telemetry = Arc::new(Mutex::new(AudioBackendTelemetry::default()));
        let worker_telemetry = Arc::clone(&telemetry);
        let pipeline_control = new_pipeline_control();
        let worker_pipeline_control = Arc::clone(&pipeline_control);
        let worker_cancel = Arc::clone(&cancel);
        let worker_flushing = Arc::clone(&flushing);
        let worker = thread::Builder::new()
            .name("carplay-audio-mixer".to_owned())
            .spawn(move || {
                audio_worker(AudioWorkerContext {
                    plan,
                    output,
                    equalizer: config.equalizer,
                    echo_reference: config.echo_reference,
                    frames: frame_receivers,
                    controls: control_rx,
                    cancel: worker_cancel,
                    flushing: worker_flushing,
                    startup: startup_tx,
                    errors: error_tx,
                    latency: worker_latency,
                    telemetry: worker_telemetry,
                    pipeline_control: worker_pipeline_control,
                })
            })
            .map_err(|error| Error::Backend(format!("cannot start audio mixer: {error}")))?;
        let worker = receive_startup_or_join(
            startup_rx,
            STARTUP_TIMEOUT,
            "audio mixer",
            &cancel,
            || {},
            worker,
        )?;
        Ok(Self {
            frames: frame_senders,
            controls: control_tx,
            cancel,
            flushing,
            errors: Mutex::new(error_rx),
            latency,
            telemetry,
            pipeline_control,
            worker: Some(worker),
        })
    }

    pub fn try_submit(&self, frame: EncryptedAudioFrame) -> Result<()> {
        self.try_submit_inner(None, frame)
    }

    pub fn try_submit_generation(&self, generation: u64, frame: EncryptedAudioFrame) -> Result<()> {
        if generation == 0 {
            return Err(Error::InvalidAudio(
                "audio stream generation must be nonzero".to_owned(),
            ));
        }
        self.try_submit_inner(Some(generation), frame)
    }

    fn try_submit_inner(&self, generation: Option<u64>, frame: EncryptedAudioFrame) -> Result<()> {
        let sender = self.frames.get(&frame.stream_id).ok_or_else(|| {
            Error::InvalidAudio("audio frame has no bounded ingress queue".to_owned())
        })?;
        queue_audio_frame(sender, QueuedAudioFrame { generation, frame })
    }

    pub fn frame_sender(&self, stream_id: AudioStreamId) -> Option<AudioFrameSender> {
        self.frames.get(&stream_id).map(|sender| AudioFrameSender {
            stream_id,
            sender: sender.clone(),
        })
    }

    pub fn try_error(&self) -> Result<Option<BackendFailure>> {
        let reported = try_receive(&self.errors, "audio error channel")?;
        if reported.is_some() {
            return Ok(reported);
        }
        Ok(self.cancel.load(Ordering::Acquire).then(|| BackendFailure {
            component: "audio",
            message: "audio mixer was cancelled before session shutdown".to_owned(),
        }))
    }

    pub fn set_ducking(&self, active: bool) -> Result<()> {
        self.set_ducking_gain(active, None)
    }

    pub fn set_ducking_gain(&self, active: bool, main_linear_gain: Option<f64>) -> Result<()> {
        self.set_ducking_gain_ramped(active, main_linear_gain, Duration::ZERO)
    }

    pub fn set_ducking_gain_ramped(
        &self,
        active: bool,
        main_linear_gain: Option<f64>,
        duration: Duration,
    ) -> Result<()> {
        let main_gain_q15 = main_linear_gain
            .map(|gain| {
                if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
                    return Err(Error::InvalidAudio(
                        "ducking gain is not a finite linear value within 0..=1".to_owned(),
                    ));
                }
                Ok((gain * f64::from(UNITY_GAIN_Q15)).round() as u16)
            })
            .transpose()?;
        match self.controls.try_send(AudioControlCommand::SetDucking {
            active,
            main_gain_q15,
            duration,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::Backend(
                "audio control queue is full; ducking state was not changed".to_owned(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(Error::NotReady {
                component: "audio mixer",
                state: "stopped",
            }),
        }
    }

    pub fn configure_stream(&self, stream: AudioStreamConfig, generation: u64) -> Result<()> {
        if generation == 0 {
            return Err(Error::InvalidAudio(
                "audio stream generation must be nonzero".to_owned(),
            ));
        }
        self.call_control(|reply| AudioControlCommand::ConfigureStream {
            stream,
            generation,
            reply,
        })
    }

    pub fn deactivate_stream(&self, stream_id: AudioStreamId, generation: u64) -> Result<()> {
        if generation == 0 {
            return Err(Error::InvalidAudio(
                "audio stream generation must be nonzero".to_owned(),
            ));
        }
        self.call_control(|reply| AudioControlCommand::DeactivateStream {
            stream_id,
            generation,
            reply,
        })
    }

    fn call_control(
        &self,
        command: impl FnOnce(SyncSender<std::result::Result<(), String>>) -> AudioControlCommand,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        match self.controls.try_send(command(reply_tx)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(Error::Backend(
                    "audio control queue is full; stream state was not changed".to_owned(),
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(Error::NotReady {
                    component: "audio mixer",
                    state: "stopped",
                });
            }
        }
        match reply_rx.recv_timeout(AUDIO_CONTROL_TIMEOUT) {
            Ok(result) => result.map_err(Error::Backend),
            Err(error) => {
                self.cancel.store(true, Ordering::Release);
                let interrupted = interrupt_pipeline(&self.pipeline_control);
                let detail = match error {
                    mpsc::RecvTimeoutError::Timeout => "timed out",
                    mpsc::RecvTimeoutError::Disconnected => "lost its reply channel",
                };
                match interrupted {
                    Ok(()) => Err(Error::Backend(format!(
                        "audio stream control {detail}; the mixer was stopped to prevent a late state change"
                    ))),
                    Err(interrupt) => Err(Error::Backend(format!(
                        "audio stream control {detail}; stopping the mixer also failed: {interrupt}"
                    ))),
                }
            }
        }
    }

    pub fn set_muted(&self, active: bool) -> Result<()> {
        match self
            .controls
            .try_send(AudioControlCommand::SetMuted(active))
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::Backend(
                "audio control queue is full; mute state was not changed".to_owned(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(Error::NotReady {
                component: "audio mixer",
                state: "stopped",
            }),
        }
    }

    pub fn play_test_tone(&self) -> Result<()> {
        match self.controls.try_send(AudioControlCommand::TestTone) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::Backend(
                "audio control queue is full; test tone was not started".to_owned(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(Error::NotReady {
                component: "audio mixer",
                state: "stopped",
            }),
        }
    }

    pub fn flush(&self) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.flushing.store(true, Ordering::Release);
        if let Err(error) = begin_audio_pipeline_flush(&self.pipeline_control) {
            let _ = finish_audio_pipeline_flush(&self.pipeline_control);
            self.flushing.store(false, Ordering::Release);
            return Err(error);
        }
        let deadline = Instant::now() + AUDIO_CONTROL_TIMEOUT;
        let result = (|| {
            let mut command = AudioControlCommand::Flush(reply_tx);
            loop {
                match self.controls.try_send(command) {
                    Ok(()) => break,
                    Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                        command = returned;
                        thread::sleep(WORKER_POLL);
                    }
                    Err(TrySendError::Full(_)) => {
                        return Err(Error::Backend(
                            "audio control queue stayed full during flush".to_owned(),
                        ));
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        return Err(Error::NotReady {
                            component: "audio mixer",
                            state: "stopped",
                        });
                    }
                }
            }
            reply_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| {
                    Error::Backend("audio flush did not complete within its bound".to_owned())
                })?
                .map_err(Error::Backend)
        })();
        if let Err(error) = result {
            let cleanup = finish_audio_pipeline_flush(&self.pipeline_control);
            self.flushing.store(false, Ordering::Release);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Backend(format!(
                    "{error}; cancelling the incomplete audio flush failed: {cleanup}"
                ))),
            };
        }
        Ok(())
    }

    pub fn latency(&self) -> Result<AudioLatencySnapshot> {
        self.latency
            .lock()
            .map(|snapshot| *snapshot)
            .map_err(|_| Error::Backend("audio latency lock is poisoned".to_owned()))
    }

    pub fn telemetry(&self) -> Result<AudioBackendTelemetrySnapshot> {
        self.telemetry
            .lock()
            .map(|telemetry| telemetry.snapshot)
            .map_err(|_| Error::Backend("audio telemetry lock is poisoned".to_owned()))
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        let interrupt_result = interrupt_pipeline(&self.pipeline_control);
        let Some(worker) = self.worker.take() else {
            return interrupt_result;
        };
        let joined = worker
            .join()
            .map_err(|_| Error::Backend("audio mixer thread panicked".to_owned()))?;
        interrupt_result?;
        joined
    }
}

impl Drop for LinuxAudioBackend {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

struct AudioGstPipeline {
    equalizer: Option<OutputEqualizer>,
    pipeline: gst::Pipeline,
    sink: gst::Element,
    test_tone: gst::Element,
    sources: BTreeMap<AudioStreamId, gst_app::AppSrc>,
    plan: AudioMixerPlan,
    mixer_pads: BTreeMap<AudioStreamId, gst::Pad>,
    clock_origins: BTreeMap<AudioStreamId, PcmClockOrigin>,
    inactive_streams: BTreeSet<AudioStreamId>,
}

const fn appsrc_timestamps_arrival(codec: EncodedAudioCodec) -> bool {
    !matches!(codec, EncodedAudioCodec::PcmS16Be { .. })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PcmClockOrigin {
    first_timestamp: u32,
    first_pts_ns: u64,
}

impl PcmClockOrigin {
    fn new(first_timestamp: u32, running_time_ns: u64, playout_latency_ms: u32) -> Result<Self> {
        let playout_latency_ns = u64::from(playout_latency_ms)
            .checked_mul(1_000_000)
            .ok_or_else(|| Error::InvalidAudio("PCM playout latency overflows".to_owned()))?;
        let first_pts_ns = running_time_ns
            .checked_add(playout_latency_ns)
            .ok_or_else(|| Error::InvalidAudio("PCM presentation time overflows".to_owned()))?;
        Ok(Self {
            first_timestamp,
            first_pts_ns,
        })
    }

    fn schedule(
        current: Option<Self>,
        timestamp: u32,
        running_time_ns: u64,
        playout_latency_ms: u32,
        payload_bytes: usize,
        sample_rate_hz: u32,
        channels: u8,
    ) -> Result<(Self, u64, u64)> {
        if let Some(origin) = current {
            let (pts_ns, duration_ns) =
                origin.timing(timestamp, payload_bytes, sample_rate_hz, channels)?;
            if pts_ns >= running_time_ns {
                return Ok((origin, pts_ns, duration_ns));
            }
        }
        let origin = Self::new(timestamp, running_time_ns, playout_latency_ms)?;
        let (pts_ns, duration_ns) =
            origin.timing(timestamp, payload_bytes, sample_rate_hz, channels)?;
        Ok((origin, pts_ns, duration_ns))
    }

    fn timing(
        self,
        timestamp: u32,
        payload_bytes: usize,
        sample_rate_hz: u32,
        channels: u8,
    ) -> Result<(u64, u64)> {
        let bytes_per_frame = usize::from(channels)
            .checked_mul(2)
            .ok_or_else(|| Error::InvalidAudio("PCM frame width overflows".to_owned()))?;
        if bytes_per_frame == 0 || payload_bytes == 0 || payload_bytes % bytes_per_frame != 0 {
            return Err(Error::InvalidAudio(
                "PCM payload is not aligned to complete channel frames".to_owned(),
            ));
        }
        let sample_offset = u64::from(timestamp.wrapping_sub(self.first_timestamp));
        let offset_ns = sample_offset
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_div(u64::from(sample_rate_hz)))
            .ok_or_else(|| Error::InvalidAudio("PCM presentation offset overflows".to_owned()))?;
        let duration_samples = u64::try_from(payload_bytes / bytes_per_frame)
            .map_err(|_| Error::InvalidAudio("PCM frame sample count overflows".to_owned()))?;
        let duration_ns = duration_samples
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_div(u64::from(sample_rate_hz)))
            .ok_or_else(|| Error::InvalidAudio("PCM frame duration overflows".to_owned()))?;
        let pts_ns = self
            .first_pts_ns
            .checked_add(offset_ns)
            .ok_or_else(|| Error::InvalidAudio("PCM presentation time overflows".to_owned()))?;
        Ok((pts_ns, duration_ns))
    }
}

struct AudioWorkerContext {
    equalizer: crate::EqualizerControl,
    echo_reference: Option<crate::EchoReference>,
    plan: AudioMixerPlan,
    output: ResolvedAudioOutput,
    frames: BTreeMap<AudioStreamId, Receiver<QueuedAudioFrame>>,
    controls: Receiver<AudioControlCommand>,
    cancel: Arc<AtomicBool>,
    flushing: Arc<AtomicBool>,
    startup: SyncSender<Result<()>>,
    errors: SyncSender<BackendFailure>,
    latency: Arc<Mutex<AudioLatencySnapshot>>,
    telemetry: Arc<Mutex<AudioBackendTelemetry>>,
    pipeline_control: PipelineControl,
}

fn update_audio_telemetry(
    telemetry: &Mutex<AudioBackendTelemetry>,
    update: impl FnOnce(&mut AudioBackendTelemetry),
) {
    if let Ok(mut telemetry) = telemetry.lock() {
        update(&mut telemetry);
    }
}

fn revalidate_audio_output(
    output: &ResolvedAudioOutput,
    telemetry: &Mutex<AudioBackendTelemetry>,
) -> Result<()> {
    let result = output.revalidate();
    update_audio_telemetry(telemetry, |telemetry| {
        telemetry.endpoint_revalidated(result.is_ok());
    });
    result
}

fn stop_audio_pipeline(
    active: &mut AudioGstPipeline,
    telemetry: &Mutex<AudioBackendTelemetry>,
) -> Result<()> {
    let result = active.stop();
    update_audio_telemetry(telemetry, |telemetry| {
        if result.is_ok() {
            telemetry.pipeline_stopped();
        } else {
            telemetry.pipeline_failed();
        }
    });
    result
}

const AUDIO_STREAM_POLL_ORDER: [AudioStreamId; 3] = [
    AudioStreamId::Alternate,
    AudioStreamId::Main,
    AudioStreamId::High,
];

fn try_next_audio_frame(
    frames: &BTreeMap<AudioStreamId, Receiver<QueuedAudioFrame>>,
    next_index: &mut usize,
) -> Option<QueuedAudioFrame> {
    for offset in 0..AUDIO_STREAM_POLL_ORDER.len() {
        let index = (*next_index + offset) % AUDIO_STREAM_POLL_ORDER.len();
        let stream_id = AUDIO_STREAM_POLL_ORDER[index];
        let Some(receiver) = frames.get(&stream_id) else {
            continue;
        };
        match receiver.try_recv() {
            Ok(frame) => {
                *next_index = (index + 1) % AUDIO_STREAM_POLL_ORDER.len();
                return Some(frame);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
    }
    None
}

fn drain_audio_frames(frames: &BTreeMap<AudioStreamId, Receiver<QueuedAudioFrame>>) {
    for receiver in frames.values() {
        while receiver.try_recv().is_ok() {}
    }
}

fn audio_frame_generation_is_current(
    stream_generations: &BTreeMap<AudioStreamId, u64>,
    queued: &QueuedAudioFrame,
) -> bool {
    queued.generation.is_none_or(|generation| {
        stream_generations.get(&queued.frame.stream_id) == Some(&generation)
    })
}

fn audio_worker(context: AudioWorkerContext) -> Result<()> {
    let AudioWorkerContext {
        plan,
        output,
        equalizer,
        echo_reference,
        frames,
        controls,
        cancel,
        flushing,
        startup,
        errors,
        latency,
        telemetry,
        pipeline_control,
    } = context;
    let initialized = (|| {
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        revalidate_audio_output(&output, &telemetry)?;
        prime_alsa_output(output.device(), plan.latency)?;
        revalidate_audio_output(&output, &telemetry)?;
        AudioGstPipeline::start_with_equalizer(
            plan,
            output.device(),
            equalizer.clone(),
            echo_reference.clone(),
        )
    })();
    let (mut active, _registration) = match initialized {
        Ok(mut active) => {
            update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_started);
            if let Err(error) = register_pipeline(&pipeline_control, &active.pipeline) {
                update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                let _ = startup.send(Err(error));
                stop_audio_pipeline(&mut active, &telemetry)?;
                return Ok(());
            }
            if startup.send(Ok(())).is_err() {
                stop_audio_pipeline(&mut active, &telemetry)?;
                return Ok(());
            }
            (active, PipelineRegistration::new(&pipeline_control))
        }
        Err(error) => {
            equalizer.report_availability(crate::EqualizerAvailability::Unavailable);
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };
    let mut next_health_check = Instant::now();
    let mut main_gain_q15 = active.main_gain_target(false, None);
    let mut main_gain_ramp: Option<(AudioGainRamp, Instant)> = None;
    let mut stream_generations = BTreeMap::new();
    let mut next_stream_index = 0;
    let mut ducked = false;
    let mut muted = false;
    let mut test_tone_until = None;
    while !cancel.load(Ordering::Acquire) {
        if let Some(dsp) = active.equalizer.as_mut()
            && let Err(error) = dsp.tick()
        {
            update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
            report_failure(&errors, "audio", &error);
            stop_audio_pipeline(&mut active, &telemetry)?;
            return Err(error);
        }
        if let Some((ramp, started)) = main_gain_ramp {
            let elapsed = started.elapsed();
            let next_gain = ramp.gain_at(elapsed);
            if next_gain != main_gain_q15 {
                main_gain_q15 = next_gain;
                if !muted {
                    if let Err(error) = active.set_mix_state(main_gain_q15, false, ducked) {
                        update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                        report_failure(&errors, "audio", &error);
                        stop_audio_pipeline(&mut active, &telemetry)?;
                        return Err(error);
                    }
                }
            }
            if ramp.is_complete(elapsed) {
                main_gain_q15 = ramp.target();
                main_gain_ramp = None;
            }
        }
        match controls.try_recv() {
            Ok(AudioControlCommand::SetDucking {
                active: requested,
                main_gain_q15: requested_main_gain_q15,
                duration,
            }) => {
                ducked = requested;
                let target = active.main_gain_target(requested, requested_main_gain_q15);
                let ramp = AudioGainRamp::new(main_gain_q15, target, duration)?;
                if duration.is_zero() {
                    main_gain_q15 = target;
                    main_gain_ramp = None;
                } else {
                    main_gain_ramp = Some((ramp, Instant::now()));
                }
                if let Err(error) = active.set_mix_state(main_gain_q15, muted, ducked) {
                    update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                    report_failure(&errors, "audio", &error);
                    stop_audio_pipeline(&mut active, &telemetry)?;
                    return Err(error);
                }
                continue;
            }
            Ok(AudioControlCommand::ConfigureStream {
                stream,
                generation,
                reply,
            }) => {
                let result = active
                    .configure_stream(stream)
                    .and_then(|()| active.set_mix_state(main_gain_q15, muted, ducked));
                if result.is_ok() {
                    stream_generations.insert(stream.stream_id, generation);
                }
                let _ = reply.send(result.map_err(|error| error.to_string()));
                continue;
            }
            Ok(AudioControlCommand::DeactivateStream {
                stream_id,
                generation,
                reply,
            }) => {
                let result = if stream_generations.get(&stream_id) == Some(&generation) {
                    active.deactivate_stream(stream_id).map(|()| {
                        stream_generations.remove(&stream_id);
                    })
                } else {
                    Ok(())
                };
                let _ = reply.send(result.map_err(|error| error.to_string()));
                continue;
            }
            Ok(AudioControlCommand::SetMuted(requested)) => {
                muted = requested;
                if let Err(error) = active.set_mix_state(main_gain_q15, muted, ducked) {
                    update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                    report_failure(&errors, "audio", &error);
                    stop_audio_pipeline(&mut active, &telemetry)?;
                    return Err(error);
                }
                continue;
            }
            Ok(AudioControlCommand::TestTone) => {
                if let Err(error) = active.set_test_tone(true) {
                    update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                    report_failure(&errors, "audio", &error);
                    stop_audio_pipeline(&mut active, &telemetry)?;
                    return Err(error);
                }
                test_tone_until = Some(Instant::now() + TEST_TONE_DURATION);
                continue;
            }
            Ok(AudioControlCommand::Flush(reply)) => {
                drain_audio_frames(&frames);
                match active.finish_flush() {
                    Ok(()) => {
                        drain_audio_frames(&frames);
                        flushing.store(false, Ordering::Release);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        flushing.store(false, Ordering::Release);
                        let _ = reply.send(Err(error.to_string()));
                        update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                        report_failure(&errors, "audio", &error);
                        stop_audio_pipeline(&mut active, &telemetry)?;
                        return Err(error);
                    }
                }
                continue;
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        if let Some(queued) = try_next_audio_frame(&frames, &mut next_stream_index) {
            if audio_frame_generation_is_current(&stream_generations, &queued) {
                if let Err(error) = active.push(queued.frame) {
                    if cancel.load(Ordering::Acquire) {
                        stop_audio_pipeline(&mut active, &telemetry)?;
                        return Ok(());
                    }
                    if flushing.load(Ordering::Acquire) {
                        continue;
                    }
                    update_audio_telemetry(&telemetry, AudioBackendTelemetry::frame_push_failed);
                    report_failure(&errors, "audio", &error);
                    stop_audio_pipeline(&mut active, &telemetry)?;
                    return Err(error);
                }
                update_audio_telemetry(&telemetry, AudioBackendTelemetry::frame_pushed);
            }
        } else {
            thread::sleep(WORKER_POLL);
        }
        if test_tone_until.is_some_and(|deadline| Instant::now() >= deadline) {
            if let Err(error) = active.set_test_tone(false) {
                update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                report_failure(&errors, "audio", &error);
                stop_audio_pipeline(&mut active, &telemetry)?;
                return Err(error);
            }
            test_tone_until = None;
        }
        update_audio_telemetry(&telemetry, AudioBackendTelemetry::bus_polled);
        if let Err(error) = active.check_bus() {
            update_audio_telemetry(&telemetry, AudioBackendTelemetry::bus_failed);
            report_failure(&errors, "audio", &error);
            stop_audio_pipeline(&mut active, &telemetry)?;
            return Err(error);
        }
        if Instant::now() >= next_health_check {
            if let Err(error) = revalidate_audio_output(&output, &telemetry) {
                report_failure(&errors, "audio", &error);
                stop_audio_pipeline(&mut active, &telemetry)?;
                return Err(error);
            }
            if let Err(error) = active.measure_latency(&latency) {
                update_audio_telemetry(&telemetry, AudioBackendTelemetry::pipeline_failed);
                report_failure(&errors, "audio", &error);
                stop_audio_pipeline(&mut active, &telemetry)?;
                return Err(error);
            }
            let sink_stats = active.sink_stats();
            update_audio_telemetry(&telemetry, |telemetry| {
                telemetry.sampled_sink(sink_stats);
            });
            next_health_check = Instant::now() + Duration::from_secs(1);
        }
    }
    stop_audio_pipeline(&mut active, &telemetry)
}

fn schedule_jitter_latency_on_branch(pad: &gst::Pad, jitter_latency_ms: u32) -> Result<()> {
    let jitter = gst::ClockTime::from_mseconds(u64::from(jitter_latency_ms));
    let offset = i64::try_from(jitter.nseconds())
        .map_err(|_| Error::Backend("RTP jitter latency overflows a pad offset".to_owned()))?;
    pad.set_offset(offset);
    pad.add_probe(
        gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL,
        move |_, info| {
            if let Some(gst::PadProbeData::Query(query)) = &mut info.data
                && let gst::QueryViewMut::Latency(latency) = query.view_mut()
            {
                let (live, minimum, maximum) = latency.result();
                if live {
                    latency.set(live, minimum.saturating_sub(jitter), maximum);
                }
            }
            gst::PadProbeReturn::Ok
        },
    )
    .ok_or_else(|| Error::Backend("cannot schedule RTP jitter latency on its branch".to_owned()))?;
    Ok(())
}

fn tap_echo_reference(
    sink: &gst::Element,
    output_caps: &crate::CapsPlan,
    reference: crate::EchoReference,
) -> Result<()> {
    let signed_field = |name| match output_caps.field(name) {
        Some(CapsValue::Signed(value)) => u32::try_from(*value).ok(),
        _ => None,
    };
    let rate = signed_field("rate")
        .ok_or_else(|| Error::Backend("mixer output caps have no rate".to_owned()))?;
    let channels = signed_field("channels")
        .and_then(|channels| u16::try_from(channels).ok())
        .ok_or_else(|| Error::Backend("mixer output caps have no channel count".to_owned()))?;
    if output_caps.field("format") != Some(&CapsValue::String("S16LE".to_owned())) {
        return Err(Error::Backend(
            "echo reference needs S16LE mixer output".to_owned(),
        ));
    }
    let pad = sink
        .static_pad("sink")
        .ok_or_else(|| Error::Backend("audio sink has no input pad".to_owned()))?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer()
            && let Ok(mapped) = buffer.map_readable()
        {
            reference.render_s16le(mapped.as_slice(), rate, channels);
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or_else(|| {
        Error::Backend("cannot tap the mixer output for echo cancellation".to_owned())
    })?;
    Ok(())
}

fn floating_mix_caps(plan: &AudioMixerPlan) -> Result<gst::Caps> {
    let mut mix = plan.output_caps.clone();
    for (name, value) in &mut mix.fields {
        if *name == "format" {
            *value = CapsValue::String("F32LE".to_owned());
        }
    }
    caps(&mix)
}

impl AudioGstPipeline {
    #[cfg(test)]
    fn start(plan: AudioMixerPlan, alsa_device: &str) -> Result<Self> {
        Self::start_with_equalizer(plan, alsa_device, Default::default(), None)
    }

    fn start_with_equalizer(
        plan: AudioMixerPlan,
        alsa_device: &str,
        equalizer_control: crate::EqualizerControl,
        echo_reference: Option<crate::EchoReference>,
    ) -> Result<Self> {
        let pipeline = gst::Pipeline::new();
        let clock = gst::SystemClock::obtain();
        pipeline.use_clock(Some(&clock));
        let mixer = make("audiomixer", "carplay-mixer")?;
        set_checked(&mixer, "ignore-inactive-pads", true)?;
        set_checked(
            &mixer,
            "output-buffer-duration",
            u64::from(MIXER_OUTPUT_BUFFER_MICROS) * 1_000,
        )?;
        let output_convert = make("audioconvert", "mixer-output-converter")?;
        let output_caps = make("capsfilter", "mixer-output-caps")?;
        set_checked(&output_caps, "caps", caps(&plan.output_caps)?)?;
        let sink = make("alsasink", "carplay-audio-sink")?;
        set_checked(&sink, "device", alsa_device)?;
        set_checked(&sink, "sync", plan.sink_clock_synchronized)?;
        set_checked(
            &sink,
            "buffer-time",
            i64::from(plan.latency.sink_buffer_micros),
        )?;
        set_checked(
            &sink,
            "latency-time",
            i64::from(plan.latency.sink_period_micros),
        )?;
        let mix_caps = make("capsfilter", "float-mixer-output")?;
        set_checked(&mix_caps, "caps", floating_mix_caps(&plan)?)?;
        let equalizer = OutputEqualizer::create(equalizer_control)?;
        let mut output_elements = vec![&mixer, &mix_caps];
        if let Some(dsp) = equalizer.as_ref() {
            output_elements.extend(dsp.elements());
        }
        output_elements.extend([&output_convert, &output_caps, &sink]);
        pipeline
            .add_many(&output_elements)
            .map_err(gst_error("cannot add audio output elements"))?;
        gst::Element::link_many(&output_elements).map_err(gst_error("cannot link audio output"))?;
        if let Some(reference) = echo_reference {
            tap_echo_reference(&sink, &plan.output_caps, reference)?;
        }
        let test_tone = make("audiotestsrc", "audio-output-test-tone")?;
        set_checked(&test_tone, "is-live", true)?;
        set_checked(
            &test_tone,
            "samplesperbuffer",
            i32::try_from(MIXER_TEST_TONE_SAMPLES)
                .map_err(|_| Error::Backend("test-tone buffer size overflows".to_owned()))?,
        )?;
        set_checked(&test_tone, "freq", 440.0_f64)?;
        set_checked(&test_tone, "volume", 0.08_f64)?;
        set_enum_checked(&test_tone, "wave", "silence")?;
        let test_convert = make("audioconvert", "test-tone-converter")?;
        let test_resample = make("audioresample", "test-tone-resampler")?;
        let test_caps = make("capsfilter", "test-tone-output-caps")?;
        set_checked(&test_caps, "caps", floating_mix_caps(&plan)?)?;
        let test_queue = make("queue", "test-tone-queue")?;
        pipeline
            .add_many([
                &test_tone,
                &test_convert,
                &test_resample,
                &test_caps,
                &test_queue,
            ])
            .map_err(gst_error("cannot add audio test-tone branch"))?;
        gst::Element::link_many([
            &test_tone,
            &test_convert,
            &test_resample,
            &test_caps,
            &test_queue,
        ])
        .map_err(gst_error("cannot link audio test-tone branch"))?;
        let test_source_pad = test_queue
            .static_pad("src")
            .ok_or_else(|| Error::Backend("audio test-tone branch has no source pad".to_owned()))?;
        let test_mixer_pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
            Error::Backend("audiomixer did not allocate a test-tone input pad".to_owned())
        })?;
        test_source_pad.link(&test_mixer_pad).map_err(|error| {
            Error::Backend(format!("cannot link test-tone mixer pad: {error:?}"))
        })?;
        let mut sources = BTreeMap::new();
        let mut mixer_pads = BTreeMap::new();
        for stream in &plan.streams {
            let suffix = (stream.stream_id as u8).to_string();
            let source_element = make("appsrc", &format!("audio-source-{suffix}"))?;
            let source = source_element
                .clone()
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| Error::Backend("audio appsrc has wrong runtime type".to_owned()))?;
            source.set_caps(Some(&caps(&stream.input_caps)?));
            source.set_format(gst::Format::Time);
            source.set_is_live(true);
            source.set_do_timestamp(appsrc_timestamps_arrival(stream.codec));
            source.set_block(true);
            source.set_max_bytes((crate::MAXIMUM_RTP_PACKET_BYTES * 32) as u64);
            let input_queue = make("queue", &format!("audio-input-queue-{suffix}"))?;
            set_checked(&input_queue, "max-size-time", 2_000_000_000_u64)?;
            set_checked(&input_queue, "max-size-bytes", 0_u32)?;
            set_checked(&input_queue, "max-size-buffers", 0_u32)?;
            let convert = make("audioconvert", &format!("audio-converter-{suffix}"))?;
            let resample = make("audioresample", &format!("audio-resampler-{suffix}"))?;
            let branch_caps = make("capsfilter", &format!("mixer-input-caps-{suffix}"))?;
            set_checked(&branch_caps, "caps", floating_mix_caps(&plan)?)?;
            let mixer_queue = make("queue", &format!("mixer-input-queue-{suffix}"))?;
            set_checked(&mixer_queue, "max-size-time", 200_000_000_u64)?;
            set_checked(&mixer_queue, "max-size-bytes", 0_u32)?;
            set_checked(&mixer_queue, "max-size-buffers", 0_u32)?;
            let mut branch = vec![source_element, input_queue];
            for codec_element in &stream.elements[2..stream.elements.len() - 3] {
                let element = make(
                    codec_element.factory,
                    &format!("{}-{suffix}", codec_element.name),
                )?;
                if codec_element.factory == "rtpjitterbuffer" {
                    set_checked(
                        &element,
                        "latency",
                        stream.jitter_latency_ms.ok_or_else(|| {
                            Error::Backend("RTP jitter latency is absent".to_owned())
                        })?,
                    )?;
                }
                branch.push(element);
            }
            branch.extend([convert, resample, branch_caps, mixer_queue]);
            let branch_refs: Vec<&gst::Element> = branch.iter().collect();
            pipeline
                .add_many(&branch_refs)
                .map_err(gst_error("cannot add audio branch"))?;
            gst::Element::link_many(&branch_refs).map_err(gst_error("cannot link audio branch"))?;
            let source_pad = branch
                .last()
                .and_then(|element| element.static_pad("src"))
                .ok_or_else(|| Error::Backend("audio branch has no source pad".to_owned()))?;
            if let Some(jitter_latency_ms) = stream.jitter_latency_ms {
                schedule_jitter_latency_on_branch(&source_pad, jitter_latency_ms)?;
            }
            let mixer_pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
                Error::Backend("audiomixer did not allocate an input pad".to_owned())
            })?;
            source_pad
                .link(&mixer_pad)
                .map_err(|error| Error::Backend(format!("cannot link mixer pad: {error:?}")))?;
            set_checked(
                &mixer_pad,
                "volume",
                gain_as_f64(plan.mix_policy.gain(stream.stream_id, false)),
            )?;
            sources.insert(stream.stream_id, source);
            mixer_pads.insert(stream.stream_id, mixer_pad);
        }
        if let Err(error) = set_playing(&pipeline, "audio mixer") {
            let _ = stop_pipeline(&pipeline, "audio mixer");
            return Err(error);
        }
        Ok(Self {
            equalizer,
            pipeline,
            sink,
            test_tone,
            sources,
            plan,
            mixer_pads,
            clock_origins: BTreeMap::new(),
            inactive_streams: BTreeSet::new(),
        })
    }

    fn finish_flush(&mut self) -> Result<()> {
        let mut stopped = self.test_tone.send_event(gst::event::FlushStop::new(false));
        for source in self.sources.values() {
            stopped &= source.send_event(gst::event::FlushStop::new(false));
        }
        self.clock_origins.clear();
        if !stopped {
            return Err(Error::Backend(
                "audio pipeline rejected flush stop".to_owned(),
            ));
        }
        Ok(())
    }

    fn push(&mut self, frame: EncryptedAudioFrame) -> Result<()> {
        let stream = self.plan.stream(frame.stream_id).ok_or_else(|| {
            Error::InvalidAudio("audio frame has no configured mixer branch".to_owned())
        })?;
        let packet = stream.adapt_frame(&frame)?;
        let mut buffer = gst::Buffer::from_mut_slice(packet);
        if let crate::EncodedAudioCodec::PcmS16Be {
            sample_rate_hz,
            channels,
        } = stream.codec
        {
            let running_time_ns = self
                .pipeline
                .current_running_time()
                .ok_or_else(|| Error::Backend("audio mixer has no running-time clock".to_owned()))?
                .nseconds();
            let (origin, pts_ns, duration_ns) = PcmClockOrigin::schedule(
                self.clock_origins.get(&frame.stream_id).copied(),
                frame.timestamp,
                running_time_ns,
                stream.playout_latency_ms,
                frame.payload.len(),
                sample_rate_hz,
                channels,
            )?;
            self.clock_origins.insert(frame.stream_id, origin);
            let writable = buffer.get_mut().ok_or_else(|| {
                Error::Backend("new audio buffer is unexpectedly shared".to_owned())
            })?;
            writable.set_pts(gst::ClockTime::from_nseconds(pts_ns));
            writable.set_duration(gst::ClockTime::from_nseconds(duration_ns));
        }
        self.sources
            .get(&frame.stream_id)
            .ok_or_else(|| Error::Backend("audio appsrc is missing".to_owned()))?
            .push_buffer(buffer)
            .map_err(|error| Error::Backend(format!("audio appsrc rejected a frame: {error:?}")))?;
        Ok(())
    }

    fn check_bus(&self) -> Result<()> {
        check_bus(&self.pipeline, "audio mixer")
    }

    fn sink_stats(&self) -> Option<AudioSinkStats> {
        base_sink_stats(&self.sink)
    }

    fn configure_stream(&mut self, stream: AudioStreamConfig) -> Result<()> {
        let replacement = crate::AudioStreamPlan::with_latency(stream, self.plan.latency)?;
        let index = self
            .plan
            .streams
            .iter()
            .position(|configured| configured.stream_id == stream.stream_id)
            .ok_or_else(|| {
                Error::InvalidAudio(
                    "audio stream was not provisioned in the persistent mixer".to_owned(),
                )
            })?;
        let configured = &self.plan.streams[index];
        if configured
            .elements
            .iter()
            .map(|element| element.factory)
            .ne(replacement.elements.iter().map(|element| element.factory))
        {
            return Err(Error::InvalidAudio(
                "audio stream codec topology cannot change in a live mixer".to_owned(),
            ));
        }
        let source = self
            .sources
            .get(&stream.stream_id)
            .ok_or_else(|| Error::Backend("provisioned audio stream has no appsrc".to_owned()))?;
        let replacement_caps = caps(&replacement.input_caps)?;
        if self.inactive_streams.contains(&stream.stream_id) {
            let peer = source
                .static_pad("src")
                .and_then(|pad| pad.peer())
                .ok_or_else(|| {
                    Error::Backend("retired audio appsrc has no downstream pad".to_owned())
                })?;
            if !peer.send_event(gst::event::FlushStart::new()) {
                return Err(Error::Backend(
                    "audio branch rejected flush start".to_owned(),
                ));
            }
            source.set_state(gst::State::Ready).map_err(|error| {
                Error::Backend(format!("cannot reset retired audio appsrc: {error:?}"))
            })?;
            source.set_caps(Some(&replacement_caps));
            if !peer.send_event(gst::event::FlushStop::new(false)) {
                return Err(Error::Backend(
                    "audio branch rejected flush stop".to_owned(),
                ));
            }
            source.sync_state_with_parent().map_err(|error| {
                Error::Backend(format!("cannot reactivate retired audio appsrc: {error}"))
            })?;
            self.inactive_streams.remove(&stream.stream_id);
        } else {
            source.set_caps(Some(&replacement_caps));
        }
        self.plan.streams[index] = replacement;
        self.clock_origins.remove(&stream.stream_id);
        Ok(())
    }

    fn deactivate_stream(&mut self, stream_id: AudioStreamId) -> Result<()> {
        let source = self
            .sources
            .get(&stream_id)
            .ok_or_else(|| Error::Backend("deactivated audio stream has no appsrc".to_owned()))?;
        source.end_of_stream().map_err(|error| {
            Error::Backend(format!("retiring audio stream appsrc failed: {error:?}"))
        })?;
        self.clock_origins.remove(&stream_id);
        self.inactive_streams.insert(stream_id);
        Ok(())
    }

    fn main_gain_target(&self, ducked: bool, requested: Option<u16>) -> u16 {
        if ducked {
            requested.unwrap_or_else(|| self.plan.mix_policy.gain(AudioStreamId::Main, true))
        } else {
            self.plan.mix_policy.gain(AudioStreamId::Main, false)
        }
    }

    fn set_mix_state(&self, main_gain_q15: u16, muted: bool, ducked: bool) -> Result<()> {
        if main_gain_q15 > UNITY_GAIN_Q15 {
            return Err(Error::InvalidAudio(
                "MainAudio mixer gain exceeds unity".to_owned(),
            ));
        }
        for (stream, pad) in &self.mixer_pads {
            let gain = if muted {
                0
            } else if *stream == AudioStreamId::Main {
                main_gain_q15
            } else {
                self.plan.mix_policy.gain(*stream, ducked)
            };
            set_checked(pad, "volume", gain_as_f64(gain))?;
        }
        Ok(())
    }

    fn set_test_tone(&self, active: bool) -> Result<()> {
        set_enum_checked(
            &self.test_tone,
            "wave",
            if active { "sine" } else { "silence" },
        )
    }

    fn measure_latency(&self, snapshot: &Mutex<AudioLatencySnapshot>) -> Result<()> {
        let mut query = gst::query::Latency::new();
        if !self.pipeline.query(&mut query) {
            return Ok(());
        }
        let (live, minimum, maximum) = query.result();
        let mut snapshot = snapshot
            .lock()
            .map_err(|_| Error::Backend("audio latency lock is poisoned".to_owned()))?;
        snapshot.gstreamer_query_live = Some(live);
        snapshot.gstreamer_query_minimum_ns = Some(minimum.nseconds());
        snapshot.gstreamer_query_maximum_ns = maximum.map(gst::ClockTime::nseconds);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        stop_pipeline(&self.pipeline, "audio mixer")
    }
}

fn base_sink_stats(sink: &gst::Element) -> Option<AudioSinkStats> {
    let property = sink.find_property("stats")?;
    if !property.flags().contains(glib::ParamFlags::READABLE) {
        return None;
    }
    let stats = sink.property_value("stats").get::<gst::Structure>().ok()?;
    Some(AudioSinkStats {
        rendered_buffers: stats.get::<u64>("rendered").ok()?,
        dropped_buffers: stats.get::<u64>("dropped").ok()?,
    })
}

impl Drop for AudioGstPipeline {
    fn drop(&mut self) {
        let _ = stop_pipeline(&self.pipeline, "audio mixer");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneCaptureLevel {
    pub rms_q15: u16,
    pub peak_q15: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrophoneBackendEvent {
    Packet {
        packet: MicrophonePacket,
        level: Option<MicrophoneCaptureLevel>,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrophoneInputRoute {
    BuiltInSuperbird,
}

#[derive(Debug, Clone)]
enum ResolvedMicrophoneInput {
    BuiltIn {
        endpoint: AlsaEndpoint,
        device: String,
    },
}

impl ResolvedMicrophoneInput {
    fn resolve(route: MicrophoneInputRoute) -> Result<Self> {
        match route {
            MicrophoneInputRoute::BuiltInSuperbird => {
                let endpoint = resolve_builtin_microphone(Path::new("/sys"))?;
                let device = endpoint.alsa_device();
                Ok(Self::BuiltIn { endpoint, device })
            }
        }
    }

    fn device(&self) -> &str {
        match self {
            Self::BuiltIn { device, .. } => device,
        }
    }

    fn revalidate(&self) -> Result<()> {
        match self {
            Self::BuiltIn { endpoint, .. } => {
                let current = resolve_builtin_microphone(Path::new("/sys"))?;
                if current != *endpoint {
                    return Err(Error::Backend(
                        "built-in microphone ALSA endpoint changed during capture".to_owned(),
                    ));
                }
                Ok(())
            }
        }
    }
}

pub struct LinuxMicrophoneBackendConfig {
    pub packetizer: MicrophonePacketizer,
    pub event_capacity: usize,
    pub route: MicrophoneInputRoute,
    pub echo_reference: Option<crate::EchoReference>,
}

impl std::fmt::Debug for LinuxMicrophoneBackendConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxMicrophoneBackendConfig")
            .field("packetizer", &"<redacted-key-state>")
            .field("event_capacity", &self.event_capacity)
            .field("route", &self.route)
            .field("echo_cancellation", &self.echo_reference.is_some())
            .finish()
    }
}

impl LinuxMicrophoneBackendConfig {
    pub fn new(packetizer: MicrophonePacketizer) -> Self {
        Self {
            packetizer,
            event_capacity: 32,
            route: MicrophoneInputRoute::BuiltInSuperbird,
            echo_reference: None,
        }
    }

    pub fn with_route(packetizer: MicrophonePacketizer, route: MicrophoneInputRoute) -> Self {
        Self {
            packetizer,
            event_capacity: 32,
            route,
            echo_reference: None,
        }
    }
}

pub struct LinuxMicrophoneBackend {
    cancel: Arc<AtomicBool>,
    events: Mutex<Receiver<MicrophoneBackendEvent>>,
    errors: Mutex<Receiver<BackendFailure>>,
    worker: Option<JoinHandle<Result<()>>>,
    echo_canceller: Option<SharedEchoCanceller>,
}

type SharedEchoCanceller = Arc<Mutex<crate::EchoCanceller>>;

impl std::fmt::Debug for LinuxMicrophoneBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxMicrophoneBackend")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl LinuxMicrophoneBackend {
    pub fn preflight(config: MicrophoneConfig) -> Result<AlsaEndpoint> {
        let endpoint = resolve_builtin_microphone(Path::new("/sys"))?;
        let plan = crate::MicrophonePipelinePlan::new(config)?;
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        for element in &plan.elements {
            require_factory(element.factory)?;
        }
        Ok(endpoint)
    }

    pub fn preflight_route(config: MicrophoneConfig, route: MicrophoneInputRoute) -> Result<()> {
        let input = ResolvedMicrophoneInput::resolve(route)?;
        let plan = crate::MicrophonePipelinePlan::new(config)?;
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        for element in &plan.elements {
            require_factory(element.factory)?;
        }
        probe_resolved_microphone_input(&input)
    }

    pub fn start(config: LinuxMicrophoneBackendConfig) -> Result<Self> {
        validate_command_capacity(config.event_capacity)?;
        let input = ResolvedMicrophoneInput::resolve(config.route)?;
        probe_resolved_microphone_input(&input)?;
        let (event_tx, event_rx) = mpsc::sync_channel(config.event_capacity);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let (error_tx, error_rx) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let echo_canceller = config.echo_reference.as_ref().map(|reference| {
            Arc::new(Mutex::new(crate::EchoCanceller::attach(
                reference,
                config.packetizer.config().sample_rate_hz,
            )))
        });
        let worker_echo_canceller = echo_canceller.clone();
        let worker = thread::Builder::new()
            .name("carplay-microphone".to_owned())
            .spawn(move || {
                microphone_worker(
                    config.packetizer,
                    worker_echo_canceller,
                    input,
                    event_tx,
                    worker_cancel,
                    startup_tx,
                    error_tx,
                )
            })
            .map_err(|error| Error::Backend(format!("cannot start microphone: {error}")))?;
        let worker = receive_startup_or_join(
            startup_rx,
            STARTUP_TIMEOUT,
            "microphone",
            &cancel,
            || {},
            worker,
        )?;
        Ok(Self {
            cancel,
            events: Mutex::new(event_rx),
            errors: Mutex::new(error_rx),
            worker: Some(worker),
            echo_canceller,
        })
    }

    pub fn echo_canceller_summary(&self) -> Option<crate::EchoCancellerSummary> {
        let canceller = self.echo_canceller.as_ref()?;
        Some(
            canceller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .summary(),
        )
    }

    pub fn try_event(&self) -> Result<Option<MicrophoneBackendEvent>> {
        try_receive(&self.events, "microphone event channel")
    }

    pub fn try_error(&self) -> Result<Option<BackendFailure>> {
        try_receive(&self.errors, "microphone error channel")
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| Error::Backend("microphone thread panicked".to_owned()))?
    }
}

impl Drop for LinuxMicrophoneBackend {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn microphone_worker(
    mut packetizer: MicrophonePacketizer,
    echo_canceller: Option<SharedEchoCanceller>,
    input: ResolvedMicrophoneInput,
    events: SyncSender<MicrophoneBackendEvent>,
    cancel: Arc<AtomicBool>,
    startup: SyncSender<Result<()>>,
    errors: SyncSender<BackendFailure>,
) -> Result<()> {
    let initialized = (|| {
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        MicrophoneGstPipeline::start(&packetizer, &input, echo_canceller)
    })();
    let active = match initialized {
        Ok(active) => {
            if startup.send(Ok(())).is_err() {
                active.stop()?;
                return Ok(());
            }
            active
        }
        Err(error) => {
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };
    let mut next_identity_check = Instant::now();
    while !cancel.load(Ordering::Acquire) {
        if Instant::now() >= next_identity_check {
            match input.revalidate() {
                Ok(()) => {}
                Err(error) => {
                    report_failure(&errors, "microphone", &error);
                    active.stop()?;
                    packetizer.teardown();
                    return Err(error);
                }
            }
            next_identity_check = Instant::now() + Duration::from_secs(1);
        }
        if let Err(error) = active.check_bus() {
            report_failure(&errors, "microphone", &error);
            active.stop()?;
            packetizer.teardown();
            return Err(error);
        }
        let Some(sample) = active
            .sink
            .try_pull_sample(gst::ClockTime::from_mseconds(10))
        else {
            continue;
        };
        let result = process_microphone_sample(&sample, &mut packetizer);
        let (packets, level) = match result {
            Ok(packets) => packets,
            Err(error) => {
                report_failure(&errors, "microphone", &error);
                active.stop()?;
                packetizer.teardown();
                return Err(error);
            }
        };
        for packet in packets {
            match events.try_send(MicrophoneBackendEvent::Packet { packet, level }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let error = Error::Backend(
                        "microphone uplink queue is full; capture stopped fail-closed".to_owned(),
                    );
                    report_failure(&errors, "microphone", &error);
                    active.stop()?;
                    packetizer.teardown();
                    return Err(error);
                }
                Err(TrySendError::Disconnected(_)) => {
                    active.stop()?;
                    packetizer.teardown();
                    return Ok(());
                }
            }
        }
    }
    active.stop()?;
    packetizer.teardown();
    let _ = events.try_send(MicrophoneBackendEvent::Stopped);
    Ok(())
}

struct MicrophoneGstPipeline {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
}

impl MicrophoneGstPipeline {
    fn start(
        packetizer: &MicrophonePacketizer,
        input: &ResolvedMicrophoneInput,
        echo_canceller: Option<SharedEchoCanceller>,
    ) -> Result<Self> {
        let plan = crate::MicrophonePipelinePlan::new(packetizer.config())?;
        let codec = packetizer.config().codec;
        let pipeline = gst::Pipeline::new();
        let source = make("alsasrc", "built-in-microphone")?;
        set_checked(&source, "device", input.device())?;
        set_checked(
            &source,
            "latency-time",
            i64::from(MICROPHONE_CAPTURE_PERIOD_MICROS),
        )?;
        let source_caps = make("capsfilter", "microphone-source-caps")?;
        set_checked(&source_caps, "caps", caps(&plan.source_caps)?)?;
        let queue = make("queue", "microphone-capture-queue")?;
        set_checked(&queue, "max-size-time", 200_000_000_u64)?;
        set_checked(&queue, "max-size-bytes", 0_u32)?;
        set_checked(&queue, "max-size-buffers", 0_u32)?;
        let convert = make("audioconvert", "microphone-downmixer")?;
        let resample = make("audioresample", "microphone-resampler")?;
        let output_caps = make("capsfilter", "microphone-packetizer-caps")?;
        set_checked(&output_caps, "caps", caps(&plan.packetizer_input_caps)?)?;
        if let Some(canceller) = echo_canceller {
            cancel_echo_on_capture(&output_caps, canceller)?;
        }
        let mut elements = vec![source, source_caps, queue, convert, resample, output_caps];
        if codec == MicrophoneCodec::Opus {
            let encoder = make("opusenc", "microphone-opus-encoder")?;
            set_enum_checked(&encoder, "audio-type", "voice")?;
            set_checked(
                &encoder,
                "bitrate",
                i32::try_from(
                    plan.opus_bitrate
                        .ok_or_else(|| Error::Backend("Opus bitrate is absent".to_owned()))?,
                )
                .map_err(|_| Error::Backend("Opus bitrate overflows gint".to_owned()))?,
            )?;
            set_enum_checked(
                &encoder,
                "frame-size",
                opus_frame_size_nick(packetizer.config().frame_duration_micros)?,
            )?;
            elements.push(encoder);
        }
        let sink_element = make("appsink", "microphone-sink")?;
        let sink = sink_element
            .clone()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| Error::Backend("microphone appsink has wrong runtime type".to_owned()))?;
        sink.set_sync(false);
        sink.set_drop(false);
        sink.set_max_buffers(8);
        elements.push(sink_element);
        let references: Vec<&gst::Element> = elements.iter().collect();
        pipeline
            .add_many(&references)
            .map_err(gst_error("cannot add microphone pipeline elements"))?;
        gst::Element::link_many(&references)
            .map_err(gst_error("cannot link microphone pipeline"))?;
        if let Err(error) = set_playing(&pipeline, "microphone pipeline") {
            let _ = stop_pipeline(&pipeline, "microphone pipeline");
            return Err(error);
        }
        Ok(Self { pipeline, sink })
    }

    fn check_bus(&self) -> Result<()> {
        check_bus(&self.pipeline, "microphone pipeline")
    }

    fn stop(&self) -> Result<()> {
        stop_pipeline(&self.pipeline, "microphone pipeline")
    }
}

impl Drop for MicrophoneGstPipeline {
    fn drop(&mut self) {
        let _ = stop_pipeline(&self.pipeline, "microphone pipeline");
    }
}

fn cancel_echo_on_capture(element: &gst::Element, canceller: SharedEchoCanceller) -> Result<()> {
    let pad = element
        .static_pad("src")
        .ok_or_else(|| Error::Backend("microphone caps filter has no output pad".to_owned()))?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data
            && let Ok(mut mapped) = buffer.make_mut().map_writable()
        {
            canceller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .process_s16le(mapped.as_mut_slice());
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or_else(|| Error::Backend("cannot add echo cancellation to the microphone".to_owned()))?;
    Ok(())
}

fn process_microphone_sample(
    sample: &gst::Sample,
    packetizer: &mut MicrophonePacketizer,
) -> Result<(Vec<MicrophonePacket>, Option<MicrophoneCaptureLevel>)> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| Error::Backend("microphone sample has no buffer".to_owned()))?;
    let mapped = buffer
        .map_readable()
        .map_err(|error| Error::Backend(format!("cannot map microphone sample: {error}")))?;
    match packetizer.config().codec {
        MicrophoneCodec::PcmS16 | MicrophoneCodec::RawPcmS16Le => packetizer
            .push_pcm_s16le(mapped.as_slice())
            .map(|packets| (packets, pcm_s16le_level(mapped.as_slice()))),
        MicrophoneCodec::Opus => packetizer
            .packetize_opus(mapped.as_slice())
            .map(|packet| (vec![packet], None)),
    }
}

fn pcm_s16le_level(pcm: &[u8]) -> Option<MicrophoneCaptureLevel> {
    if pcm.is_empty() || pcm.len() % 2 != 0 {
        return None;
    }
    let mut sum_squares = 0_u128;
    let mut peak = 0_u32;
    let mut count = 0_u64;
    for sample in pcm.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]);
        let magnitude = i32::from(value).unsigned_abs();
        peak = peak.max(magnitude);
        sum_squares = sum_squares.saturating_add(u128::from(magnitude) * u128::from(magnitude));
        count = count.saturating_add(1);
    }
    let mean_square = sum_squares.checked_div(u128::from(count))?;
    let rms = (mean_square as f64).sqrt().round() as u64;
    Some(MicrophoneCaptureLevel {
        rms_q15: u16::try_from(rms.min(i16::MAX as u64)).unwrap_or(u16::MAX),
        peak_q15: u16::try_from(peak.min(i16::MAX as u32)).unwrap_or(u16::MAX),
    })
}

fn probe_target_video_decoder() -> Result<()> {
    validate_video_decoder_target(
        Path::new(TARGET_VIDEO_DEVICE),
        Path::new(TARGET_VIDEO_DRIVER_ATTRIBUTE),
    )?;
    let metadata = fs::metadata(TARGET_VIDEO_DEVICE)
        .map_err(|error| Error::Backend(format!("cannot inspect /dev/video1: {error}")))?;
    if !metadata.file_type().is_char_device() {
        return Err(Error::Backend(
            "required /dev/video1 target is not a character device".to_owned(),
        ));
    }
    Ok(())
}

fn new_pipeline_control() -> PipelineControl {
    Arc::new(Mutex::new(None))
}

fn register_pipeline(control: &PipelineControl, pipeline: &gst::Pipeline) -> Result<()> {
    let mut active = control
        .lock()
        .map_err(|_| Error::Backend("pipeline control lock is poisoned".to_owned()))?;
    *active = Some(pipeline.clone());
    Ok(())
}

fn interrupt_pipeline(control: &PipelineControl) -> Result<()> {
    let pipeline = control
        .lock()
        .map_err(|_| Error::Backend("pipeline control lock is poisoned".to_owned()))?
        .clone();
    if let Some(pipeline) = pipeline {
        unblock_pipeline_sources(&pipeline);
        pipeline.set_state(gst::State::Null).map_err(|error| {
            Error::Backend(format!("cannot interrupt media pipeline: {error:?}"))
        })?;
    }
    Ok(())
}

fn unblock_pipeline_sources(pipeline: &gst::Pipeline) {
    for source in pipeline.iterate_sources().into_iter().flatten() {
        if let Some(peer) = source.static_pad("src").and_then(|pad| pad.peer()) {
            let _ = peer.send_event(gst::event::FlushStart::new());
        }
    }
}

fn begin_audio_pipeline_flush(control: &PipelineControl) -> Result<()> {
    send_audio_pipeline_flush_event(control, true)
}

fn finish_audio_pipeline_flush(control: &PipelineControl) -> Result<()> {
    send_audio_pipeline_flush_event(control, false)
}

fn send_audio_pipeline_flush_event(control: &PipelineControl, starting: bool) -> Result<()> {
    let pipeline = control
        .lock()
        .map_err(|_| Error::Backend("pipeline control lock is poisoned".to_owned()))?
        .clone()
        .ok_or(Error::NotReady {
            component: "audio mixer",
            state: "stopped",
        })?;
    let mut found = false;
    let mut accepted = true;
    for source_name in [
        "audio-output-test-tone",
        "audio-source-100",
        "audio-source-101",
        "audio-source-102",
    ] {
        let Some(source) = pipeline.by_name(source_name) else {
            continue;
        };
        found = true;
        accepted &= if starting {
            source.send_event(gst::event::FlushStart::new())
        } else {
            source.send_event(gst::event::FlushStop::new(false))
        };
    }
    if !found || !accepted {
        return Err(Error::Backend(format!(
            "audio pipeline rejected {}",
            if starting {
                "flush start"
            } else {
                "flush stop"
            }
        )));
    }
    Ok(())
}

fn clear_pipeline(control: &PipelineControl) -> Result<()> {
    let mut active = control
        .lock()
        .map_err(|_| Error::Backend("pipeline control lock is poisoned".to_owned()))?;
    active.take();
    Ok(())
}

fn caps(plan: &CapsPlan) -> Result<gst::Caps> {
    let mut builder = gst::Caps::builder(plan.media_type);
    for (name, value) in &plan.fields {
        builder = match value {
            CapsValue::String(value) => builder.field(*name, value.as_str()),
            CapsValue::Signed(value) => builder.field(*name, *value),
            CapsValue::Unsigned(value) => builder.field(*name, *value),
            CapsValue::Fraction(numerator, denominator) => builder.field(
                *name,
                gst::Fraction::new(
                    i32::try_from(*numerator).map_err(|_| {
                        Error::Backend("caps fraction numerator overflows".to_owned())
                    })?,
                    i32::try_from(*denominator).map_err(|_| {
                        Error::Backend("caps fraction denominator overflows".to_owned())
                    })?,
                ),
            ),
            CapsValue::Bytes(value) => builder.field(*name, gst::Buffer::from_slice(value.clone())),
        };
    }
    Ok(builder.build())
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|error| {
            Error::Backend(format!(
                "required GStreamer element {factory} is unavailable: {error}"
            ))
        })
}

fn set_checked<Object, Value>(object: &Object, name: &str, value: Value) -> Result<()>
where
    Object: glib::prelude::IsA<glib::Object>,
    Value: ToValue,
{
    let object = object.as_ref();
    let specification = object.find_property(name).ok_or_else(|| {
        Error::Backend(format!("GStreamer object has no required {name} property"))
    })?;
    if !specification.flags().contains(glib::ParamFlags::WRITABLE) {
        return Err(Error::Backend(format!(
            "GStreamer {name} property is not writable"
        )));
    }
    let value = value.to_value();
    if value.type_() != specification.value_type() {
        return Err(Error::Backend(format!(
            "GStreamer {name} property type does not match the configured value"
        )));
    }
    object.set_property_from_value(name, &value);
    Ok(())
}

fn set_enum_checked<Object>(object: &Object, name: &str, nick: &str) -> Result<()>
where
    Object: glib::prelude::IsA<glib::Object>,
{
    let object = object.as_ref();
    let specification = object.find_property(name).ok_or_else(|| {
        Error::Backend(format!("GStreamer object has no required {name} property"))
    })?;
    if !specification.flags().contains(glib::ParamFlags::WRITABLE) {
        return Err(Error::Backend(format!(
            "GStreamer {name} property is not writable"
        )));
    }
    let enum_class = glib::EnumClass::with_type(specification.value_type())
        .ok_or_else(|| Error::Backend(format!("GStreamer {name} property is not an enum")))?;
    let value = enum_class.to_value_by_nick(nick).ok_or_else(|| {
        Error::Backend(format!(
            "GStreamer {name} property does not support the required value"
        ))
    })?;
    object.set_property_from_value(name, &value);
    Ok(())
}

fn require_string_property_equals<Object>(object: &Object, name: &str, expected: &str) -> Result<()>
where
    Object: glib::prelude::IsA<glib::Object>,
{
    let object = object.as_ref();
    let specification = object.find_property(name).ok_or_else(|| {
        Error::Backend(format!("GStreamer object has no required {name} property"))
    })?;
    if !specification.flags().contains(glib::ParamFlags::READABLE) {
        return Err(Error::Backend(format!(
            "GStreamer {name} property is not readable"
        )));
    }
    let actual = object
        .property_value(name)
        .get::<String>()
        .map_err(|_| Error::Backend(format!("GStreamer {name} property is not a string")))?;
    if actual != expected {
        return Err(Error::Backend(format!(
            "GStreamer {name} property does not select the required target"
        )));
    }
    Ok(())
}

const fn gain_as_f64(gain_q15: u16) -> f64 {
    gain_q15 as f64 / UNITY_GAIN_Q15 as f64
}

fn opus_frame_size_nick(frame_duration_micros: u32) -> Result<&'static str> {
    match frame_duration_micros {
        2_500 => Ok("2.5"),
        5_000 => Ok("5"),
        10_000 => Ok("10"),
        20_000 => Ok("20"),
        40_000 => Ok("40"),
        60_000 => Ok("60"),
        _ => Err(Error::InvalidAudio(
            "Opus frame duration has no GStreamer enum value".to_owned(),
        )),
    }
}

fn require_factory(factory: &str) -> Result<()> {
    gst::ElementFactory::find(factory).map_or_else(
        || {
            Err(Error::Backend(format!(
                "required GStreamer element {factory} is unavailable"
            )))
        },
        |_| Ok(()),
    )
}

fn probe_alsa_output(device: &str) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let sink = make("alsasink", "audio-output-preflight")?;
    set_checked(&sink, "device", device)?;
    pipeline
        .add(&sink)
        .map_err(gst_error("cannot add audio preflight sink"))?;
    let start = (|| {
        pipeline.set_state(gst::State::Ready).map_err(|error| {
            Error::Backend(format!("cannot open selected audio output: {error:?}"))
        })?;
        let (transition, current, pending) = pipeline.state(GST_STATE_TIMEOUT);
        transition.map_err(|error| {
            Error::Backend(format!(
                "selected audio output did not reach Ready: {error:?}"
            ))
        })?;
        require_settled_state(
            lifecycle_state(current),
            lifecycle_state(pending),
            PipelineLifecycleState::Ready,
            "audio output preflight",
        )
    })();
    let stop = stop_pipeline(&pipeline, "audio output preflight");
    match (start, stop) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

fn prime_alsa_output(device: &str, latency: AudioLatencyConfig) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let source = make("audiotestsrc", "audio-output-prime-source")?;
    set_checked(&source, "is-live", true)?;
    set_enum_checked(&source, "wave", "silence")?;
    let convert = make("audioconvert", "audio-output-prime-converter")?;
    let resample = make("audioresample", "audio-output-prime-resampler")?;
    let output_caps = make("capsfilter", "audio-output-prime-caps")?;
    set_checked(
        &output_caps,
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", 48_000_i32)
            .field("channels", 2_i32)
            .build(),
    )?;
    let sink = make("alsasink", "audio-output-prime-sink")?;
    set_checked(&sink, "device", device)?;
    set_checked(&sink, "sync", true)?;
    set_checked(&sink, "buffer-time", i64::from(latency.sink_buffer_micros))?;
    set_checked(&sink, "latency-time", i64::from(latency.sink_period_micros))?;
    pipeline
        .add_many([&source, &convert, &resample, &output_caps, &sink])
        .map_err(gst_error("cannot add audio output prime elements"))?;
    gst::Element::link_many([&source, &convert, &resample, &output_caps, &sink])
        .map_err(gst_error("cannot link audio output prime elements"))?;
    let start = (|| {
        set_playing(&pipeline, "audio output prime")?;
        thread::sleep(AUDIO_OUTPUT_PRIME_DURATION);
        check_bus(&pipeline, "audio output prime")
    })();
    let stop = stop_pipeline(&pipeline, "audio output prime");
    match (start, stop) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

fn probe_alsa_input(device: &str) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let source = make("alsasrc", "microphone-input-preflight")?;
    set_checked(&source, "device", device)?;
    let sink = make("fakesink", "microphone-preflight-sink")?;
    pipeline
        .add_many([&source, &sink])
        .map_err(gst_error("cannot add microphone preflight elements"))?;
    source
        .link(&sink)
        .map_err(gst_error("cannot link microphone preflight pipeline"))?;
    let start = (|| {
        pipeline.set_state(gst::State::Ready).map_err(|error| {
            Error::Backend(format!("cannot open selected microphone input: {error:?}"))
        })?;
        let (transition, current, pending) = pipeline.state(GST_STATE_TIMEOUT);
        transition.map_err(|error| {
            Error::Backend(format!(
                "selected microphone input did not reach Ready: {error:?}"
            ))
        })?;
        require_settled_state(
            lifecycle_state(current),
            lifecycle_state(pending),
            PipelineLifecycleState::Ready,
            "microphone input preflight",
        )
    })();
    let stop = stop_pipeline(&pipeline, "microphone input preflight");
    match (start, stop) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

fn probe_resolved_microphone_input(input: &ResolvedMicrophoneInput) -> Result<()> {
    probe_alsa_input(input.device())
}

fn probe_resolved_audio_output(output: &ResolvedAudioOutput) -> Result<()> {
    probe_alsa_output(output.device())
}

fn set_playing(pipeline: &gst::Pipeline, description: &str) -> Result<()> {
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Backend(format!("cannot start {description}: {error:?}")))?;
    wait_for_playing(pipeline, description)
}

fn wait_for_playing(pipeline: &gst::Pipeline, description: &str) -> Result<()> {
    let (transition, current, pending) = pipeline.state(GST_STATE_TIMEOUT);
    transition.map_err(|error| {
        Error::Backend(format!("{description} did not reach Playing: {error:?}"))
    })?;
    require_settled_state(
        lifecycle_state(current),
        lifecycle_state(pending),
        PipelineLifecycleState::Playing,
        description,
    )
}

fn start_video_pipeline(pipeline: &gst::Pipeline) -> Result<bool> {
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Backend(format!("cannot start video pipeline: {error:?}")))?;
    let (transition, current, pending) = pipeline.state(GST_STATE_TIMEOUT);
    transition.map_err(|error| {
        Error::Backend(format!("video pipeline did not begin Playing: {error:?}"))
    })?;
    video_start_is_settled(
        lifecycle_state(current),
        lifecycle_state(pending),
        "video pipeline",
    )
}

fn stop_pipeline(pipeline: &gst::Pipeline, description: &str) -> Result<()> {
    unblock_pipeline_sources(pipeline);
    pipeline
        .set_state(gst::State::Null)
        .map_err(|error| Error::Backend(format!("cannot stop {description}: {error:?}")))?;
    let (transition, current, pending) = pipeline.state(GST_STATE_TIMEOUT);
    transition
        .map_err(|error| Error::Backend(format!("{description} did not reach Null: {error:?}")))?;
    require_settled_state(
        lifecycle_state(current),
        lifecycle_state(pending),
        PipelineLifecycleState::Null,
        description,
    )
}

const fn lifecycle_state(state: gst::State) -> PipelineLifecycleState {
    match state {
        gst::State::VoidPending => PipelineLifecycleState::VoidPending,
        gst::State::Null => PipelineLifecycleState::Null,
        gst::State::Ready => PipelineLifecycleState::Ready,
        gst::State::Paused => PipelineLifecycleState::Paused,
        gst::State::Playing => PipelineLifecycleState::Playing,
    }
}

fn check_bus(pipeline: &gst::Pipeline, description: &str) -> Result<()> {
    let Some(bus) = pipeline.bus() else {
        return Err(Error::Backend(format!("{description} has no bus")));
    };
    while let Some(message) = bus.timed_pop_filtered(
        gst::ClockTime::ZERO,
        &[gst::MessageType::Error, gst::MessageType::Eos],
    ) {
        match message.view() {
            gst::MessageView::Error(error) => {
                return Err(Error::Backend(format!(
                    "{description} error from {}: {} ({:?})",
                    error
                        .src()
                        .map(|source| source.path_string())
                        .unwrap_or_else(|| "unknown".into()),
                    error.error(),
                    error.debug()
                )));
            }
            gst::MessageView::Eos(_) => {
                return Err(Error::Backend(format!("{description} reached EOS")));
            }
            _ => {}
        }
    }
    Ok(())
}

fn try_receive<T>(receiver: &Mutex<Receiver<T>>, description: &str) -> Result<Option<T>> {
    let receiver = receiver
        .lock()
        .map_err(|_| Error::Backend(format!("{description} lock is poisoned")))?;
    match receiver.try_recv() {
        Ok(value) => Ok(Some(value)),
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Ok(None),
    }
}

fn report_failure(channel: &SyncSender<BackendFailure>, component: &'static str, error: &Error) {
    let _ = channel.try_send(BackendFailure {
        component,
        message: error.to_string(),
    });
}

fn validate_command_capacity(capacity: usize) -> Result<()> {
    if capacity == 0 || capacity > MAXIMUM_COMMAND_CAPACITY {
        return Err(Error::Backend(
            "backend channel capacity is outside 1..=1024".to_owned(),
        ));
    }
    Ok(())
}

fn gst_error(context: &'static str) -> impl FnOnce(gst::glib::BoolError) -> Error + use<> {
    move |error| Error::Backend(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_pcm(stream_id: AudioStreamId, generation: Option<u64>) -> QueuedAudioFrame {
        let format_id = AudioFormatId::new(0x8000);
        QueuedAudioFrame {
            generation,
            frame: EncryptedAudioFrame {
                stream_id,
                format_id,
                codec: format_id.codec().expect("PCM codec"),
                sequence: 0,
                timestamp: 0,
                ssrc: 1,
                nonce_counter: 0,
                payload: vec![0; 4],
                rtp_packet: vec![0; 12],
            },
        }
    }

    #[test]
    fn navigation_has_independent_ingress_and_stale_generations_are_rejected() {
        let (main_tx, main_rx) = mpsc::sync_channel(1);
        let (alternate_tx, alternate_rx) = mpsc::sync_channel(1);
        let (high_tx, high_rx) = mpsc::sync_channel(1);
        main_tx
            .try_send(queued_pcm(AudioStreamId::Main, Some(1)))
            .expect("fill Main queue");
        assert!(
            main_tx
                .try_send(queued_pcm(AudioStreamId::Main, Some(1)))
                .is_err()
        );
        alternate_tx
            .try_send(queued_pcm(AudioStreamId::Alternate, Some(9)))
            .expect("navigation queue remains available");
        let receivers = BTreeMap::from([
            (AudioStreamId::Main, main_rx),
            (AudioStreamId::Alternate, alternate_rx),
            (AudioStreamId::High, high_rx),
        ]);
        drop(high_tx);
        let mut next_index = 0;
        let navigation = try_next_audio_frame(&receivers, &mut next_index).expect("navigation");
        assert_eq!(navigation.frame.stream_id, AudioStreamId::Alternate);

        let generations = BTreeMap::from([(AudioStreamId::Alternate, 9)]);
        assert!(audio_frame_generation_is_current(&generations, &navigation));
        assert!(!audio_frame_generation_is_current(
            &generations,
            &queued_pcm(AudioStreamId::Alternate, Some(8))
        ));
        assert!(audio_frame_generation_is_current(
            &generations,
            &queued_pcm(AudioStreamId::Alternate, None)
        ));
    }

    #[test]
    fn pcm_appsrc_preserves_explicit_running_time_timestamps() {
        assert!(!appsrc_timestamps_arrival(EncodedAudioCodec::PcmS16Be {
            sample_rate_hz: 48_000,
            channels: 2,
        }));
        assert!(appsrc_timestamps_arrival(EncodedAudioCodec::Opus {
            sample_rate_hz: 48_000,
            channels: 1,
        }));
        assert!(appsrc_timestamps_arrival(EncodedAudioCodec::AacLc {
            sample_rate_hz: 48_000,
            channels: 2,
        }));
    }

    #[test]
    fn pcm_clock_is_anchored_to_live_pipeline_time_and_survives_rtp_wrap() {
        let first_timestamp = u32::MAX - 239;
        let origin =
            PcmClockOrigin::new(first_timestamp, 7_000_000_000, 32).expect("live PCM origin");
        assert_eq!(
            origin
                .timing(first_timestamp, 1_920, 48_000, 2)
                .expect("first PCM frame"),
            (7_032_000_000, 10_000_000)
        );
        assert_eq!(
            origin
                .timing(first_timestamp.wrapping_add(480), 1_920, 48_000, 2)
                .expect("wrapped PCM frame"),
            (7_042_000_000, 10_000_000)
        );
        assert!(origin.timing(first_timestamp, 1_918, 48_000, 2).is_err());
        assert!(PcmClockOrigin::new(0, u64::MAX, 1).is_err());
    }

    #[test]
    fn late_pcm_reanchors_to_live_pipeline_time_instead_of_falling_behind() {
        let (origin, pts, _) =
            PcmClockOrigin::schedule(None, 0, 1_000_000_000, 200, 1_920, 48_000, 2)
                .expect("first PCM frame");
        assert_eq!(pts, 1_200_000_000);
        let (same, pts, _) =
            PcmClockOrigin::schedule(Some(origin), 480, 1_100_000_000, 200, 1_920, 48_000, 2)
                .expect("on-time PCM frame");
        assert_eq!(same, origin);
        assert_eq!(pts, 1_210_000_000);
        let (reanchored, pts, _) =
            PcmClockOrigin::schedule(Some(origin), 960, 5_000_000_000, 200, 1_920, 48_000, 2)
                .expect("PCM frame after a gap");
        assert_ne!(reanchored, origin);
        assert_eq!(pts, 5_200_000_000);
        let (_, pts, _) = PcmClockOrigin::schedule(
            Some(reanchored),
            1_440,
            5_005_000_000,
            200,
            1_920,
            48_000,
            2,
        )
        .expect("PCM frame continuing after re-anchor");
        assert_eq!(pts, 5_210_000_000);
    }

    #[test]
    fn microphone_levels_are_bounded_and_never_expose_samples() {
        assert_eq!(
            pcm_s16le_level(&[0, 0, 0, 0]),
            Some(MicrophoneCaptureLevel {
                rms_q15: 0,
                peak_q15: 0,
            })
        );
        let voice = [1_000_i16.to_le_bytes(), (-1_000_i16).to_le_bytes()].concat();
        assert_eq!(
            pcm_s16le_level(&voice),
            Some(MicrophoneCaptureLevel {
                rms_q15: 1_000,
                peak_q15: 1_000,
            })
        );
        assert_eq!(
            pcm_s16le_level(&i16::MIN.to_le_bytes()),
            Some(MicrophoneCaptureLevel {
                rms_q15: i16::MAX as u16,
                peak_q15: i16::MAX as u16,
            })
        );
        assert_eq!(pcm_s16le_level(&[]), None);
        assert_eq!(pcm_s16le_level(&[0]), None);
    }

    fn skip(reason: &str) {
        eprintln!("SKIP target media integration: {reason}");
    }

    fn require_host_audio_or_skip(reason: &str) {
        if std::env::var_os("PRELUDE_REQUIRE_HOST_AUDIO_TEST").is_some() {
            panic!("required host audio integration is unavailable: {reason}");
        }
        skip(reason);
    }

    #[test]
    fn video_delay_queue_and_telemetry_are_bounded_and_saturating() {
        let delay = AudioOutputDelay::configured_calibration(275).expect("calibration");
        assert_eq!(video_sink_capacity(AudioOutputDelay::uncompensated()), 2);
        assert_eq!(video_sink_capacity(delay), 11);

        let telemetry = VideoSynchronizationTelemetry::new(delay);
        {
            let mut snapshot = telemetry.snapshot.lock().expect("telemetry");
            snapshot.presented_frames = u64::MAX;
            snapshot.presentation_deadline_miss_events = u64::MAX;
            snapshot.total_scheduled_wait_ns = u64::MAX;
            snapshot.maximum_presentation_lateness_ns = 1;
        }
        telemetry.record_presentation(
            VideoPresentationSchedule {
                deadline_ns: 10,
                wait_ns: u64::MAX,
                lateness_ns: 0,
            },
            VIDEO_FRAME_DURATION_NS + 1,
        );
        let snapshot = telemetry.snapshot().expect("snapshot");
        assert_eq!(snapshot.output_delay, delay);
        assert_eq!(snapshot.presented_frames, u64::MAX);
        assert_eq!(snapshot.presentation_deadline_miss_events, u64::MAX);
        assert_eq!(snapshot.total_scheduled_wait_ns, u64::MAX);
        assert_eq!(
            snapshot.maximum_presentation_lateness_ns,
            VIDEO_FRAME_DURATION_NS + 1
        );
    }

    #[test]
    fn target_plugins_are_probed_without_software_fallback() {
        if gst::init().is_err() {
            skip("GStreamer runtime is unavailable");
            return;
        }
        for factory in [
            "appsrc",
            "h264parse",
            "v4l2h264dec",
            "appsink",
            "audiomixer",
            "audiotestsrc",
            "rtpopusdepay",
            "opusdec",
            "rtpmp4gdepay",
            "aacparse",
            "faad",
            "alsasink",
            "alsasrc",
            "opusenc",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                skip(&format!(
                    "required GStreamer factory {factory} is unavailable"
                ));
                return;
            }
        }
    }

    #[test]
    fn target_video_probe_skips_when_device_is_absent() {
        match probe_target_video_decoder() {
            Ok(()) => {}
            Err(error) => skip(&error.to_string()),
        }
    }

    #[test]
    fn sixteen_khz_opus_siri_audio_negotiates_and_decodes() {
        if gst::init().is_err() {
            skip("GStreamer runtime is unavailable");
            return;
        }
        for factory in ["opusenc", "rtpjitterbuffer", "rtpopusdepay", "opusdec"] {
            if gst::ElementFactory::find(factory).is_none() {
                skip(&format!(
                    "required GStreamer factory {factory} is unavailable"
                ));
                return;
            }
        }
        let format = AudioFormatId::new(0x1000_0000);
        let codec = format.codec().expect("codec");
        let plan = crate::AudioStreamPlan::new(AudioStreamId::Main, format).expect("plan");
        let encoder = gst::parse::launch(
            "audiotestsrc num-buffers=25 samplesperbuffer=320 ! \
             audio/x-raw,format=S16LE,layout=interleaved,rate=16000,channels=1 ! \
             opusenc frame-size=20 ! appsink name=sink sync=false",
        )
        .expect("encoder")
        .downcast::<gst::Pipeline>()
        .expect("encoder pipeline");
        let encoded = encoder
            .by_name("sink")
            .expect("encoder sink")
            .downcast::<gst_app::AppSink>()
            .expect("encoder appsink");
        encoder.set_state(gst::State::Playing).expect("encoding");
        let mut payloads = Vec::new();
        while let Some(sample) = encoded.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            let buffer = sample.buffer().expect("Opus buffer");
            payloads.push(buffer.map_readable().expect("Opus map").to_vec());
        }
        encoder
            .set_state(gst::State::Null)
            .expect("encoder stopped");

        let decoder = gst::parse::launch(
            "appsrc name=src format=time is-live=true do-timestamp=true ! \
             rtpjitterbuffer latency=100 ! rtpopusdepay ! opusdec ! \
             appsink name=sink sync=false",
        )
        .expect("decoder")
        .downcast::<gst::Pipeline>()
        .expect("decoder pipeline");
        let source = decoder
            .by_name("src")
            .expect("decoder source")
            .downcast::<gst_app::AppSrc>()
            .expect("decoder appsrc");
        source.set_caps(Some(&caps(&plan.input_caps).expect("planned caps")));
        let decoded = decoder
            .by_name("sink")
            .expect("decoder sink")
            .downcast::<gst_app::AppSink>()
            .expect("decoder appsink");
        decoder.set_state(gst::State::Playing).expect("decoding");
        for (index, payload) in payloads.into_iter().enumerate() {
            let sequence = u16::try_from(index).expect("sequence");
            let timestamp = u32::from(sequence) * 320;
            let mut rtp_packet = vec![0x80, AudioStreamId::Main as u8];
            rtp_packet.extend_from_slice(&sequence.to_be_bytes());
            rtp_packet.extend_from_slice(&timestamp.to_be_bytes());
            rtp_packet.extend_from_slice(&3_u32.to_be_bytes());
            rtp_packet.extend_from_slice(&payload);
            let frame = EncryptedAudioFrame {
                stream_id: AudioStreamId::Main,
                format_id: format,
                codec,
                sequence,
                timestamp,
                ssrc: 3,
                nonce_counter: u64::from(sequence),
                payload,
                rtp_packet,
            };
            source
                .push_buffer(gst::Buffer::from_mut_slice(
                    plan.adapt_frame(&frame).expect("adapted Opus"),
                ))
                .expect("Opus RTP accepted");
        }
        source.end_of_stream().expect("end of stream");
        let mut decoded_samples = 0;
        while let Some(sample) = decoded.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            decoded_samples += sample.buffer().map_or(0, gst::BufferRef::size) / 2;
        }
        let failure = decoder
            .bus()
            .expect("decoder bus")
            .pop_filtered(&[gst::MessageType::Error]);
        decoder
            .set_state(gst::State::Null)
            .expect("decoder stopped");
        assert!(failure.is_none(), "decoder failed: {failure:?}");
        assert!(decoded_samples >= 48_000 * 2 / 5);
    }

    #[test]
    fn target_property_types_accept_required_values() {
        if gst::init().is_err() {
            skip("GStreamer runtime is unavailable");
            return;
        }
        let encoder = match make("opusenc", "property-test-opus-encoder") {
            Ok(encoder) => encoder,
            Err(error) => {
                skip(&error.to_string());
                return;
            }
        };
        set_enum_checked(&encoder, "audio-type", "voice").expect("voice enum");
        set_enum_checked(&encoder, "frame-size", "20").expect("frame size enum");
        set_checked(&encoder, "bitrate", 96_000_i32).expect("bitrate gint");

        let sink = match make("alsasink", "property-test-alsa-sink") {
            Ok(sink) => sink,
            Err(error) => {
                skip(&error.to_string());
                return;
            }
        };
        set_checked(&sink, "sync", true).expect("clock sync boolean");
        set_checked(&sink, "buffer-time", 100_000_i64).expect("buffer time gint64");
        set_checked(&sink, "latency-time", 20_000_i64).expect("latency time gint64");

        let parser = match make("h264parse", "property-test-h264-parser") {
            Ok(parser) => parser,
            Err(error) => {
                skip(&error.to_string());
                return;
            }
        };
        set_checked(&parser, "config-interval", -1_i32).expect("parameter sets before every IDR");
        let parser_caps = match make("capsfilter", "property-test-h264-caps") {
            Ok(filter) => filter,
            Err(error) => {
                skip(&error.to_string());
                return;
            }
        };
        set_checked(
            &parser_caps,
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        )
        .expect("byte-stream access-unit caps");

        let decoder = match make("v4l2h264dec", "property-test-video-decoder") {
            Ok(decoder) => decoder,
            Err(error) => {
                skip(&error.to_string());
                return;
            }
        };
        require_string_property_equals(&decoder, "device", TARGET_VIDEO_DEVICE)
            .expect("target decoder device");
        set_enum_checked(&decoder, "output-io-mode", "mmap").expect("decoder output mmap");
        set_enum_checked(&decoder, "capture-io-mode", "mmap").expect("decoder capture mmap");
    }

    #[test]
    fn rtp_jitter_buffers_do_not_delay_the_other_mixer_streams() {
        gst::init().expect("GStreamer");
        let pcm = AudioFormatId::new(0x8000);
        let opus = AudioFormatId::new(0x4000_0000);
        let aac = AudioFormatId::new(0x80_0000);
        let wired = [
            AudioStreamConfig::with_negotiated_latency(AudioStreamId::Main, pcm, Some(32)),
            AudioStreamConfig::new(AudioStreamId::Alternate, pcm),
        ];
        let wireless = [
            AudioStreamConfig::new(AudioStreamId::Main, opus),
            AudioStreamConfig::new(AudioStreamId::Alternate, opus),
            AudioStreamConfig::with_negotiated_latency(AudioStreamId::High, aac, Some(1_000)),
        ];
        for streams in [&wired[..], &wireless[..]] {
            let latency = AudioLatencyConfig::default();
            let plan =
                AudioMixerPlan::with_stream_options(streams, AudioMixPolicy::default(), latency)
                    .expect("mixer plan");
            let pipeline = AudioGstPipeline::start(plan, "null").expect("null mixer");
            let mut query = gst::query::Latency::new();
            assert!(pipeline.pipeline.query(&mut query));
            let (live, minimum, _) = query.result();
            assert!(live);
            assert_eq!(
                minimum.useconds(),
                u64::from(latency.output_latency_micros())
            );
        }
    }

    #[test]
    fn mixer_output_feeds_the_echo_reference_while_a_microphone_listens() {
        gst::init().expect("GStreamer");
        let plan = AudioMixerPlan::new(&[(AudioStreamId::Main, AudioFormatId::new(0x8000))])
            .expect("mixer plan");
        let reference = crate::EchoReference::new();
        let _canceller = crate::EchoCanceller::attach(&reference, 48_000);
        let _pipeline = AudioGstPipeline::start_with_equalizer(
            plan,
            "null",
            Default::default(),
            Some(reference.clone()),
        )
        .expect("null mixer");
        let deadline = Instant::now() + Duration::from_secs(2);
        while reference.pending_samples() < 48_000 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(reference.pending_samples() >= 48_000);
    }

    #[test]
    fn microphone_echo_canceller_rewrites_capture_buffers() {
        gst::init().expect("GStreamer");
        let reference = crate::EchoReference::new();
        let canceller = Arc::new(Mutex::new(crate::EchoCanceller::attach(&reference, 48_000)));
        let pipeline = gst::Pipeline::new();
        let source = make("audiotestsrc", "capture").expect("source");
        set_checked(&source, "num-buffers", 50_i32).expect("buffers");
        set_checked(&source, "samplesperbuffer", 240_i32).expect("buffer size");
        let filter = make("capsfilter", "capture-caps").expect("caps");
        set_checked(
            &filter,
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("layout", "interleaved")
                .field("rate", 48_000_i32)
                .field("channels", 1_i32)
                .build(),
        )
        .expect("caps");
        cancel_echo_on_capture(&filter, Arc::clone(&canceller)).expect("probe");
        let sink_element = make("appsink", "capture-sink").expect("sink");
        let sink = sink_element
            .clone()
            .downcast::<gst_app::AppSink>()
            .expect("appsink");
        sink.set_sync(false);
        pipeline
            .add_many([&source, &filter, &sink_element])
            .expect("add");
        gst::Element::link_many([&source, &filter, &sink_element]).expect("link");
        pipeline.set_state(gst::State::Playing).expect("play");
        let mut samples = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let buffer = sample.buffer().expect("buffer");
            let mapped = buffer.map_readable().expect("map");
            assert_eq!(mapped.len(), 480);
            samples.extend(
                mapped
                    .chunks_exact(2)
                    .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]])),
            );
        }
        pipeline.set_state(gst::State::Null).expect("stop");
        assert_eq!(samples.len(), 50 * 240);
        assert!(samples[..480].iter().all(|sample| *sample == 0));
        assert!(samples[480..].iter().any(|sample| *sample != 0));
    }

    #[test]
    fn siri_to_media_reconfiguration_unblocks_a_full_branch() {
        assert_full_siri_branch_is_released(false);
    }

    #[test]
    fn shutdown_unblocks_a_full_siri_branch() {
        assert_full_siri_branch_is_released(true);
    }

    fn assert_full_siri_branch_is_released(shutdown: bool) {
        gst::init().expect("GStreamer");
        let voice = AudioFormatId::new(0x40);
        let plan = AudioMixerPlan::new(&[
            (AudioStreamId::Main, voice),
            (AudioStreamId::Alternate, AudioFormatId::new(0x8)),
        ])
        .expect("mixer plan");
        let mut pipeline = AudioGstPipeline::start(plan, "null").expect("null mixer");
        let queue = pipeline
            .pipeline
            .by_name("audio-input-queue-100")
            .expect("queue");
        set_checked(&queue, "max-size-buffers", 1_u32).expect("one buffer queue");
        let pad = queue.static_pad("src").expect("queue source");
        let (blocked_tx, blocked_rx) = mpsc::sync_channel(1);
        let probe = pad
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    let _ = blocked_tx.try_send(());
                    gst::PadProbeReturn::Ok
                },
            )
            .expect("block downstream");
        for sequence in 0..8_u16 {
            pipeline
                .push(EncryptedAudioFrame {
                    stream_id: AudioStreamId::Main,
                    format_id: voice,
                    codec: voice.codec().expect("voice codec"),
                    sequence,
                    timestamp: u32::from(sequence) * 240,
                    ssrc: 1,
                    nonce_counter: u64::from(sequence),
                    payload: vec![0; 480],
                    rtp_packet: vec![0; 12],
                })
                .expect("voice preroll");
        }
        blocked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked branch");
        let deadline = Instant::now() + Duration::from_secs(1);
        while queue.property::<u32>("current-level-buffers") != 1 {
            assert!(Instant::now() < deadline, "queue did not fill");
            thread::sleep(Duration::from_millis(5));
        }
        pipeline
            .deactivate_stream(AudioStreamId::Main)
            .expect("retire Siri");
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = if shutdown {
                let control = new_pipeline_control();
                register_pipeline(&control, &pipeline.pipeline).expect("register mixer");
                interrupt_pipeline(&control)
            } else {
                pipeline.configure_stream(AudioStreamConfig::new(
                    AudioStreamId::Main,
                    AudioFormatId::new(0x8),
                ))
            };
            done_tx.send(result).expect("report reconfiguration");
            pipeline
        });
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        pad.remove_probe(probe);
        let mut pipeline = worker.join().expect("reconfiguration worker");
        pipeline.stop().expect("stop mixer");
        result
            .expect("Siri-to-media reconfiguration blocked on the old stream")
            .expect("reconfigure");
    }

    #[test]
    fn live_backend_keeps_one_pipeline_across_navigation_and_applies_timed_ducking() {
        if let Err(error) = gst::init() {
            require_host_audio_or_skip(&format!("GStreamer runtime is unavailable: {error}"));
            return;
        }
        for factory in [
            "appsrc",
            "queue",
            "audiomixer",
            "audiotestsrc",
            "audioconvert",
            "audioresample",
            "capsfilter",
            "alsasink",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                require_host_audio_or_skip(&format!(
                    "required GStreamer factory {factory} is unavailable"
                ));
                return;
            }
        }
        probe_alsa_output("null").expect("ALSA null PCM preflight");
        let format = AudioFormatId::new(0x8000);
        let mut config = LinuxAudioBackendConfig::with_negotiated_route(
            AudioOutputRoute::TestAlsa("null".to_owned()),
            vec![
                AudioStreamConfig::new(AudioStreamId::Main, format),
                AudioStreamConfig::new(AudioStreamId::Alternate, format),
            ],
        );
        config.mix_policy = AudioMixPolicy::carplay();
        let backend = LinuxAudioBackend::start(config).expect("live null backend");
        backend
            .configure_stream(AudioStreamConfig::new(AudioStreamId::Main, format), 1)
            .expect("activate Main");
        backend
            .configure_stream(AudioStreamConfig::new(AudioStreamId::Alternate, format), 2)
            .expect("activate Alternate");
        let pipeline = backend
            .pipeline_control
            .lock()
            .expect("pipeline control")
            .clone()
            .expect("live pipeline");
        let mixer_pad = |stream_id: AudioStreamId| {
            pipeline
                .by_name(&format!("mixer-input-queue-{}", stream_id as u8))
                .expect("stream mixer queue")
                .static_pad("src")
                .expect("stream mixer queue source pad")
                .peer()
                .expect("audiomixer peer pad")
        };
        let main_pad = mixer_pad(AudioStreamId::Main);
        let alternate_pad = mixer_pad(AudioStreamId::Alternate);
        assert!((main_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);
        assert!((alternate_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);

        let frame = |stream_id, sequence, timestamp, nonce_counter| EncryptedAudioFrame {
            stream_id,
            format_id: format,
            codec: format.codec().expect("PCM codec"),
            sequence,
            timestamp,
            ssrc: if stream_id == AudioStreamId::Main {
                7
            } else {
                8
            },
            nonce_counter,
            payload: vec![0; 48_000 / 100 * 2 * 2],
            rtp_packet: vec![0; 12],
        };
        backend
            .try_submit_generation(1, frame(AudioStreamId::Main, 0, 0, 0))
            .expect("submit initial Main frame");
        let pushed_deadline = Instant::now() + Duration::from_secs(1);
        while backend.telemetry().expect("telemetry").frames_pushed < 1 {
            assert!(
                Instant::now() < pushed_deadline,
                "Main frame was not pushed"
            );
            thread::sleep(Duration::from_millis(5));
        }

        backend
            .deactivate_stream(AudioStreamId::Alternate, 2)
            .expect("retire navigation generation");
        backend
            .configure_stream(AudioStreamConfig::new(AudioStreamId::Alternate, format), 3)
            .expect("reuse persistent navigation branch");
        backend
            .try_submit_generation(2, frame(AudioStreamId::Alternate, 0, 0, 0))
            .expect("queue stale navigation frame for rejection");
        thread::sleep(Duration::from_millis(30));
        assert_eq!(backend.telemetry().expect("telemetry").frames_pushed, 1);
        backend
            .try_submit_generation(3, frame(AudioStreamId::Alternate, 1, 480, 1))
            .expect("submit current navigation frame");
        backend
            .try_submit_generation(1, frame(AudioStreamId::Main, 1, 480, 1))
            .expect("submit uninterrupted Main frame");
        let continuity_deadline = Instant::now() + Duration::from_secs(1);
        while backend.telemetry().expect("telemetry").frames_pushed < 3 {
            assert!(
                Instant::now() < continuity_deadline,
                "Main and navigation did not continue through branch reuse"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let requested_gain = 0.25_f64;
        let expected_gain =
            gain_as_f64((requested_gain * f64::from(UNITY_GAIN_Q15)).round() as u16);
        backend
            .set_ducking_gain_ramped(true, Some(requested_gain), Duration::from_millis(120))
            .expect("request timed duck");
        thread::sleep(Duration::from_millis(30));
        let attack_gain = main_pad.property::<f64>("volume");
        assert!(attack_gain > expected_gain && attack_gain <= 1.0);
        let attack_deadline = Instant::now() + Duration::from_millis(400);
        loop {
            let gain = main_pad.property::<f64>("volume");
            if (gain - expected_gain).abs() < f64::EPSILON {
                break;
            }
            assert!(Instant::now() < attack_deadline, "duck ramp did not settle");
            thread::sleep(Duration::from_millis(5));
        }
        assert!((alternate_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);
        backend
            .set_ducking_gain_ramped(false, None, Duration::from_millis(120))
            .expect("request timed unduck");
        let release_deadline = Instant::now() + Duration::from_millis(400);
        loop {
            let gain = main_pad.property::<f64>("volume");
            if (gain - 1.0).abs() < f64::EPSILON {
                break;
            }
            assert!(
                Instant::now() < release_deadline,
                "unduck ramp did not settle"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let telemetry = backend.telemetry().expect("final telemetry");
        assert_eq!(telemetry.pipeline_start_events, 1);
        assert_eq!(telemetry.pipeline_stop_events, 0);
        backend.shutdown().expect("stop live null backend");
    }

    #[test]
    fn live_equalizer_survives_backend_reconnect_without_restarts_and_coexists_with_mute_duck() {
        if gst::init().is_err()
            || ["equalizer-nbands", "volume", "alsasink"]
                .into_iter()
                .any(|factory| gst::ElementFactory::find(factory).is_none())
        {
            require_host_audio_or_skip("equalizer elements unavailable");
            return;
        }
        let control = crate::EqualizerControl::default();
        let saved = crate::EqualizerSettings {
            enabled: true,
            bands_half_db: [6; 10],
            preamp_half_db: -12,
        };
        control.set(saved).expect("set before backend exists");
        let format = AudioFormatId::new(0x8000);
        let mut config = LinuxAudioBackendConfig::with_negotiated_route(
            AudioOutputRoute::TestAlsa("null".to_owned()),
            vec![AudioStreamConfig::new(AudioStreamId::Main, format)],
        );
        config.equalizer = control.clone();
        for reconnect in 0..2 {
            let backend = LinuxAudioBackend::start(config.clone()).expect("start/reconnect");
            backend
                .configure_stream(AudioStreamConfig::new(AudioStreamId::Main, format), 1)
                .expect("activate stream");
            backend
                .try_submit_generation(
                    1,
                    EncryptedAudioFrame {
                        stream_id: AudioStreamId::Main,
                        format_id: format,
                        codec: format.codec().expect("PCM"),
                        sequence: 0,
                        timestamp: 0,
                        ssrc: 7,
                        nonce_counter: 0,
                        payload: vec![0; 480 * 2 * 2],
                        rtp_packet: vec![0; 12],
                    },
                )
                .expect("real PCM frame");
            let ready = Instant::now() + Duration::from_secs(1);
            while backend.telemetry().expect("telemetry").frames_pushed < 1 {
                assert!(Instant::now() < ready, "audio worker did not consume PCM");
                thread::sleep(Duration::from_millis(5));
            }
            let pipeline = backend
                .pipeline_control
                .lock()
                .expect("control")
                .clone()
                .expect("pipeline");
            let preamp = pipeline.by_name("usb-output-preamp").expect("preamp");
            assert!(
                (preamp.property::<f64>("volume") - crate::db_to_linear(-6.0)).abs() < 0.00001,
                "new backend inherits latest preamp"
            );
            let mix_filter = pipeline
                .by_name("float-mixer-output")
                .expect("float DSP caps");
            assert_eq!(
                mix_filter
                    .property::<gst::Caps>("caps")
                    .structure(0)
                    .expect("structure")
                    .get::<String>("format")
                    .expect("format"),
                "F32LE"
            );
            let dac_filter = pipeline.by_name("mixer-output-caps").expect("DAC caps");
            assert_eq!(
                dac_filter
                    .property::<gst::Caps>("caps")
                    .structure(0)
                    .expect("structure")
                    .get::<String>("format")
                    .expect("format"),
                "S16LE"
            );
            backend.set_muted(true).expect("mute");
            backend
                .set_ducking_gain_ramped(true, Some(0.25), Duration::from_millis(20))
                .expect("duck");
            for gain in -24..=24 {
                control
                    .set(crate::EqualizerSettings {
                        bands_half_db: [gain; 10],
                        ..saved
                    })
                    .expect("live adjustment");
            }
            control
                .set(crate::EqualizerSettings {
                    enabled: false,
                    ..saved
                })
                .expect("bypass");
            let deadline = Instant::now() + Duration::from_secs(1);
            while (preamp.property::<f64>("volume") - 1.0).abs() > 0.00001 {
                assert!(
                    Instant::now() < deadline,
                    "bypass failed to settle: volume={}, telemetry={:?}, error={:?}",
                    preamp.property::<f64>("volume"),
                    backend.telemetry(),
                    backend.try_error()
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(control.settings().bands_half_db, saved.bands_half_db);
            backend.set_muted(false).expect("unmute");
            control.set(saved).expect("reenable");
            let deadline = Instant::now() + Duration::from_secs(1);
            while (preamp.property::<f64>("volume") - crate::db_to_linear(-6.0)).abs() > 0.00001 {
                assert!(
                    Instant::now() < deadline,
                    "enable failed to settle: telemetry={:?}, error={:?}",
                    backend.telemetry(),
                    backend.try_error()
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                backend.pipeline_control.lock().expect("control").as_ref(),
                Some(&pipeline)
            );
            let telemetry = backend.telemetry().expect("telemetry");
            assert_eq!(telemetry.pipeline_start_events, 1, "reconnect {reconnect}");
            assert_eq!(telemetry.pipeline_stop_events, 0);
            assert_eq!(telemetry.last_failure, None);
            backend.shutdown().expect("shutdown");
        }
    }

    #[test]
    fn real_pcm_mixer_opens_and_writes_the_alsa_null_sink() {
        if let Err(error) = gst::init() {
            require_host_audio_or_skip(&format!("GStreamer runtime is unavailable: {error}"));
            return;
        }
        for factory in [
            "appsrc",
            "queue",
            "audiomixer",
            "audiotestsrc",
            "audioconvert",
            "audioresample",
            "capsfilter",
            "alsasink",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                require_host_audio_or_skip(&format!(
                    "required GStreamer factory {factory} is unavailable"
                ));
                return;
            }
        }
        probe_alsa_output("null").expect("ALSA null PCM preflight");
        prime_alsa_output("null", AudioLatencyConfig::default()).expect("ALSA null PCM prime");
        let format = AudioFormatId::new(0x8000);
        let plan = AudioMixerPlan::with_options(
            &[
                (AudioStreamId::Main, format),
                (AudioStreamId::Alternate, format),
            ],
            AudioMixPolicy::carplay(),
            AudioLatencyConfig::default(),
        )
        .expect("mixer plan");
        let mut pipeline = AudioGstPipeline::start(plan, "null").expect("live null mixer");
        assert_eq!(pipeline.pipeline.clock(), Some(gst::SystemClock::obtain()));
        let main_pad = pipeline
            .mixer_pads
            .get(&AudioStreamId::Main)
            .expect("Main mixer pad");
        let alternate_pad = pipeline
            .mixer_pads
            .get(&AudioStreamId::Alternate)
            .expect("Alternate mixer pad");
        assert!((main_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);
        assert!((alternate_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);
        pipeline
            .set_mix_state(3_277, false, true)
            .expect("duck MainAudio");
        assert!((main_pad.property::<f64>("volume") - gain_as_f64(3_277)).abs() < f64::EPSILON);
        assert!((alternate_pad.property::<f64>("volume") - 1.0).abs() < f64::EPSILON);
        pipeline
            .set_mix_state(UNITY_GAIN_Q15, false, false)
            .expect("restore MainAudio");
        let main_source = pipeline
            .sources
            .get(&AudioStreamId::Main)
            .expect("Main appsrc")
            .clone();
        assert!(main_source.property::<bool>("block"));
        assert!(!main_source.property::<bool>("do-timestamp"));
        let first_pcm_timing = Arc::new(Mutex::new(Vec::new()));
        let observed_timing = Arc::clone(&first_pcm_timing);
        main_source
            .static_pad("src")
            .expect("Main appsrc source pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
                    return gst::PadProbeReturn::Ok;
                };
                if let Ok(mut timing) = observed_timing.lock() {
                    timing.push((buffer.pts(), buffer.dts()));
                }
                gst::PadProbeReturn::Ok
            })
            .expect("PCM timing probe");
        for name in ["audio-input-queue-100", "mixer-input-queue-100"] {
            let queue = pipeline.pipeline.by_name(name).expect("Main audio queue");
            let value = queue.property_value("leaky");
            let leaky = value.get::<&glib::EnumValue>().expect("queue leaky enum");
            assert_eq!(leaky.nick(), "no");
        }
        pipeline.set_test_tone(true).expect("start test tone");
        let tone_deadline = Instant::now() + Duration::from_secs(1);
        let tone_stats = loop {
            let stats = pipeline
                .sink_stats()
                .expect("GStreamer BaseSink stats are available");
            if stats.rendered_buffers != 0 || Instant::now() >= tone_deadline {
                break stats;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(tone_stats.rendered_buffers >= 1);
        pipeline.check_bus().expect("test tone bus");
        pipeline.set_test_tone(false).expect("stop test tone");
        for index in 0..20_u16 {
            pipeline
                .push(EncryptedAudioFrame {
                    stream_id: AudioStreamId::Main,
                    format_id: format,
                    codec: format.codec().expect("PCM codec"),
                    sequence: index,
                    timestamp: u32::from(index) * 480,
                    ssrc: 7,
                    nonce_counter: u64::from(index),
                    payload: vec![0; 48_000 / 100 * 2 * 2],
                    rtp_packet: vec![0; 12],
                })
                .expect("write PCM frame");
        }
        let timing_deadline = Instant::now() + Duration::from_secs(1);
        let (first_pts, first_dts) = loop {
            if let Some(timing) = first_pcm_timing
                .lock()
                .expect("PCM timing")
                .first()
                .copied()
            {
                break timing;
            }
            assert!(
                Instant::now() < timing_deadline,
                "PCM buffer did not reach the appsrc output"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(first_pts.is_some());
        assert!(first_dts.is_none());
        assert!(pipeline.clock_origins.contains_key(&AudioStreamId::Main));
        let main_origin = pipeline.clock_origins[&AudioStreamId::Main];
        pipeline
            .configure_stream(AudioStreamConfig::new(
                AudioStreamId::Alternate,
                AudioFormatId::new(0x10),
            ))
            .expect("configure dormant navigation branch in place");
        assert_eq!(
            pipeline.clock_origins.get(&AudioStreamId::Main),
            Some(&main_origin)
        );
        assert_eq!(
            pipeline
                .plan
                .stream(AudioStreamId::Alternate)
                .expect("Alternate plan")
                .format_id,
            AudioFormatId::new(0x10)
        );
        let navigation_format = AudioFormatId::new(0x10);
        pipeline
            .push(EncryptedAudioFrame {
                stream_id: AudioStreamId::Alternate,
                format_id: navigation_format,
                codec: navigation_format.codec().expect("navigation PCM codec"),
                sequence: 0,
                timestamp: 0,
                ssrc: 8,
                nonce_counter: 0,
                payload: vec![0; 16_000 / 100 * 2],
                rtp_packet: vec![0; 12],
            })
            .expect("write navigation PCM frame");
        thread::sleep(Duration::from_millis(20));
        pipeline
            .deactivate_stream(AudioStreamId::Alternate)
            .expect("retire navigation branch");
        pipeline
            .configure_stream(AudioStreamConfig::new(AudioStreamId::Alternate, format))
            .expect("reuse navigation branch with renegotiated caps");
        pipeline
            .push(EncryptedAudioFrame {
                stream_id: AudioStreamId::Alternate,
                format_id: format,
                codec: format.codec().expect("renegotiated PCM codec"),
                sequence: 1,
                timestamp: 480,
                ssrc: 9,
                nonce_counter: 1,
                payload: vec![0; 48_000 / 100 * 2 * 2],
                rtp_packet: vec![0; 12],
            })
            .expect("write renegotiated navigation PCM frame");
        assert_eq!(
            pipeline.clock_origins.get(&AudioStreamId::Main),
            Some(&main_origin)
        );
        let previous_main_pts = first_pcm_timing
            .lock()
            .expect("Main PCM observations")
            .iter()
            .filter_map(|(pts, _)| *pts)
            .max()
            .expect("pre-navigation Main PTS");
        pipeline
            .push(EncryptedAudioFrame {
                stream_id: AudioStreamId::Main,
                format_id: format,
                codec: format.codec().expect("Main PCM codec"),
                sequence: 20,
                timestamp: 20 * 480,
                ssrc: 7,
                nonce_counter: 20,
                payload: vec![0; 48_000 / 100 * 2 * 2],
                rtp_packet: vec![0; 12],
            })
            .expect("write uninterrupted post-navigation Main frame");
        let continuity_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let advanced = first_pcm_timing
                .lock()
                .expect("Main PCM observations")
                .iter()
                .filter_map(|(pts, _)| *pts)
                .any(|pts| pts > previous_main_pts);
            if advanced {
                break;
            }
            assert!(
                Instant::now() < continuity_deadline,
                "Main PCM did not continue after navigation branch reuse"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let post_flush_observations = Arc::new(Mutex::new(Vec::new()));
        let observed_post_flush = Arc::clone(&post_flush_observations);
        main_source
            .static_pad("src")
            .expect("Main appsrc source pad")
            .add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::BUFFER,
                move |_, info| {
                    if let Ok(mut observations) = observed_post_flush.lock() {
                        match info.data.as_ref() {
                            Some(gst::PadProbeData::Event(event))
                                if event.type_() == gst::EventType::Segment =>
                            {
                                observations.push("segment");
                            }
                            Some(gst::PadProbeData::Buffer(_)) => observations.push("buffer"),
                            _ => {}
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            )
            .expect("post-flush ordering probe");
        let pipeline_control = new_pipeline_control();
        register_pipeline(&pipeline_control, &pipeline.pipeline).expect("register live mixer");
        begin_audio_pipeline_flush(&pipeline_control).expect("begin live mixer flush");
        post_flush_observations
            .lock()
            .expect("post-flush observations")
            .clear();
        pipeline.finish_flush().expect("finish live mixer flush");
        clear_pipeline(&pipeline_control).expect("clear live mixer registration");
        assert!(pipeline.clock_origins.is_empty());
        pipeline
            .push(EncryptedAudioFrame {
                stream_id: AudioStreamId::Main,
                format_id: format,
                codec: format.codec().expect("PCM codec"),
                sequence: 20,
                timestamp: 20 * 480,
                ssrc: 7,
                nonce_counter: 20,
                payload: vec![0; 48_000 / 100 * 2 * 2],
                rtp_packet: vec![0; 12],
            })
            .expect("write post-flush PCM frame");
        assert!(pipeline.clock_origins.contains_key(&AudioStreamId::Main));
        let post_flush_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if post_flush_observations
                .lock()
                .expect("post-flush observations")
                .contains(&"buffer")
            {
                break;
            }
            assert!(
                Instant::now() < post_flush_deadline,
                "post-flush Main buffer did not leave appsrc"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let observations = post_flush_observations
            .lock()
            .expect("post-flush observations");
        let segment = observations
            .iter()
            .position(|observation| *observation == "segment")
            .expect("post-flush segment");
        let buffer = observations
            .iter()
            .position(|observation| *observation == "buffer")
            .expect("post-flush buffer");
        assert!(segment < buffer);
        drop(observations);
        let deadline = Instant::now() + Duration::from_secs(1);
        let sink_stats = loop {
            let stats = pipeline
                .sink_stats()
                .expect("GStreamer BaseSink stats are available");
            if stats.rendered_buffers != 0 || Instant::now() >= deadline {
                break stats;
            }
            thread::sleep(Duration::from_millis(10));
        };
        pipeline.check_bus().expect("mixer bus");
        assert!(sink_stats.rendered_buffers >= 1);
        assert_eq!(sink_stats.dropped_buffers, 0);
        let latency = Mutex::new(AudioLatencySnapshot::unmeasured(
            AudioLatencyConfig::default(),
        ));
        pipeline.measure_latency(&latency).expect("latency query");
        let snapshot = *latency.lock().expect("latency snapshot");
        assert_eq!(snapshot.configured, AudioLatencyConfig::default());
        pipeline.stop().expect("stop mixer");
    }

    #[test]
    fn audio_telemetry_counters_saturate_and_latest_failure_wins() {
        let mut telemetry = AudioBackendTelemetry::default();
        telemetry.snapshot.pipeline_start_events = u64::MAX;
        telemetry.snapshot.pipeline_stop_events = u64::MAX;
        telemetry.snapshot.frames_pushed = u64::MAX;
        telemetry.snapshot.frame_push_failure_events = u64::MAX;
        telemetry.snapshot.bus_poll_events = u64::MAX;
        telemetry.snapshot.bus_failure_events = u64::MAX;
        telemetry.snapshot.endpoint_revalidation_events = u64::MAX;
        telemetry.snapshot.endpoint_revalidation_failure_events = u64::MAX;
        telemetry.snapshot.sink_stats_poll_events = u64::MAX;

        telemetry.pipeline_started();
        telemetry.frame_pushed();
        telemetry.frame_push_failed();
        telemetry.bus_polled();
        telemetry.bus_failed();
        telemetry.endpoint_revalidated(false);
        telemetry.sampled_sink(Some(AudioSinkStats {
            rendered_buffers: u64::MAX,
            dropped_buffers: u64::MAX,
        }));
        telemetry.pipeline_stopped();

        assert_eq!(telemetry.snapshot.pipeline_start_events, u64::MAX);
        assert_eq!(telemetry.snapshot.pipeline_stop_events, u64::MAX);
        assert_eq!(telemetry.snapshot.frames_pushed, u64::MAX);
        assert_eq!(telemetry.snapshot.frame_push_failure_events, u64::MAX);
        assert_eq!(telemetry.snapshot.bus_poll_events, u64::MAX);
        assert_eq!(telemetry.snapshot.bus_failure_events, u64::MAX);
        assert_eq!(telemetry.snapshot.endpoint_revalidation_events, u64::MAX);
        assert_eq!(
            telemetry.snapshot.endpoint_revalidation_failure_events,
            u64::MAX
        );
        assert_eq!(telemetry.snapshot.sink_stats_poll_events, u64::MAX);
        assert_eq!(
            telemetry.snapshot.last_failure,
            Some(AudioBackendFailureKind::EndpointRevalidation)
        );
        assert_eq!(
            telemetry.snapshot.sink_stats,
            Some(AudioSinkStats {
                rendered_buffers: u64::MAX,
                dropped_buffers: u64::MAX,
            })
        );
    }

    #[test]
    fn non_bt709_h264_colorimetry_is_removed_only_for_eight_bit_420() {
        if gst::init().is_err() {
            skip("GStreamer runtime is unavailable");
            return;
        }
        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .field("bit-depth-luma", 8_u32)
            .field("bit-depth-chroma", 8_u32)
            .field("chroma-format", "4:2:0")
            .field("profile", "high-10")
            .field("colorimetry", "1:4:5:1")
            .build();
        let adapted = h264_caps_without_unsupported_colorimetry(&caps)
            .expect("eligible H.264 caps should be adapted");
        let structure = adapted.structure(0).expect("adapted caps structure");
        assert!(!structure.has_field("colorimetry"));
        assert_eq!(structure.get::<&str>("profile"), Ok("high-10"));

        for field in ["bit-depth-luma", "bit-depth-chroma"] {
            let mut incompatible = caps.clone();
            incompatible
                .make_mut()
                .structure_mut(0)
                .expect("incompatible caps contain one structure")
                .set(field, 10_u32);
            assert!(h264_caps_without_unsupported_colorimetry(&incompatible).is_none());
        }

        let mut supported = caps.clone();
        supported
            .make_mut()
            .structure_mut(0)
            .expect("supported caps contain one structure")
            .set("colorimetry", "bt709");
        assert!(h264_caps_without_unsupported_colorimetry(&supported).is_none());

        for colorimetry in ["bt601", "2:4:5:4", "1:3:5:1", "1:4:7:1"] {
            let mut android = caps.clone();
            android
                .make_mut()
                .structure_mut(0)
                .expect("android caps contain one structure")
                .set("colorimetry", colorimetry);
            let adapted = h264_caps_without_unsupported_colorimetry(&android)
                .expect("non-bt709 colorimetry should be removed");
            assert!(
                !adapted
                    .structure(0)
                    .expect("adapted caps")
                    .has_field("colorimetry")
            );
        }
    }
}
