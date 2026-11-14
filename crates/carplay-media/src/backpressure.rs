use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
use std::time::Instant;

use crate::{
    EncryptedVideoFrame, Error, MAXIMUM_BACKEND_FRAME_BYTES, Result, VideoCodecConfiguration,
    VideoPipelinePlan, WireVideoCodec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoBackpressureConfig {
    pub capacity: usize,
    pub maximum_frame_bytes: usize,
}

impl Default for VideoBackpressureConfig {
    fn default() -> Self {
        Self {
            capacity: 8,
            maximum_frame_bytes: MAXIMUM_BACKEND_FRAME_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFrameDisposition {
    Queued,
    ReplacedStaleFrame,
    DroppedAwaitingKeyframe,
    DroppedQueueFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoBackpressureStats {
    pub configurations: u64,
    pub queued_frames: u64,
    pub replaced_frames: u64,
    pub dropped_frames: u64,
    pub live_depth: u64,
}

pub struct VideoIngressBuffer {
    shared: Arc<Shared>,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupSampleDisposition {
    Suppress,
    SuppressAndRequestKeyFrame,
    Render,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
#[derive(Debug, Default)]
pub(crate) struct VideoStartupRenderGate {
    first_idr_seen: bool,
    render_from_pts_ns: Option<u64>,
    keyframe_requested: bool,
    opened: bool,
    last_output_pts_ns: Option<u64>,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
impl VideoStartupRenderGate {
    pub(crate) fn register_input(&mut self, idr: bool, pts_ns: u64) {
        if idr {
            if self.first_idr_seen {
                self.render_from_pts_ns.get_or_insert(pts_ns);
            } else {
                self.first_idr_seen = true;
            }
        }
    }

    pub(crate) fn observe_output(
        &mut self,
        pts_ns: Option<u64>,
    ) -> Result<StartupSampleDisposition> {
        let pts_ns = pts_ns.ok_or_else(|| {
            Error::Backend("hardware-decoded startup sample has no presentation time".to_owned())
        })?;
        if self
            .last_output_pts_ns
            .is_some_and(|previous| pts_ns < previous)
        {
            return Err(Error::Backend(
                "hardware-decoded startup samples are not presentation-time ordered".to_owned(),
            ));
        }
        self.last_output_pts_ns = Some(pts_ns);
        if self.opened {
            return Ok(StartupSampleDisposition::Render);
        }
        let Some(render_from) = self.render_from_pts_ns else {
            if self.keyframe_requested {
                return Ok(StartupSampleDisposition::Suppress);
            }
            self.keyframe_requested = true;
            return Ok(StartupSampleDisposition::SuppressAndRequestKeyFrame);
        };
        if pts_ns < render_from {
            return Ok(StartupSampleDisposition::Suppress);
        }
        self.opened = true;
        Ok(StartupSampleDisposition::Render)
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.opened
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
#[derive(Debug, Default)]
pub(crate) struct DecodeStallWatchdog {
    undisplayed_frames: u32,
    undisplayed_since: Option<Instant>,
    last_request: Option<Instant>,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
impl DecodeStallWatchdog {
    const STALL_FRAMES: u32 = 6;
    const STALL_TIMEOUT: Duration = Duration::from_millis(500);
    const RETRY: Duration = Duration::from_secs(1);

    pub(crate) fn record_input(&mut self, now: Instant) {
        self.undisplayed_frames = self.undisplayed_frames.saturating_add(1);
        self.undisplayed_since.get_or_insert(now);
    }

    pub(crate) fn record_render(&mut self) {
        self.undisplayed_frames = 0;
        self.undisplayed_since = None;
        self.last_request = None;
    }

    pub(crate) fn keyframe_due(&mut self, now: Instant) -> bool {
        let stalled = self.undisplayed_frames >= Self::STALL_FRAMES
            && self
                .undisplayed_since
                .is_some_and(|since| now.saturating_duration_since(since) >= Self::STALL_TIMEOUT);
        let retry_allowed = self
            .last_request
            .is_none_or(|last| now.saturating_duration_since(last) >= Self::RETRY);
        if stalled && retry_allowed {
            self.last_request = Some(now);
            return true;
        }
        false
    }
}

impl fmt::Debug for VideoIngressBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VideoIngressBuffer")
            .field("config", &self.shared.config)
            .field("contents", &"<redacted-media>")
            .finish()
    }
}

impl Clone for VideoIngressBuffer {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Debug)]
struct Shared {
    config: VideoBackpressureConfig,
    state: Mutex<State>,
    available: Condvar,
}

struct State {
    commands: VecDeque<VideoCommand>,
    configuration: Option<VideoCodecConfiguration>,
    awaiting_keyframe: bool,
    keyframe_requested: bool,
    closed: bool,
    stats: VideoBackpressureStats,
}

impl fmt::Debug for State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("State")
            .field("queued_commands", &self.commands.len())
            .field("configured", &self.configuration.is_some())
            .field("awaiting_keyframe", &self.awaiting_keyframe)
            .field("keyframe_requested", &self.keyframe_requested)
            .field("closed", &self.closed)
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-backend")),
    allow(dead_code)
)]
pub(crate) enum VideoCommand {
    Configuration(VideoCodecConfiguration),
    Frame(EncryptedVideoFrame),
    Stop,
}

impl VideoIngressBuffer {
    pub fn new(config: VideoBackpressureConfig) -> Result<Self> {
        if config.capacity < 2
            || config.maximum_frame_bytes == 0
            || config.maximum_frame_bytes > MAXIMUM_BACKEND_FRAME_BYTES
        {
            return Err(Error::InvalidVideo(
                "video ingress capacity or frame limit is invalid".to_owned(),
            ));
        }
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                state: Mutex::new(State {
                    commands: VecDeque::with_capacity(config.capacity),
                    configuration: None,
                    awaiting_keyframe: true,
                    keyframe_requested: false,
                    closed: false,
                    stats: VideoBackpressureStats::default(),
                }),
                available: Condvar::new(),
            }),
        })
    }

    pub fn submit_configuration(&self, configuration: VideoCodecConfiguration) -> Result<()> {
        VideoPipelinePlan::h264(&configuration)?;
        let mut state = self.lock()?;
        require_open(&state)?;
        if state.configuration.as_ref() == Some(&configuration) {
            return Ok(());
        }
        state.commands.clear();
        state.configuration = Some(configuration.clone());
        state.awaiting_keyframe = true;
        state.stats.configurations = state.stats.configurations.saturating_add(1);
        state
            .commands
            .push_back(VideoCommand::Configuration(configuration));
        drop(state);
        self.shared.available.notify_one();
        Ok(())
    }

    pub fn submit_frame(&self, frame: EncryptedVideoFrame) -> Result<VideoFrameDisposition> {
        if frame.data.is_empty() || frame.data.len() > self.shared.config.maximum_frame_bytes {
            return Err(Error::InvalidVideo(
                "video frame is empty or exceeds the backend bound".to_owned(),
            ));
        }
        if frame.codec != WireVideoCodec::H264 {
            return Err(Error::Unsupported(
                "the target media backend currently supports H.264 only",
            ));
        }
        let keyframe = frame
            .nal_units
            .iter()
            .any(|descriptor| descriptor.unit_type == 5);
        let mut state = self.lock()?;
        require_open(&state)?;
        let Some(configuration) = state.configuration.as_ref() else {
            return Err(Error::NotReady {
                component: "video ingress",
                state: "unconfigured",
            });
        };
        if configuration.codec != frame.codec {
            return Err(Error::InvalidVideo(
                "video frame codec does not match active configuration".to_owned(),
            ));
        }
        if state.awaiting_keyframe && !keyframe {
            state.stats.dropped_frames = state.stats.dropped_frames.saturating_add(1);
            return Ok(VideoFrameDisposition::DroppedAwaitingKeyframe);
        }

        let disposition = if keyframe {
            let old_frames = remove_all_frames(&mut state.commands);
            state.stats.replaced_frames = state
                .stats
                .replaced_frames
                .saturating_add(old_frames as u64);
            state.awaiting_keyframe = false;
            if old_frames == 0 {
                VideoFrameDisposition::Queued
            } else {
                VideoFrameDisposition::ReplacedStaleFrame
            }
        } else if state.commands.len() >= self.shared.config.capacity {
            state.awaiting_keyframe = true;
            state.keyframe_requested = true;
            state.stats.dropped_frames = state.stats.dropped_frames.saturating_add(1);
            return Ok(VideoFrameDisposition::DroppedQueueFull);
        } else {
            VideoFrameDisposition::Queued
        };
        state.stats.queued_frames = state.stats.queued_frames.saturating_add(1);
        state.commands.push_back(VideoCommand::Frame(frame));
        drop(state);
        self.shared.available.notify_one();
        Ok(disposition)
    }

    pub fn close(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        state.commands.clear();
        state.commands.push_back(VideoCommand::Stop);
        drop(state);
        self.shared.available.notify_all();
        Ok(())
    }

    pub fn stats(&self) -> Result<VideoBackpressureStats> {
        let state = self.lock()?;
        let mut stats = state.stats;
        stats.live_depth = state
            .commands
            .iter()
            .filter(|command| matches!(command, VideoCommand::Frame(_)))
            .count() as u64;
        Ok(stats)
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "linux-backend")),
        allow(dead_code)
    )]
    pub(crate) fn take_keyframe_request(&self) -> Result<bool> {
        Ok(std::mem::take(&mut self.lock()?.keyframe_requested))
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "linux-backend")),
        allow(dead_code)
    )]
    pub(crate) fn pop_timeout(&self, timeout: Duration) -> Result<Option<VideoCommand>> {
        let mut state = self.lock()?;
        if state.commands.is_empty() {
            let (next, _) = self
                .shared
                .available
                .wait_timeout(state, timeout)
                .map_err(|_| Error::Backend("video ingress lock is poisoned".to_owned()))?;
            state = next;
        }
        Ok(state.commands.pop_front())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.shared
            .state
            .lock()
            .map_err(|_| Error::Backend("video ingress lock is poisoned".to_owned()))
    }
}

fn require_open(state: &State) -> Result<()> {
    if state.closed {
        return Err(Error::NotReady {
            component: "video ingress",
            state: "closed",
        });
    }
    Ok(())
}

fn remove_all_frames(commands: &mut VecDeque<VideoCommand>) -> usize {
    let before = commands.len();
    commands.retain(|command| !matches!(command, VideoCommand::Frame(_)));
    before - commands.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NalUnitDescriptor;

    fn configuration() -> VideoCodecConfiguration {
        VideoCodecConfiguration {
            codec: WireVideoCodec::H264,
            codec_data: vec![1, 100, 0, 31, 0xff, 0xe1, 0, 1, 0x67],
            nal_length_bytes: 4,
        }
    }

    fn frame(unit_type: u8, marker: u8) -> EncryptedVideoFrame {
        EncryptedVideoFrame {
            data: vec![0, 0, 0, 1, marker],
            codec: WireVideoCodec::H264,
            nal_units: vec![NalUnitDescriptor {
                payload_offset: 4,
                payload_length: 1,
                unit_type,
            }],
            presentation_time_ns: None,
            nonce_counter: u64::from(marker),
        }
    }

    #[test]
    fn decode_stall_requests_keyframes_until_a_frame_renders() {
        let start = Instant::now();
        let mut watchdog = DecodeStallWatchdog::default();
        for _ in 0..DecodeStallWatchdog::STALL_FRAMES {
            watchdog.record_input(start);
        }
        assert!(!watchdog.keyframe_due(start + Duration::from_millis(499)));
        assert!(watchdog.keyframe_due(start + DecodeStallWatchdog::STALL_TIMEOUT));
        assert!(!watchdog.keyframe_due(start + Duration::from_millis(1_499)));
        assert!(
            watchdog.keyframe_due(
                start + DecodeStallWatchdog::STALL_TIMEOUT + DecodeStallWatchdog::RETRY
            )
        );
        watchdog.record_render();
        assert!(!watchdog.keyframe_due(start + Duration::from_secs(10)));
    }

    #[test]
    fn decode_stall_ignores_a_few_buffered_frames_on_a_static_screen() {
        let start = Instant::now();
        let mut watchdog = DecodeStallWatchdog::default();
        for _ in 1..DecodeStallWatchdog::STALL_FRAMES {
            watchdog.record_input(start);
        }
        assert!(!watchdog.keyframe_due(start + Duration::from_secs(60)));
    }

    #[test]
    fn deltas_are_dropped_until_keyframe_after_configuration() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig::default()).expect("queue");
        queue.submit_configuration(configuration()).expect("config");
        assert_eq!(
            queue.submit_frame(frame(1, 1)).expect("delta"),
            VideoFrameDisposition::DroppedAwaitingKeyframe
        );
        assert_eq!(
            queue.submit_frame(frame(5, 2)).expect("key"),
            VideoFrameDisposition::Queued
        );
    }

    #[test]
    fn newest_keyframe_replaces_stale_reference_chain() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig {
            capacity: 4,
            ..VideoBackpressureConfig::default()
        })
        .expect("queue");
        queue.submit_configuration(configuration()).expect("config");
        queue.submit_frame(frame(5, 1)).expect("key");
        queue.submit_frame(frame(1, 2)).expect("delta");
        assert_eq!(
            queue.submit_frame(frame(5, 3)).expect("new key"),
            VideoFrameDisposition::ReplacedStaleFrame
        );
        let stats = queue.stats().expect("stats");
        assert_eq!(stats.replaced_frames, 2);
        assert_eq!(stats.live_depth, 1);
    }

    #[test]
    fn live_depth_counts_buffered_frames_not_configuration() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig {
            capacity: 4,
            ..VideoBackpressureConfig::default()
        })
        .expect("queue");
        queue.submit_configuration(configuration()).expect("config");
        assert_eq!(queue.stats().expect("stats").live_depth, 0);
        queue.submit_frame(frame(5, 1)).expect("key");
        queue.submit_frame(frame(1, 2)).expect("delta");
        assert_eq!(queue.stats().expect("stats").live_depth, 2);
        let _ = queue.pop_timeout(Duration::ZERO).expect("pop config");
        assert_eq!(queue.stats().expect("stats").live_depth, 2);
        let _ = queue.pop_timeout(Duration::ZERO).expect("pop frame");
        assert_eq!(queue.stats().expect("stats").live_depth, 1);
    }

    #[test]
    fn full_queue_drops_deltas_until_the_requested_keyframe() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig {
            capacity: 3,
            ..VideoBackpressureConfig::default()
        })
        .expect("queue");
        queue.submit_configuration(configuration()).expect("config");
        queue.submit_frame(frame(5, 1)).expect("key");
        queue.submit_frame(frame(1, 2)).expect("delta");
        assert!(!queue.take_keyframe_request().expect("no request yet"));
        assert_eq!(
            queue.submit_frame(frame(1, 3)).expect("overflow"),
            VideoFrameDisposition::DroppedQueueFull
        );
        assert!(queue.take_keyframe_request().expect("request"));
        assert!(!queue.take_keyframe_request().expect("request consumed"));
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO).expect("pop config"),
            Some(VideoCommand::Configuration(_))
        ));
        assert_eq!(
            queue.submit_frame(frame(1, 4)).expect("delta after gap"),
            VideoFrameDisposition::DroppedAwaitingKeyframe
        );
        assert_eq!(
            queue.submit_frame(frame(5, 5)).expect("recovery key"),
            VideoFrameDisposition::ReplacedStaleFrame
        );
        let stats = queue.stats().expect("stats");
        assert_eq!(stats.dropped_frames, 2);
        assert_eq!(stats.replaced_frames, 2);
    }

    #[test]
    fn reconfiguration_discards_old_frames_and_requires_new_keyframe() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig::default()).expect("queue");
        queue.submit_configuration(configuration()).expect("config");
        queue.submit_frame(frame(5, 1)).expect("key");
        let mut changed = configuration();
        changed.codec_data[1] = 77;
        queue.submit_configuration(changed).expect("reconfigure");
        assert_eq!(
            queue.submit_frame(frame(1, 2)).expect("delta"),
            VideoFrameDisposition::DroppedAwaitingKeyframe
        );
    }

    #[test]
    fn identical_configuration_does_not_reset_the_active_reference_chain() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig::default()).expect("queue");
        let active = configuration();
        queue
            .submit_configuration(active.clone())
            .expect("configuration");
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO).expect("pop config"),
            Some(VideoCommand::Configuration(_))
        ));
        assert_eq!(
            queue.submit_frame(frame(5, 1)).expect("keyframe"),
            VideoFrameDisposition::Queued
        );
        queue
            .submit_configuration(active)
            .expect("duplicate configuration");
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO).expect("pop frame"),
            Some(VideoCommand::Frame(_))
        ));
        assert!(
            queue
                .pop_timeout(Duration::ZERO)
                .expect("no reconfiguration")
                .is_none()
        );
        assert_eq!(queue.stats().expect("stats").configurations, 1);
    }

    #[test]
    fn close_is_idempotent_and_rejects_more_media() {
        let queue = VideoIngressBuffer::new(VideoBackpressureConfig::default()).expect("queue");
        queue.close().expect("close");
        queue.close().expect("close twice");
        assert!(matches!(
            queue.submit_configuration(configuration()),
            Err(Error::NotReady {
                state: "closed",
                ..
            })
        ));
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO).expect("pop"),
            Some(VideoCommand::Stop)
        ));
    }

    #[test]
    fn startup_gate_warms_first_gop_and_opens_on_second_idr_output() {
        let mut gate = VideoStartupRenderGate::default();
        gate.register_input(true, 0);
        assert_eq!(
            gate.observe_output(Some(0)).expect("first output"),
            StartupSampleDisposition::SuppressAndRequestKeyFrame
        );
        gate.register_input(false, 33);
        gate.register_input(true, 66);
        assert_eq!(
            gate.observe_output(Some(33)).expect("old delta"),
            StartupSampleDisposition::Suppress
        );
        assert_eq!(
            gate.observe_output(Some(66)).expect("recovery IDR"),
            StartupSampleDisposition::Render
        );
        assert!(gate.is_open());
        assert_eq!(
            gate.observe_output(Some(99)).expect("clean delta"),
            StartupSampleDisposition::Render
        );
    }

    #[test]
    fn startup_gate_fails_closed_without_ordered_timestamps() {
        let mut missing = VideoStartupRenderGate::default();
        missing.register_input(true, 0);
        assert!(matches!(
            missing.observe_output(None),
            Err(Error::Backend(_))
        ));

        let mut reversed = VideoStartupRenderGate::default();
        reversed.register_input(true, 0);
        reversed.observe_output(Some(10)).expect("first timestamp");
        assert!(matches!(
            reversed.observe_output(Some(9)),
            Err(Error::Backend(_))
        ));
    }

    #[test]
    fn startup_gate_never_opens_on_the_first_idr() {
        let mut gate = VideoStartupRenderGate::default();
        gate.register_input(true, 0);
        assert_eq!(
            gate.observe_output(Some(0)).expect("first output"),
            StartupSampleDisposition::SuppressAndRequestKeyFrame
        );
        assert_eq!(
            gate.observe_output(Some(33)).expect("first-GOP delta"),
            StartupSampleDisposition::Suppress
        );
        assert!(!gate.is_open());
    }

    #[test]
    fn startup_gate_requests_recovery_after_first_decoded_sample() {
        let mut gate = VideoStartupRenderGate::default();
        gate.register_input(true, 0);
        gate.register_input(false, 1);
        gate.register_input(false, 100);
        assert_eq!(
            gate.observe_output(Some(0)).expect("late first output"),
            StartupSampleDisposition::SuppressAndRequestKeyFrame
        );
        assert_eq!(
            gate.observe_output(Some(1)).expect("subsequent output"),
            StartupSampleDisposition::Suppress
        );
    }
}
